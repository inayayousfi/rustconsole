use crate::capture::{DesktopDuplicationCapture, DesktopDuplicationError};
use crate::capture_recovery::{
    CaptureAction, CaptureFailure, acquisition_action, retry_initialization,
};
use crate::display_mode_proof::{DisplayModePhase, DisplayModeProgress, DisplayModeSnapshot};
use crate::gpu_encode::{GpuAv1SnapshotEncoder, PreparedGpuCapture, VideoReconfigurationRequired};
use crate::transition_proof::{TransitionGoal, TransitionProgress};
use crate::worker_protocol::{AudioWorkerEvent, read_audio_event};
use crate::worker_protocol::{
    VERSION, WorkerCommand, WorkerEvent, WorkerIdentity, read_command, read_event, write_command,
    write_event,
};
use rustconsole_host_core::audio_transport::{AUDIO_QUEUE_PACKETS, MediaQueue};
use rustconsole_host_core::{DesktopCapture, LatestQueue};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, E_ACCESSDENIED, ERROR_NO_TOKEN, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE,
    HANDLE_FLAG_INHERIT, HANDLE_FLAGS, LUID, SetHandleInformation,
};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_INVALID_CALL;
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, IsWellKnownSid,
    LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, SE_TCB_NAME,
    SecurityImpersonation, SetTokenInformation, TOKEN_ADJUST_PRIVILEGES, TOKEN_ALL_ACCESS,
    TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenPrimary, TokenPrivileges,
    TokenSessionId, TokenUser, WinLocalSystemSid,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessAsUserW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentProcessId,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread, STARTUPINFOEXW,
    UpdateProcThreadAttribute,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

const ACTIVE_SESSION_NONE: u32 = u32::MAX;
const CAPTURE_LIMIT: Duration = Duration::from_secs(10);
const DESKTOP_TRANSITION_LIMIT: Duration = Duration::from_secs(120);
const LOGIN_TRANSITION_LIMIT: Duration = Duration::from_secs(300);
const DISPLAY_MODE_TRANSITION_LIMIT: Duration = Duration::from_secs(180);
const REINITIALIZATION_DELAY: Duration = Duration::from_millis(200);
const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(250);
const FRAME_TARGET: u64 = 3;

#[must_use]
pub fn active_console_session_available() -> bool {
    // SAFETY: this query has no arguments and returns the current session identifier.
    (unsafe { WTSGetActiveConsoleSessionId() }) != ACTIVE_SESSION_NONE
}

pub struct MediaWorker {
    command: File,
    events: Option<File>,
    audio_events: Option<File>,
    _job: Option<OwnedHandle>,
    _process: OwnedHandle,
    process_id: u32,
    session_id: u32,
    identity: WorkerIdentity,
    connection_token: [u8; 16],
    hello_accepted: bool,
}

pub enum VideoPreparation {
    Interrupted,
    Ready(crate::worker_protocol::WorkerVideoConfiguration),
    SystemIdentityRequired,
}

pub struct MediaWorkerStream {
    worker: MediaWorker,
    events: Arc<LatestQueue<io::Result<WorkerEvent>>>,
    dropped_events: Arc<AtomicU64>,
    reader: Option<thread::JoinHandle<()>>,
    audio: Arc<MediaQueue<io::Result<AudioWorkerEvent>>>,
    audio_dropped: Arc<AtomicU64>,
    audio_reader: Option<thread::JoinHandle<()>>,
}

impl MediaWorker {
    pub fn launch_prepared_video(
        executable: &Path,
        shutdown_rx: &Receiver<()>,
    ) -> Result<
        Option<(Self, crate::worker_protocol::WorkerVideoConfiguration)>,
        Box<dyn std::error::Error>,
    > {
        let mut worker = Self::launch(executable)?;
        match worker.prepare_video_stream(shutdown_rx)? {
            VideoPreparation::Ready(configuration) => Ok(Some((worker, configuration))),
            VideoPreparation::Interrupted => Ok(None),
            VideoPreparation::SystemIdentityRequired => {
                Err("LOCAL_SYSTEM media worker requested its own identity".into())
            }
        }
    }

    pub fn audio_proof(
        &mut self,
        shutdown_rx: &Receiver<()>,
        encode_opus: bool,
    ) -> Result<(bool, String), Box<dyn std::error::Error>> {
        self.run_proof(
            if encode_opus {
                WorkerCommand::AudioEncodeProof
            } else {
                WorkerCommand::AudioProof
            },
            shutdown_rx,
        )
    }

    pub fn stop_and_wait(
        &mut self,
        stop_already_sent: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        if !stop_already_sent {
            self.stop()?;
        }
        // SAFETY: this process handle belongs to the supervised worker.
        unsafe {
            if WaitForSingleObject(self._process.get(), 5_000) != WAIT_OBJECT_0 {
                return Err("audio worker did not exit within five seconds".into());
            }
            let mut code = 0;
            GetExitCodeProcess(self._process.get(), &mut code)?;
            if code != 0 {
                return Err(format!("audio worker exited with code {code}").into());
            }
        }
        Ok(())
    }

    pub fn launch(executable: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        Self::launch_system(executable)
    }

    pub fn launch_active_user(executable: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // SAFETY: this query has no arguments and returns the current session identifier.
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        if session_id == ACTIVE_SESSION_NONE {
            return Err("Windows has no active console session".into());
        }
        let token = active_user_token(session_id)?;
        let connection = crate::interactive_worker::launch(executable, session_id, token.get())?;
        Ok(Self {
            command: connection.command,
            events: Some(connection.events),
            audio_events: Some(connection.audio_events),
            _job: None,
            _process: OwnedHandle::new(connection.process)?,
            process_id: connection.process_id,
            session_id,
            identity: WorkerIdentity::ActiveUser,
            connection_token: connection.connection_token,
            hello_accepted: false,
        })
    }

    fn launch_system(executable: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // SAFETY: this query has no arguments and returns the current session identifier.
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        if session_id == ACTIVE_SESSION_NONE {
            return Err("Windows has no active console session".into());
        }

        let (worker_commands, service_commands) = inheritable_pipe()?;
        let (service_events, worker_events) = inheritable_pipe()?;
        let (service_audio, worker_audio) = inheritable_pipe()?;
        set_not_inheritable(&service_commands)?;
        set_not_inheritable(&service_events)?;
        set_not_inheritable(&service_audio)?;

        let token = session_system_token(session_id)?;
        let job = kill_on_close_job()?;
        let child_handles = [
            raw_handle(&worker_commands),
            raw_handle(&worker_events),
            raw_handle(&worker_audio),
        ];
        let mut attributes = ProcessAttributes::new(&child_handles)?;
        let mut desktop = wide("winsta0\\default");
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = u32::try_from(std::mem::size_of::<STARTUPINFOEXW>())?;
        startup.StartupInfo.lpDesktop = PWSTR(desktop.as_mut_ptr());
        startup.lpAttributeList = attributes.as_ptr();

        let command_line = format!(
            "\"{}\" media-worker {} {} {}",
            executable.display(),
            raw_handle(&worker_commands).0 as usize,
            raw_handle(&worker_events).0 as usize,
            raw_handle(&worker_audio).0 as usize,
        );
        let mut command_line = wide(&command_line);
        let executable_wide = wide(executable.as_os_str());
        let current_directory = executable
            .parent()
            .ok_or("worker executable has no parent")?;
        let current_directory_wide = wide(current_directory.as_os_str());
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: token, attribute list, mutable command line, startup data, and
        // process output remain valid for the complete call.
        unsafe {
            CreateProcessAsUserW(
                Some(token.get()),
                PCWSTR(executable_wide.as_ptr()),
                Some(PWSTR(command_line.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                None,
                PCWSTR(current_directory_wide.as_ptr()),
                &startup.StartupInfo,
                &mut process,
            )
            .map_err(|error| windows_error("CreateProcessAsUserW", error))?;
        }
        let process_handle = OwnedHandle::new(process.hProcess)?;
        let thread_handle = OwnedHandle::new(process.hThread)?;
        // SAFETY: both handles were returned for the suspended child.
        unsafe {
            AssignProcessToJobObject(job.get(), process_handle.get())?;
            if ResumeThread(thread_handle.get()) == u32::MAX {
                return Err(windows::core::Error::from_thread().into());
            }
        }
        drop(attributes);
        drop(worker_commands);
        drop(worker_events);
        drop(worker_audio);

        Ok(Self {
            command: service_commands,
            events: Some(service_events),
            audio_events: Some(service_audio),
            _job: Some(job),
            _process: process_handle,
            process_id: process.dwProcessId,
            session_id,
            identity: WorkerIdentity::LocalSystem,
            connection_token: [0; 16],
            hello_accepted: false,
        })
    }

    pub fn capture_proof(
        &mut self,
        shutdown_rx: &Receiver<()>,
    ) -> Result<(bool, String), Box<dyn std::error::Error>> {
        self.run_proof(WorkerCommand::CaptureProof, shutdown_rx)
    }

    pub fn desktop_transition_proof(
        &mut self,
        shutdown_rx: &Receiver<()>,
    ) -> Result<(bool, String), Box<dyn std::error::Error>> {
        self.run_proof(WorkerCommand::DesktopTransitionProof, shutdown_rx)
    }

    pub fn login_transition_proof(
        &mut self,
        shutdown_rx: &Receiver<()>,
    ) -> Result<(bool, String), Box<dyn std::error::Error>> {
        self.run_proof(WorkerCommand::LoginTransitionProof, shutdown_rx)
    }

    pub fn display_mode_transition_proof(
        &mut self,
        shutdown_rx: &Receiver<()>,
        report_path: &Path,
    ) -> Result<(bool, String), Box<dyn std::error::Error>> {
        let mut events = self.events.take().ok_or("worker events already consumed")?;
        let (event_tx, event_rx) = sync_channel(2);
        let reader = thread::spawn(move || {
            for _ in 0..4 {
                if event_tx.send(read_event(&mut events)).is_err() {
                    break;
                }
            }
        });

        let hello = wait_event(&event_rx, shutdown_rx, &mut self.command)
            .map_err(|error| self.worker_error("proof hello", error.as_ref()))?;
        if !self.accept_hello(hello)? {
            return Ok((true, "status=interrupted\n".to_owned()));
        }
        write_command(&mut self.command, WorkerCommand::DisplayModeTransitionProof)?;

        let report = loop {
            match wait_event(&event_rx, shutdown_rx, &mut self.command)? {
                Some(WorkerEvent::ProofProgress(report)) => {
                    std::fs::write(report_path, self.report_with_identity(&report))?;
                }
                Some(WorkerEvent::CaptureReport(report)) => break report,
                Some(event) => {
                    return Err(format!("unexpected media worker event: {event:?}").into());
                }
                None => return Ok((true, "status=interrupted\n".to_owned())),
            }
        };
        reader
            .join()
            .map_err(|_| "media worker event reader panicked")?;
        Ok((false, self.report_with_identity(&report)))
    }

    pub fn encode_snapshot(
        &mut self,
        shutdown_rx: &Receiver<()>,
        frames_per_second: u16,
        bitrate_bits_per_second: u64,
    ) -> Result<Option<WorkerEvent>, Box<dyn std::error::Error>> {
        let mut events = self.events.take().ok_or("worker events already consumed")?;
        let (event_tx, event_rx) = sync_channel(2);
        let event_count = if self.hello_accepted { 1 } else { 2 };
        let reader = thread::spawn(move || {
            for _ in 0..event_count {
                if event_tx.send(read_event(&mut events)).is_err() {
                    break;
                }
            }
        });
        if !self.hello_accepted {
            let hello = wait_event(&event_rx, shutdown_rx, &mut self.command)?;
            if !self.accept_hello(hello)? {
                return Ok(None);
            }
            self.hello_accepted = true;
        }
        write_command(
            &mut self.command,
            WorkerCommand::EncodeSnapshot {
                frames_per_second,
                bitrate_bits_per_second,
            },
        )?;
        let event = wait_event(&event_rx, shutdown_rx, &mut self.command)?;
        reader
            .join()
            .map_err(|_| "media worker event reader panicked")?;
        Ok(event)
    }

    pub fn start_video_stream(
        mut self,
        shutdown_rx: &Receiver<()>,
        frames_per_second: u16,
        bitrate_bits_per_second: u64,
        enable_audio: bool,
    ) -> Result<Option<MediaWorkerStream>, Box<dyn std::error::Error>> {
        let mut event_pipe = self.events.take().ok_or("worker events already consumed")?;
        let events = Arc::new(LatestQueue::new(2));
        let dropped_events = Arc::new(AtomicU64::new(0));
        let reader_events = Arc::clone(&events);
        let reader_dropped = Arc::clone(&dropped_events);
        let reader = thread::spawn(move || {
            loop {
                let event = read_event(&mut event_pipe);
                let closed = event.is_err();
                if reader_events.push(event).is_some() {
                    reader_dropped.fetch_add(1, Ordering::Relaxed);
                }
                if closed {
                    break;
                }
            }
        });

        if !self.hello_accepted {
            let hello = wait_latest_event(&events, shutdown_rx, &mut self.command)?;
            if !self.accept_hello(hello)? {
                let _ = write_command(&mut self.command, WorkerCommand::Stop);
                let _ = reader.join();
                return Ok(None);
            }
        }
        let mut audio_pipe = self
            .audio_events
            .take()
            .ok_or("audio events already consumed")?;
        let audio = Arc::new(MediaQueue::new(AUDIO_QUEUE_PACKETS));
        let audio_dropped = Arc::new(AtomicU64::new(0));
        let receiver_audio = Arc::clone(&audio);
        let receiver_dropped = Arc::clone(&audio_dropped);
        let audio_reader = thread::Builder::new()
            .name("audio-events".into())
            .spawn(move || {
                loop {
                    let event = read_audio_event(&mut audio_pipe);
                    let closed = event.is_err();
                    if matches!(
                        receiver_audio.push(event),
                        Some(Ok(AudioWorkerEvent::Packet { .. }))
                    ) {
                        receiver_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    if closed {
                        receiver_audio.close();
                        break;
                    }
                }
            });
        let audio_reader = match audio_reader {
            Ok(reader) => Some(reader),
            Err(error) => {
                audio.push(Err(error));
                audio.close();
                None
            }
        };
        write_command(
            &mut self.command,
            WorkerCommand::StartVideoStream {
                frames_per_second,
                bitrate_bits_per_second,
                audio: enable_audio,
            },
        )?;
        Ok(Some(MediaWorkerStream {
            worker: self,
            events,
            dropped_events,
            reader: Some(reader),
            audio,
            audio_dropped,
            audio_reader,
        }))
    }

    pub fn prepare_video_stream(
        &mut self,
        shutdown_rx: &Receiver<()>,
    ) -> Result<VideoPreparation, Box<dyn std::error::Error>> {
        if self.hello_accepted {
            return Err("media worker video stream was already prepared".into());
        }
        let hello = {
            let events = self
                .events
                .as_mut()
                .ok_or("worker events already consumed")?;
            read_event(events)
        }
        .map_err(|error| self.event_read_error("hello", error))?;
        if !self.accept_hello(Some(hello))? {
            return Ok(VideoPreparation::Interrupted);
        }
        self.hello_accepted = true;
        write_command(&mut self.command, WorkerCommand::PrepareVideoStream)?;
        if shutdown_rx.try_recv().is_ok() {
            write_command(&mut self.command, WorkerCommand::Stop)?;
            return Ok(VideoPreparation::Interrupted);
        }
        let event = {
            let events = self
                .events
                .as_mut()
                .ok_or("worker events already consumed")?;
            read_event(events)
        }
        .map_err(|error| self.event_read_error("video preparation", error))?;
        match event {
            WorkerEvent::VideoConfiguration(configuration) => {
                Ok(VideoPreparation::Ready(configuration))
            }
            WorkerEvent::SystemIdentityRequired => Ok(VideoPreparation::SystemIdentityRequired),
            WorkerEvent::Failure(error) => Err(error.into()),
            event => Err(format!("unexpected media worker event: {event:?}").into()),
        }
    }

    fn run_proof(
        &mut self,
        command: WorkerCommand,
        shutdown_rx: &Receiver<()>,
    ) -> Result<(bool, String), Box<dyn std::error::Error>> {
        let mut events = self.events.take().ok_or("worker events already consumed")?;
        let (event_tx, event_rx) = sync_channel(2);
        let reader = thread::spawn(move || {
            for _ in 0..2 {
                if event_tx.send(read_event(&mut events)).is_err() {
                    break;
                }
            }
        });

        let hello = wait_event(&event_rx, shutdown_rx, &mut self.command)
            .map_err(|error| self.worker_error("proof hello", error.as_ref()))?;
        if !self.accept_hello(hello)? {
            return Ok((true, "status=interrupted\n".to_owned()));
        }

        write_command(&mut self.command, command)?;
        let report = match wait_event(&event_rx, shutdown_rx, &mut self.command)
            .map_err(|error| self.worker_error("proof result", error.as_ref()))?
        {
            Some(WorkerEvent::CaptureReport(report)) => report,
            Some(event) => return Err(format!("unexpected media worker event: {event:?}").into()),
            None => return Ok((true, "status=interrupted\n".to_owned())),
        };
        reader
            .join()
            .map_err(|_| "media worker event reader panicked")?;
        Ok((false, self.report_with_identity(&report)))
    }

    fn accept_hello(&self, hello: Option<WorkerEvent>) -> Result<bool, Box<dyn std::error::Error>> {
        match hello {
            Some(event)
                if worker_hello_is_valid(
                    &event,
                    self.process_id,
                    self.session_id,
                    self.identity,
                    self.connection_token,
                ) =>
            {
                Ok(true)
            }
            Some(event) => Err(format!("invalid media worker hello: {event:?}").into()),
            None => Ok(false),
        }
    }

    fn report_with_identity(&self, report: &str) -> String {
        format!(
            "worker_process_id={}\nworker_session_id={}\nworker_identity={}\n{report}",
            self.process_id,
            self.session_id,
            match self.identity {
                WorkerIdentity::ActiveUser => "active-user",
                WorkerIdentity::LocalSystem => "LOCAL_SYSTEM",
            }
        )
    }

    fn event_read_error(&self, stage: &str, error: io::Error) -> io::Error {
        io::Error::new(error.kind(), self.worker_error(stage, &error))
    }

    fn worker_error(&self, stage: &str, error: &dyn std::fmt::Display) -> String {
        let mut exit_code = u32::MAX;
        // SAFETY: the worker process handle remains owned by this supervisor.
        let exit = unsafe {
            windows::Win32::System::Threading::GetExitCodeProcess(
                self._process.get(),
                &mut exit_code,
            )
        }
        .map(|()| exit_code.to_string())
        .unwrap_or_else(|query_error| format!("unknown ({query_error})"));
        format!(
            "media worker {} failed while reading {stage}: {error}; process exit code {exit}",
            match self.identity {
                WorkerIdentity::ActiveUser => "active-user",
                WorkerIdentity::LocalSystem => "LOCAL_SYSTEM",
            }
        )
    }

    pub fn stop(&mut self) -> io::Result<()> {
        write_command(&mut self.command, WorkerCommand::Stop)
    }
}

fn worker_hello_is_valid(
    event: &WorkerEvent,
    process_id: u32,
    session_id: u32,
    identity: WorkerIdentity,
    connection_token: [u8; 16],
) -> bool {
    matches!(
        event,
        WorkerEvent::Hello {
            version,
            process_id: actual_process_id,
            session_id: actual_session_id,
            identity: actual_identity,
            connection_token: actual_connection_token,
        } if *version == VERSION
            && *actual_process_id == process_id
            && *actual_session_id == session_id
            && *actual_identity == identity
            && *actual_connection_token == connection_token
    )
}

#[expect(dead_code, reason = "consumed by the QUIC media integration")]
impl MediaWorkerStream {
    pub fn next_audio_event(&self) -> Option<io::Result<AudioWorkerEvent>> {
        self.audio.pop_timeout(Duration::ZERO)
    }
    pub fn audio_dropped(&self) -> u64 {
        self.audio_dropped.load(Ordering::Relaxed)
    }
    pub fn next_event(
        &mut self,
        shutdown_rx: &Receiver<()>,
    ) -> Result<Option<WorkerEvent>, Box<dyn std::error::Error>> {
        wait_latest_event(&self.events, shutdown_rx, &mut self.worker.command)
    }

    pub fn next_event_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Option<WorkerEvent>, Box<dyn std::error::Error>> {
        self.events
            .pop_timeout(timeout)
            .transpose()
            .map_err(Into::into)
    }

    pub fn set_bitrate(&mut self, bitrate_bits_per_second: u64) -> io::Result<()> {
        write_command(
            &mut self.worker.command,
            WorkerCommand::SetVideoBitrate(bitrate_bits_per_second),
        )
    }

    pub fn set_frame_divisor(&mut self, divisor: u8) -> io::Result<()> {
        write_command(
            &mut self.worker.command,
            WorkerCommand::SetVideoFrameDivisor(divisor),
        )
    }

    pub fn request_keyframe(&mut self) -> io::Result<()> {
        write_command(
            &mut self.worker.command,
            WorkerCommand::RequestVideoKeyframe,
        )
    }

    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }
}

impl Drop for MediaWorkerStream {
    fn drop(&mut self) {
        if let Err(error) = self.worker.stop_and_wait(false) {
            eprintln!("media worker shutdown: {error}");
        }
        self.worker._job.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.audio_reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for MediaWorker {
    fn drop(&mut self) {
        if self._job.is_none() {
            // SAFETY: interactive workers are opened with PROCESS_TERMINATE and
            // may otherwise outlive a failed or disconnected supervisor.
            let _ = unsafe {
                windows::Win32::System::Threading::TerminateProcess(self._process.get(), 1)
            };
        }
    }
}

pub fn run(
    command_read: usize,
    event_write: usize,
    audio_write: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: these handles are supplied through the explicit inherited handle
    // list and ownership transfers to the worker process exactly once.
    let commands = unsafe { File::from_raw_handle(command_read as *mut _) };
    let events = unsafe { File::from_raw_handle(event_write as *mut _) };
    let audio_events = unsafe { File::from_raw_handle(audio_write as *mut _) };
    run_files(commands, events, audio_events, [0; 16])
}

pub fn run_named(
    control_pipe: &str,
    audio_pipe: &str,
    connection_token: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection_token = parse_connection_token(connection_token)?;
    let (commands, events, audio_events) =
        crate::interactive_worker::connect(control_pipe, audio_pipe)?;
    run_files(commands, events, audio_events, connection_token)
}

fn run_files(
    mut commands: File,
    mut events: File,
    audio_events: File,
    connection_token: [u8; 16],
) -> Result<(), Box<dyn std::error::Error>> {
    let identity = if current_process_is_local_system()? {
        WorkerIdentity::LocalSystem
    } else {
        WorkerIdentity::ActiveUser
    };
    let process_id = unsafe { GetCurrentProcessId() };
    let mut session_id = 0;
    // SAFETY: session_id is valid output storage.
    unsafe { ProcessIdToSessionId(process_id, &mut session_id)? };
    write_event(
        &mut events,
        &WorkerEvent::Hello {
            version: VERSION,
            process_id,
            session_id,
            identity,
            connection_token,
        },
    )?;
    let mut prepared_video = None;

    loop {
        match read_command(&mut commands)? {
            command @ (WorkerCommand::AudioProof | WorkerCommand::AudioEncodeProof) => {
                let audio_commands = commands.try_clone()?;
                let audio =
                    thread::Builder::new()
                        .name("system-audio".into())
                        .spawn(move || {
                            audio_capture_report(
                                &audio_commands,
                                command == WorkerCommand::AudioEncodeProof,
                                identity,
                            )
                        })?;
                let report = audio.join().map_err(|_| "audio proof thread panicked")?;
                write_event(&mut events, &WorkerEvent::CaptureReport(report))?;
            }
            WorkerCommand::CaptureProof => {
                let report = capture_report(&commands, CaptureGoal::Frames, None);
                write_event(&mut events, &WorkerEvent::CaptureReport(report))?;
            }
            WorkerCommand::DesktopTransitionProof => {
                let report = capture_report(
                    &commands,
                    CaptureGoal::Transition(TransitionGoal::DesktopRoundTrip),
                    None,
                );
                write_event(&mut events, &WorkerEvent::CaptureReport(report))?;
            }
            WorkerCommand::LoginTransitionProof => {
                let report = capture_report(
                    &commands,
                    CaptureGoal::Transition(TransitionGoal::Login),
                    None,
                );
                write_event(&mut events, &WorkerEvent::CaptureReport(report))?;
            }
            WorkerCommand::DisplayModeTransitionProof => {
                let report = capture_report(
                    &commands,
                    CaptureGoal::DisplayModeTransition,
                    Some(&mut events),
                );
                write_event(&mut events, &WorkerEvent::CaptureReport(report))?;
            }
            WorkerCommand::PrepareVideoStream => {
                if prepared_video.is_some() {
                    write_event(
                        &mut events,
                        &WorkerEvent::Failure("video stream is already prepared".to_owned()),
                    )?;
                    continue;
                }
                let prepared = match crate::desktop::attach_input_desktop() {
                    Ok(desktop) => PreparedGpuCapture::for_desktop(&desktop),
                    Err(error)
                        if identity == WorkerIdentity::ActiveUser
                            && error.code() == E_ACCESSDENIED =>
                    {
                        write_event(&mut events, &WorkerEvent::SystemIdentityRequired)?;
                        continue;
                    }
                    Err(error) => Err(error.into()),
                };
                match prepared {
                    Ok(prepared) => {
                        let configuration = prepared.configuration();
                        prepared_video = Some(prepared);
                        write_event(&mut events, &WorkerEvent::VideoConfiguration(configuration))?;
                    }
                    Err(error) => {
                        write_event(&mut events, &WorkerEvent::Failure(error.to_string()))?
                    }
                }
            }
            WorkerCommand::EncodeSnapshot {
                frames_per_second,
                bitrate_bits_per_second,
            } => {
                let encoder = match prepared_video.take() {
                    Some(prepared) => GpuAv1SnapshotEncoder::from_prepared(
                        prepared,
                        frames_per_second,
                        bitrate_bits_per_second,
                    ),
                    None => GpuAv1SnapshotEncoder::new(frames_per_second, bitrate_bits_per_second),
                };
                match encoder
                    .and_then(|mut encoder| encoder.encode_snapshot(Duration::from_secs(5)))
                {
                    Ok(snapshot) => write_event(
                        &mut events,
                        &WorkerEvent::EncodedSnapshot {
                            last_present_time: snapshot.last_present_time,
                            accumulated_frames: snapshot.accumulated_frames,
                            protected_content_masked: snapshot.protected_content_masked,
                            presentation_timestamp: snapshot.packet.presentation_timestamp,
                            keyframe: snapshot.packet.keyframe,
                            payload: snapshot.packet.data,
                        },
                    )?,
                    Err(error) => {
                        write_event(&mut events, &WorkerEvent::Failure(error.to_string()))?
                    }
                }
            }
            WorkerCommand::StartVideoStream {
                frames_per_second,
                bitrate_bits_per_second,
                audio,
            } => match run_video_stream(
                &mut commands,
                &mut events,
                frames_per_second,
                bitrate_bits_per_second,
                if audio { Some(&audio_events) } else { None },
                prepared_video.take(),
            ) {
                Ok(StreamExit::Continue) => {}
                Ok(StreamExit::StopWorker) => return Ok(()),
                Err(error) => write_event(&mut events, &WorkerEvent::Failure(error.to_string()))?,
            },
            WorkerCommand::SetVideoBitrate(_)
            | WorkerCommand::SetVideoFrameDivisor(_)
            | WorkerCommand::RequestVideoKeyframe
            | WorkerCommand::StopVideoStream => {
                write_event(
                    &mut events,
                    &WorkerEvent::Failure(
                        "video stream control received while no stream is active".to_owned(),
                    ),
                )?;
            }
            WorkerCommand::Stop => return Ok(()),
        }
    }
}

fn parse_connection_token(value: &str) -> Result<[u8; 16], Box<dyn std::error::Error>> {
    if value.len() != 32 {
        return Err(
            "interactive worker connection token must contain 32 hexadecimal digits".into(),
        );
    }
    let mut token = [0; 16];
    for (index, output) in token.iter_mut().enumerate() {
        *output = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| "interactive worker connection token is not hexadecimal")?;
    }
    Ok(token)
}

fn audio_capture_report(commands: &File, encode_opus: bool, identity: WorkerIdentity) -> String {
    use crate::audio::CableCapture;
    use crate::audio_encode::{OpusAudioEncoder, OpusEncoderConfiguration};
    use rustconsole_host_core::{AudioCaptureEvent, SystemAudioCapture};
    use rustconsole_media::AudioEncoder;
    let result = (|| -> Result<String, Box<dyn std::error::Error>> {
        let started = Instant::now();
        let mut capture = CableCapture::new()?;
        let mut encoder = if encode_opus {
            Some(OpusAudioEncoder::new(OpusEncoderConfiguration {
                bitrate_bits_per_second: 128_000,
                packet_duration_micros: 10_000,
            })?)
        } else {
            None
        };
        let mut encoding_times = Vec::new();
        let mut encoder_needs_reset = false;
        let mut packets = 0_u64;
        let mut frames = 0_u64;
        let mut nonzero_samples = 0_u64;
        let mut discontinuities = 0_u64;
        let mut invalid_timestamps = 0_u64;
        let mut unavailable_polls = 0_u64;
        let mut first_timestamp = None;
        let mut last_timestamp = None;
        let mut peak = 0.0_f32;
        let mut squares = 0.0_f64;
        let mut maximum_packet_frames = 0;
        let mut interrupted = false;
        while started.elapsed() < Duration::from_secs(15) {
            if stop_requested(commands)? {
                interrupted = true;
                break;
            }
            match capture.next_samples()? {
                AudioCaptureEvent::Samples {
                    samples,
                    discontinuity,
                } => {
                    if samples.format.sample_rate != 48_000
                        || samples.format.channels != 2
                        || samples.interleaved.len() % 2 != 0
                    {
                        return Err("invalid capture sample format".into());
                    }
                    if last_timestamp.is_some_and(|last| samples.captured_at.0 <= last) {
                        return Err("capture timestamp did not advance".into());
                    }
                    first_timestamp.get_or_insert(samples.captured_at.0);
                    last_timestamp = Some(samples.captured_at.0);
                    discontinuities += u64::from(discontinuity);
                    packets += 1;
                    let count = samples.interleaved.len() as u64 / 2;
                    frames += count;
                    maximum_packet_frames = maximum_packet_frames.max(count);
                    for &sample in &samples.interleaved {
                        if !sample.is_finite() {
                            return Err("non-finite captured sample".into());
                        }
                        nonzero_samples += u64::from(sample.abs() > 0.000_01);
                        peak = peak.max(sample.abs());
                        squares += f64::from(sample).powi(2);
                    }
                    if let Some(encoder) = encoder.as_mut() {
                        if (discontinuity || encoder_needs_reset) && packets > 1 {
                            encoder.reset()?;
                        }
                        encoder_needs_reset = false;
                        if encoding_times.len() == 15_000 {
                            return Err("encoding timing sample bound reached".into());
                        }
                        let before = Instant::now();
                        let encoded = encoder.encode(samples)?;
                        encoding_times.push(before.elapsed().as_micros() as u64);
                        if encoded.iter().any(|packet| {
                            packet.payload.is_empty() || packet.decoded_samples != 480
                        }) {
                            return Err("invalid Opus proof packet".into());
                        }
                    }
                }
                AudioCaptureEvent::Idle => thread::sleep(Duration::from_millis(2)),
                AudioCaptureEvent::Unavailable => {
                    unavailable_polls += 1;
                    encoder_needs_reset = true;
                    thread::sleep(Duration::from_millis(10));
                }
                AudioCaptureEvent::InvalidTimestamp => {
                    invalid_timestamps += 1;
                    encoder_needs_reset = true;
                }
            }
        }
        let status = if interrupted {
            "interrupted"
        } else if frames >= 48_000 && nonzero_samples >= 4_800 {
            "ok"
        } else {
            "no-signal"
        };
        let mut report = format!(
            "status={status}\nworker_identity={}\ncapture_thread=system-audio\ncapture_mode=direct-vb-cable\nsample_rate_hz=48000\nchannels=2\npackets={packets}\nframes={frames}\nnonzero_samples={nonzero_samples}\npeak={peak}\nrms={}\nmaximum_packet_frames={maximum_packet_frames}\nfirst_timestamp_micros={}\nlast_timestamp_micros={}\ntimestamp_clock=windows-qpc-microseconds\ndiscontinuities={discontinuities}\ninvalid_timestamps={invalid_timestamps}\nunavailable_polls={unavailable_polls}\ndevice_opens={}\ndevice_id={}\nunavailable_reason={}\nelapsed_micros={}\nrecorded_audio_saved=false\n",
            match identity {
                WorkerIdentity::ActiveUser => "active-user",
                WorkerIdentity::LocalSystem => "LOCAL_SYSTEM",
            },
            (squares / (frames.max(1) * 2) as f64).sqrt(),
            first_timestamp.unwrap_or(0),
            last_timestamp.unwrap_or(0),
            capture.device_reopens,
            capture.device_id().unwrap_or("none"),
            capture.last_unavailable_reason.as_deref().unwrap_or("none"),
            started.elapsed().as_micros(),
        );
        capture.reset()?;
        report.push_str("routing_restored=true\n");
        if let Some(encoder) = encoder.as_mut() {
            if interrupted {
                encoder.reset()?;
            } else {
                encoder.finish()?;
            }
            let stats = encoder.statistics();
            encoding_times.sort_unstable();
            let percentile = |percent: usize| {
                if encoding_times.is_empty() {
                    0
                } else {
                    encoding_times[(encoding_times.len() * percent)
                        .div_ceil(100)
                        .saturating_sub(1)]
                }
            };
            report.push_str(&format!(
                "codec=libopus\nproof_bitrate_bits_per_second=128000\nproof_packet_duration_micros=10000\nencoder_delay_samples={}\nencoder_delay_micros={}\nencoded_packets={}\ninput_shared_bytes={}\ninput_copied_bytes={}\nzero_padding_bytes={}\npacket_copied_bytes={}\nmaximum_pending_frames={}\nencoder_resets={}\ndiscarded_samples={}\nencode_measurements={}\nencode_p50_micros={}\nencode_p95_micros={}\nencode_p99_micros={}\nencode_worst_micros={}\n",
                encoder.delay_samples(), u64::from(encoder.delay_samples()) * 1_000_000 / 48_000,
                stats.encoded_packets, stats.input_shared_bytes, stats.input_copied_bytes,
                stats.zero_padding_bytes, stats.packet_copied_bytes, stats.maximum_pending_frames,
                stats.resets, stats.discarded_samples, encoding_times.len(), percentile(50), percentile(95), percentile(99), percentile(100),
            ));
        }
        Ok(report)
    })();
    result.unwrap_or_else(|error| format!("status=error\nerror={error}\n"))
}

enum StreamExit {
    Continue,
    StopWorker,
}

fn run_video_stream(
    commands: &mut File,
    events: &mut File,
    frames_per_second: u16,
    bitrate_bits_per_second: u64,
    audio_pipe: Option<&File>,
    prepared: Option<PreparedGpuCapture>,
) -> Result<StreamExit, Box<dyn std::error::Error>> {
    if frames_per_second == 0 {
        return Err("video stream frame rate is zero".into());
    }
    let frame_period = Duration::from_nanos(1_000_000_000 / u64::from(frames_per_second));
    let prepared = prepared.ok_or("video stream was not prepared before start")?;
    let mut encoder =
        GpuAv1SnapshotEncoder::from_prepared(prepared, frames_per_second, bitrate_bits_per_second)?;
    let _audio = match audio_pipe {
        Some(pipe) => match pipe
            .try_clone()
            .and_then(crate::audio_stream::WorkerAudio::start)
        {
            Ok(audio) => Some(audio),
            Err(error) => {
                let state = rustconsole_protocol::wire::AudioStreamState::new(
                    1,
                    rustconsole_protocol::wire::AudioStatus::Failed,
                    0,
                    error.to_string(),
                );
                if let Err(report_error) = crate::worker_protocol::write_audio_event(
                    &mut &*pipe,
                    &AudioWorkerEvent::State(state),
                ) {
                    eprintln!("audio startup error could not be reported: {report_error}");
                }
                None
            }
        },
        None => None,
    };
    let mut next_tick = Instant::now();
    let mut sequence = 0_u64;
    let mut tick = 0_u64;
    let mut frame_divisor = 1_u8;

    loop {
        while stop_requested(commands)? {
            match read_command(commands)? {
                WorkerCommand::SetVideoBitrate(bitrate) => encoder.set_bitrate(bitrate)?,
                WorkerCommand::SetVideoFrameDivisor(divisor) if matches!(divisor, 1 | 2) => {
                    frame_divisor = divisor;
                }
                WorkerCommand::SetVideoFrameDivisor(_) => {
                    return Err("video frame divisor must be one or two".into());
                }
                WorkerCommand::RequestVideoKeyframe => encoder.request_keyframe()?,
                WorkerCommand::StopVideoStream => return Ok(StreamExit::Continue),
                WorkerCommand::Stop => return Ok(StreamExit::StopWorker),
                _ => return Err("invalid command received while video stream is active".into()),
            }
        }

        let now = Instant::now();
        if now < next_tick {
            thread::sleep(next_tick - now);
        }
        let encoded = if tick.is_multiple_of(u64::from(frame_divisor)) {
            match encoder.encode_next_frame(frame_period) {
                Ok(frame) => frame,
                Err(error) => {
                    if let Some(reconfiguration) =
                        error.downcast_ref::<VideoReconfigurationRequired>()
                    {
                        write_event(
                            events,
                            &WorkerEvent::VideoReconfigurationRequired(reconfiguration.cause),
                        )?;
                        return Ok(StreamExit::Continue);
                    }
                    return Err(error);
                }
            }
        } else {
            None
        };
        if let Some(frame) = encoded {
            write_event(
                events,
                &WorkerEvent::EncodedVideoFrame {
                    sequence,
                    last_present_time: frame.last_present_time,
                    accumulated_frames: frame.accumulated_frames,
                    protected_content_masked: frame.protected_content_masked,
                    presentation_timestamp: frame.packet.presentation_timestamp,
                    keyframe: frame.packet.keyframe,
                    payload: frame.packet.data,
                },
            )?;
            sequence = sequence.checked_add(1).ok_or("video sequence exhausted")?;
        }
        tick = tick.wrapping_add(1);

        next_tick += frame_period;
        let now = Instant::now();
        while next_tick <= now {
            next_tick += frame_period;
        }
    }
}

#[derive(Clone, Copy)]
enum CaptureGoal {
    Frames,
    Transition(TransitionGoal),
    DisplayModeTransition,
}

impl CaptureGoal {
    const fn limit(self) -> Duration {
        match self {
            Self::Frames => CAPTURE_LIMIT,
            Self::Transition(TransitionGoal::DesktopRoundTrip) => DESKTOP_TRANSITION_LIMIT,
            Self::Transition(TransitionGoal::Login) => LOGIN_TRANSITION_LIMIT,
            Self::DisplayModeTransition => DISPLAY_MODE_TRANSITION_LIMIT,
        }
    }
}

#[derive(Default)]
struct RecoveryStats {
    initialization_attempts: u64,
    temporarily_unavailable: u64,
    access_lost: u64,
    access_denied: u64,
    reinitializations: u64,
    desktop_transitions: u64,
}

fn capture_report(commands: &File, goal: CaptureGoal, events: Option<&mut File>) -> String {
    match capture_report_inner(commands, goal, events) {
        Ok(report) => report,
        Err(error) => format!("status=error\nerror={error}\n"),
    }
}

fn capture_report_inner(
    commands: &File,
    goal: CaptureGoal,
    mut events: Option<&mut File>,
) -> Result<String, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let limit = goal.limit();
    let mut recovery = RecoveryStats::default();
    let mut capture = initialize_capture(commands, started, limit, &mut recovery)?;
    let initial_desktop = capture.desktop_name().to_owned();
    let mut frames = 0_u64;
    let mut pointer_only_frames = 0_u64;
    let mut accumulated_frames = 0_u64;
    let mut last_sequence = 0_u64;
    let mut last_timestamp = 0_u64;
    let mut protected_content_masked = false;
    let mut transition = match goal {
        CaptureGoal::Frames | CaptureGoal::DisplayModeTransition => None,
        CaptureGoal::Transition(goal) => Some(TransitionProgress::new(goal, &initial_desktop)?),
    };
    let mut display_mode = match goal {
        CaptureGoal::DisplayModeTransition => {
            Some(DisplayModeProgress::new(display_mode_snapshot(&capture)))
        }
        _ => None,
    };

    while !goal_complete(goal, frames, transition.as_ref(), display_mode.as_ref())
        && started.elapsed() < limit
    {
        if stop_requested(commands)? {
            return Err("capture interrupted by service stop".into());
        }
        match capture.next_frame() {
            Ok(frame) if frame.frame.last_present_time() == 0 => {
                pointer_only_frames += 1;
            }
            Ok(frame) => {
                frames += 1;
                if let Some(progress) = transition.as_mut() {
                    progress.presented(capture.desktop_name());
                }
                if let Some(progress) = display_mode.as_mut() {
                    if let Some(phase) = progress.presented(&display_mode_snapshot(&capture)) {
                        let report = display_mode_progress_report(phase, progress, &recovery)?;
                        let events = events
                            .as_deref_mut()
                            .ok_or("display-mode proof has no progress event pipe")?;
                        write_event(events, &WorkerEvent::ProofProgress(report))?;
                    }
                }
                accumulated_frames += u64::from(frame.frame.accumulated_frames());
                last_sequence = frame.sequence;
                last_timestamp = frame.captured_at.0;
                protected_content_masked |= frame.frame.protected_content_masked();
            }
            Err(error) => match acquisition_action(capture_failure(&error)) {
                CaptureAction::Continue => {}
                CaptureAction::Reinitialize => {
                    record_recovery(&mut recovery, &error);
                    let previous_desktop = capture.desktop_name().to_owned();
                    drop(capture);
                    thread::sleep(REINITIALIZATION_DELAY);
                    capture = initialize_capture(commands, started, limit, &mut recovery)?;
                    recovery.reinitializations += 1;
                    if let Some(progress) = display_mode.as_mut() {
                        progress.reinitialized();
                    }
                    if capture.desktop_name() != previous_desktop {
                        recovery.desktop_transitions += 1;
                        if let Some(progress) = transition.as_mut() {
                            progress.desktop_attached(capture.desktop_name());
                        }
                    }
                }
                CaptureAction::Fail => return Err(error.into()),
            },
        }
    }
    if !goal_complete(goal, frames, transition.as_ref(), display_mode.as_ref()) {
        return Err(match goal {
            CaptureGoal::Frames => {
                format!("captured only {frames} of {FRAME_TARGET} required frames")
            }
            CaptureGoal::Transition(goal) => format!(
                "transition proof timed out: required_path={}, observed_path={}, transitions={}",
                goal.required_path(),
                transition
                    .as_ref()
                    .map_or_else(String::new, TransitionProgress::observed_path),
                recovery.desktop_transitions
            ),
            CaptureGoal::DisplayModeTransition => format!(
                "display-mode transition proof timed out after {} resource reinitializations",
                recovery.reinitializations
            ),
        }
        .into());
    }
    let format = capture.format();
    let transition_report = transition.map_or_else(String::new, |progress| {
        format!(
            "required_path={}\nobserved_path={}\n",
            goal.transition_goal().unwrap().required_path(),
            progress.observed_path()
        )
    });
    let display_mode_report = display_mode.map_or_else(String::new, |progress| {
        let initial = progress.initial();
        let changed = progress
            .changed()
            .expect("a complete display-mode proof records the changed mode");
        format!(
            "initial_display={}\ninitial_width={}\ninitial_height={}\ninitial_refresh_hz={}\ninitial_dxgi_format={}\nchanged_display={}\nchanged_width={}\nchanged_height={}\nchanged_refresh_hz={}\nchanged_dxgi_format={}\nrestored_display={}\nrestored_width={}\nrestored_height={}\nrestored_refresh_hz={}\nrestored_dxgi_format={}\nresource_generations={}\n",
            initial.display,
            initial.format.width,
            initial.format.height,
            initial.format.frames_per_second,
            initial.dxgi_format,
            changed.display,
            changed.format.width,
            changed.format.height,
            changed.format.frames_per_second,
            changed.dxgi_format,
            capture.display_name(),
            format.width,
            format.height,
            format.frames_per_second,
            capture.dxgi_format(),
            progress.generation(),
        )
    });
    Ok(format!(
        "status=ok\ninitial_desktop={}\nfinal_desktop={}\n{transition_report}{display_mode_report}display={}\nwidth={}\nheight={}\nrefresh_hz={}\ndxgi_format={}\ninitialization_attempts={}\ntemporarily_unavailable={}\naccess_lost={}\naccess_denied={}\nreinitializations={}\ndesktop_transitions={}\nframes={}\npointer_only_frames={}\naccumulated_frames={}\nlast_sequence={}\nlast_captured_at_micros={}\nprotected_content_masked={}\nelapsed_micros={}\n",
        initial_desktop,
        capture.desktop_name(),
        capture.display_name(),
        format.width,
        format.height,
        format.frames_per_second,
        capture.dxgi_format(),
        recovery.initialization_attempts,
        recovery.temporarily_unavailable,
        recovery.access_lost,
        recovery.access_denied,
        recovery.reinitializations,
        recovery.desktop_transitions,
        frames,
        pointer_only_frames,
        accumulated_frames,
        last_sequence,
        last_timestamp,
        protected_content_masked,
        started.elapsed().as_micros(),
    ))
}

fn initialize_capture(
    commands: &File,
    started: Instant,
    limit: Duration,
    recovery: &mut RecoveryStats,
) -> Result<DesktopDuplicationCapture, Box<dyn std::error::Error>> {
    loop {
        if stop_requested(commands)? {
            return Err("capture interrupted by service stop".into());
        }
        recovery.initialization_attempts += 1;
        match DesktopDuplicationCapture::new(ACQUIRE_TIMEOUT) {
            Ok(capture) => return Ok(capture),
            Err(error)
                if retry_initialization(capture_failure(&error), started.elapsed() < limit) =>
            {
                record_recovery(recovery, &error);
                thread::sleep(ACQUIRE_TIMEOUT);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn goal_complete(
    goal: CaptureGoal,
    frames: u64,
    transition: Option<&TransitionProgress>,
    display_mode: Option<&DisplayModeProgress>,
) -> bool {
    match goal {
        CaptureGoal::Frames => frames >= FRAME_TARGET,
        CaptureGoal::Transition(_) => transition.is_some_and(TransitionProgress::complete),
        CaptureGoal::DisplayModeTransition => {
            display_mode.is_some_and(DisplayModeProgress::complete)
        }
    }
}

impl CaptureGoal {
    const fn transition_goal(self) -> Option<TransitionGoal> {
        match self {
            Self::Frames => None,
            Self::Transition(goal) => Some(goal),
            Self::DisplayModeTransition => None,
        }
    }
}

fn display_mode_snapshot(capture: &DesktopDuplicationCapture) -> DisplayModeSnapshot {
    DisplayModeSnapshot {
        display: capture.display_name().to_owned(),
        format: capture.format(),
        dxgi_format: capture.dxgi_format(),
    }
}

fn display_mode_progress_report(
    phase: DisplayModePhase,
    progress: &DisplayModeProgress,
    recovery: &RecoveryStats,
) -> Result<String, Box<dyn std::error::Error>> {
    let snapshot = match phase {
        DisplayModePhase::InitialCaptured => progress.initial(),
        DisplayModePhase::ChangedCaptured => progress
            .changed()
            .ok_or("changed display mode was not recorded")?,
    };
    let phase = match phase {
        DisplayModePhase::InitialCaptured => "initial",
        DisplayModePhase::ChangedCaptured => "changed",
    };
    Ok(format!(
        "status=running\nphase={phase}\ndisplay={}\nwidth={}\nheight={}\nrefresh_hz={}\ndxgi_format={}\nreinitializations={}\n",
        snapshot.display,
        snapshot.format.width,
        snapshot.format.height,
        snapshot.format.frames_per_second,
        snapshot.dxgi_format,
        recovery.reinitializations,
    ))
}

fn capture_failure(error: &DesktopDuplicationError) -> CaptureFailure {
    match error {
        DesktopDuplicationError::Timeout => CaptureFailure::Timeout,
        DesktopDuplicationError::AccessLost => CaptureFailure::AccessLost,
        DesktopDuplicationError::AccessDenied => CaptureFailure::AccessDenied,
        DesktopDuplicationError::DesktopNotReady => CaptureFailure::DesktopNotReady,
        DesktopDuplicationError::TemporarilyUnavailable => CaptureFailure::TemporarilyUnavailable,
        DesktopDuplicationError::Windows(error) if error.code() == DXGI_ERROR_INVALID_CALL => {
            CaptureFailure::DesktopNotReady
        }
        _ => CaptureFailure::Fatal,
    }
}

fn record_recovery(recovery: &mut RecoveryStats, error: &DesktopDuplicationError) {
    match error {
        DesktopDuplicationError::TemporarilyUnavailable => recovery.temporarily_unavailable += 1,
        DesktopDuplicationError::AccessLost => recovery.access_lost += 1,
        DesktopDuplicationError::AccessDenied => recovery.access_denied += 1,
        DesktopDuplicationError::DesktopNotReady => recovery.temporarily_unavailable += 1,
        _ => {}
    }
}

fn wait_event(
    events: &Receiver<io::Result<WorkerEvent>>,
    shutdown: &Receiver<()>,
    commands: &mut File,
) -> Result<Option<WorkerEvent>, Box<dyn std::error::Error>> {
    loop {
        if shutdown.try_recv().is_ok() {
            let _ = write_command(commands, WorkerCommand::Stop);
            return Ok(None);
        }
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(event) => return Ok(Some(event?)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err("media worker event pipe closed".into());
            }
        }
    }
}

fn wait_latest_event(
    events: &LatestQueue<io::Result<WorkerEvent>>,
    shutdown: &Receiver<()>,
    commands: &mut File,
) -> Result<Option<WorkerEvent>, Box<dyn std::error::Error>> {
    loop {
        if shutdown.try_recv().is_ok() {
            let _ = write_command(commands, WorkerCommand::Stop);
            return Ok(None);
        }
        if let Some(event) = events.pop_timeout(Duration::from_millis(50)) {
            return Ok(Some(event?));
        }
    }
}

fn stop_requested(commands: &File) -> io::Result<bool> {
    let mut available = 0;
    // SAFETY: commands is an open anonymous-pipe read handle and available is
    // valid output storage.
    unsafe {
        PeekNamedPipe(
            HANDLE(commands.as_raw_handle()),
            None,
            0,
            None,
            Some(&mut available),
            None,
        )?;
    }
    Ok(available != 0)
}

fn inheritable_pipe() -> windows::core::Result<(File, File)> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    let attributes = windows::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<
            windows::Win32::Security::SECURITY_ATTRIBUTES,
        >())
        .unwrap_or(u32::MAX),
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: BOOL(1),
    };
    // SAFETY: output handles and security attributes are valid.
    unsafe { CreatePipe(&mut read, &mut write, Some(&attributes), 0)? };
    // SAFETY: CreatePipe returned distinct owned handles.
    Ok(unsafe {
        (
            File::from_raw_handle(read.0),
            File::from_raw_handle(write.0),
        )
    })
}

fn set_not_inheritable(file: &File) -> windows::core::Result<()> {
    // SAFETY: file owns a valid kernel handle.
    unsafe { SetHandleInformation(raw_handle(file), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
}

fn active_user_token(session_id: u32) -> windows::core::Result<OwnedHandle> {
    let mut token = HANDLE::default();
    // SAFETY: token is valid output storage and the service verifies the
    // active console session immediately before this call.
    unsafe { WTSQueryUserToken(session_id, &mut token)? };
    OwnedHandle::new(token)
}

fn active_user_token_is_unavailable(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|error| error.code() == windows::core::HRESULT::from_win32(ERROR_NO_TOKEN.0))
}

fn session_system_token(session_id: u32) -> windows::core::Result<OwnedHandle> {
    let mut current = HANDLE::default();
    // SAFETY: current is valid output storage.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
            &mut current,
        )
        .map_err(|error| windows_error("OpenProcessToken", error))?;
    }
    let current = OwnedHandle::new(current)?;
    let tcb_privilege = TcbPrivilege::enable(current.get())?;
    let mut duplicated = HANDLE::default();
    // SAFETY: both token handles and output storage are valid.
    unsafe {
        DuplicateTokenEx(
            current.get(),
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut duplicated,
        )
        .map_err(|error| windows_error("DuplicateTokenEx", error))?;
    }
    let duplicated = OwnedHandle::new(duplicated)?;
    // SAFETY: session_id is valid input storage for TokenSessionId.
    unsafe {
        SetTokenInformation(
            duplicated.get(),
            TokenSessionId,
            (&session_id as *const u32).cast(),
            u32::try_from(std::mem::size_of::<u32>()).unwrap_or(4),
        )
        .map_err(|error| windows_error("SetTokenInformation(TokenSessionId)", error))?;
    }
    drop(tcb_privilege);
    Ok(duplicated)
}

struct TcbPrivilege {
    token: HANDLE,
    previous: TOKEN_PRIVILEGES,
}

impl TcbPrivilege {
    fn enable(token: HANDLE) -> windows::core::Result<Self> {
        let mut luid = Default::default();
        // SAFETY: luid is valid output storage and SE_TCB_NAME is a static string.
        unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_TCB_NAME, &mut luid)? };
        let enabled = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let mut previous = TOKEN_PRIVILEGES::default();
        let mut previous_size =
            u32::try_from(std::mem::size_of::<TOKEN_PRIVILEGES>()).unwrap_or(u32::MAX);
        // SAFETY: all privilege structures are initialized and have their reported size.
        unsafe {
            AdjustTokenPrivileges(
                token,
                false,
                Some(&enabled),
                previous_size,
                Some(&mut previous),
                Some(&mut previous_size),
            )?;
            if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                return Err(windows::core::Error::new(
                    windows::core::HRESULT::from_win32(ERROR_NOT_ALL_ASSIGNED.0),
                    "the service token does not hold SeTcbPrivilege",
                ));
            }
        }
        if !token_privilege_is_enabled(token, luid)? {
            return Err(windows::core::Error::new(
                E_ACCESSDENIED,
                "SeTcbPrivilege remained disabled after AdjustTokenPrivileges",
            ));
        }
        Ok(Self { token, previous })
    }
}

impl Drop for TcbPrivilege {
    fn drop(&mut self) {
        // SAFETY: token remains owned by session_system_token until after this guard drops.
        let _ = unsafe {
            AdjustTokenPrivileges(self.token, false, Some(&self.previous), 0, None, None)
        };
    }
}

fn token_privilege_is_enabled(token: HANDLE, luid: LUID) -> windows::core::Result<bool> {
    let mut bytes = 0;
    // SAFETY: the zero-sized call obtains the required size.
    let _ = unsafe { GetTokenInformation(token, TokenPrivileges, None, 0, &mut bytes) };
    let words = usize::try_from(bytes)
        .unwrap_or(0)
        .div_ceil(std::mem::size_of::<usize>());
    let mut storage = vec![0_usize; words];
    // SAFETY: storage is pointer-aligned and has the reported byte size.
    unsafe {
        GetTokenInformation(
            token,
            TokenPrivileges,
            Some(storage.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )?;
        let privileges = &*storage.as_ptr().cast::<TOKEN_PRIVILEGES>();
        Ok(std::slice::from_raw_parts(
            privileges.Privileges.as_ptr(),
            usize::try_from(privileges.PrivilegeCount).unwrap_or(0),
        )
        .iter()
        .any(|privilege| {
            privilege.Luid == luid && privilege.Attributes.contains(SE_PRIVILEGE_ENABLED)
        }))
    }
}

fn current_process_is_local_system() -> windows::core::Result<bool> {
    let mut token = HANDLE::default();
    // SAFETY: token is valid output storage.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)? };
    let token = OwnedHandle::new(token)?;
    let mut size = 0;
    // SAFETY: the zero-sized call obtains the required size.
    let _ = unsafe { GetTokenInformation(token.get(), TokenUser, None, 0, &mut size) };
    let mut storage = vec![0_u8; usize::try_from(size).unwrap_or(0)];
    // SAFETY: storage has the reported size and TOKEN_USER is the requested type.
    unsafe {
        GetTokenInformation(
            token.get(),
            TokenUser,
            Some(storage.as_mut_ptr().cast()),
            size,
            &mut size,
        )?;
        let user = &*storage.as_ptr().cast::<TOKEN_USER>();
        Ok(IsWellKnownSid(user.User.Sid, WinLocalSystemSid).as_bool())
    }
}

fn kill_on_close_job() -> windows::core::Result<OwnedHandle> {
    // SAFETY: null name creates a private job object.
    let job = OwnedHandle::new(unsafe { CreateJobObjectW(None, PCWSTR::null())? })?;
    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: information is the structure required by the selected class.
    unsafe {
        SetInformationJobObject(
            job.get(),
            JobObjectExtendedLimitInformation,
            (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            u32::try_from(std::mem::size_of_val(&information)).unwrap_or(u32::MAX),
        )?;
    }
    Ok(job)
}

struct ProcessAttributes {
    storage: Vec<usize>,
    pointer: windows::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl ProcessAttributes {
    fn new(handles: &[HANDLE]) -> windows::core::Result<Self> {
        let mut bytes = 0;
        // SAFETY: the first call reports the required allocation size.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut bytes) };
        let words = bytes.div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0_usize; words];
        let pointer = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
        // SAFETY: storage is pointer-aligned and has the reported byte size.
        unsafe {
            InitializeProcThreadAttributeList(Some(pointer), 1, None, &mut bytes)?;
            UpdateProcThreadAttribute(
                pointer,
                0,
                usize::try_from(PROC_THREAD_ATTRIBUTE_HANDLE_LIST).unwrap_or(0),
                Some(handles.as_ptr().cast()),
                std::mem::size_of_val(handles),
                None,
                None,
            )?;
        }
        Ok(Self { storage, pointer })
    }

    const fn as_ptr(&mut self) -> windows::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST {
        self.pointer
    }
}

impl Drop for ProcessAttributes {
    fn drop(&mut self) {
        // SAFETY: pointer was initialized successfully and remains backed by storage.
        unsafe { DeleteProcThreadAttributeList(self.pointer) };
        let _ = self.storage.len();
    }
}

struct OwnedHandle(HANDLE);

// Windows kernel handles are process-wide. This wrapper owns one handle and
// moving it transfers that ownership without permitting concurrent access.
unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    fn new(handle: HANDLE) -> windows::core::Result<Self> {
        if handle.is_invalid() {
            Err(windows::core::Error::from_thread())
        } else {
            Ok(Self(handle))
        }
    }

    const fn get(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper has sole ownership of the valid handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_missing_active_user_token_selects_the_system_worker() {
        let missing = windows::core::Error::new(
            windows::core::HRESULT::from_win32(ERROR_NO_TOKEN.0),
            "no active user token",
        );
        let denied = windows::core::Error::new(E_ACCESSDENIED, "access denied");
        assert!(active_user_token_is_unavailable(&missing));
        assert!(!active_user_token_is_unavailable(&denied));
        assert!(!active_user_token_is_unavailable(&io::Error::other(
            "unrelated error"
        )));
    }

    #[test]
    fn interactive_hello_requires_every_bound_identity_field() {
        let token = [7; 16];
        let hello =
            |version, process_id, session_id, identity, connection_token| WorkerEvent::Hello {
                version,
                process_id,
                session_id,
                identity,
                connection_token,
            };
        assert!(worker_hello_is_valid(
            &hello(VERSION, 42, 1, WorkerIdentity::ActiveUser, token),
            42,
            1,
            WorkerIdentity::ActiveUser,
            token,
        ));
        for invalid in [
            hello(VERSION + 1, 42, 1, WorkerIdentity::ActiveUser, token),
            hello(VERSION, 43, 1, WorkerIdentity::ActiveUser, token),
            hello(VERSION, 42, 2, WorkerIdentity::ActiveUser, token),
            hello(VERSION, 42, 1, WorkerIdentity::LocalSystem, token),
            hello(VERSION, 42, 1, WorkerIdentity::ActiveUser, [8; 16]),
        ] {
            assert!(!worker_hello_is_valid(
                &invalid,
                42,
                1,
                WorkerIdentity::ActiveUser,
                token,
            ));
        }
    }

    #[test]
    fn named_worker_token_is_exactly_128_bits_of_hex() {
        assert_eq!(
            parse_connection_token("00112233445566778899aabbccddeeff").unwrap(),
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ]
        );
        assert!(parse_connection_token("0011").is_err());
        assert!(parse_connection_token("00112233445566778899aabbccddeefg").is_err());
    }
}

fn raw_handle(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn windows_error(context: &'static str, error: windows::core::Error) -> windows::core::Error {
    windows::core::Error::new(error.code(), format!("{context}: {error}"))
}
