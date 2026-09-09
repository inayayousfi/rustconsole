use rand::{RngCore, rngs::OsRng};
use std::fs::File;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING,
    GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_NOWAIT,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, SetNamedPipeHandleState,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSDomainName, WTSFreeMemory, WTSQuerySessionInformationW, WTSUserName,
};
use windows::Win32::System::Threading::{
    OpenProcess, OpenProcessToken, PROCESS_DUP_HANDLE, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::core::{PCWSTR, PWSTR};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const PIPE_BUFFER_SIZE: u32 = 16 * 1024 * 1024;

pub struct InteractiveConnection {
    pub command: File,
    pub events: File,
    pub audio_events: File,
    pub process: HANDLE,
    pub process_id: u32,
    pub connection_token: [u8; 16],
}

pub struct InteractiveHelperConnection {
    pub channel: File,
    pub process: HANDLE,
    pub process_id: u32,
    pub connection_token: [u8; 16],
}

pub fn launch_helper(
    executable: &Path,
    session_id: u32,
) -> Result<InteractiveHelperConnection, Box<dyn std::error::Error>> {
    let mut connection_token = [0; 16];
    OsRng.fill_bytes(&mut connection_token);
    let token_hex = hex(&connection_token);
    let suffix = &token_hex[..16];
    let pipe_name = format!(r"\\.\pipe\RC.{suffix}.w");
    let security = PipeSecurity::for_authenticated_user()?;
    let pipe = NamedPipeServer::new(&pipe_name, security.attributes())?;
    let account = session_account(session_id)?;
    let task_name = format!("RustConsoleWgcHelper-{suffix}");
    let action = format!("\"{}\" \"{pipe_name}\" {token_hex}", executable.display());
    let mut task = create_and_run_task(&task_name, &account, &action)?;
    let channel = pipe.connect()?;
    let mut process_id = 0;
    // SAFETY: channel is a connected local named-pipe server endpoint.
    unsafe { GetNamedPipeClientProcessId(HANDLE(channel.as_raw_handle()), &mut process_id)? };
    if process_id == 0 {
        return Err("WGC helper returned an invalid process id".into());
    }
    // SAFETY: process_id came from a connected local named-pipe endpoint.
    let process = unsafe {
        OpenProcess(
            PROCESS_DUP_HANDLE
                | PROCESS_QUERY_LIMITED_INFORMATION
                | PROCESS_SYNCHRONIZE
                | PROCESS_TERMINATE,
            false,
            process_id,
        )?
    };
    let mut actual_session_id = u32::MAX;
    // SAFETY: process_id identifies the connected helper and output storage is valid.
    unsafe { ProcessIdToSessionId(process_id, &mut actual_session_id)? };
    if actual_session_id != session_id {
        // SAFETY: process was opened with PROCESS_TERMINATE.
        let _ = unsafe { windows::Win32::System::Threading::TerminateProcess(process, 1) };
        let _ = unsafe { CloseHandle(process) };
        return Err("WGC helper is not running in the active console session".into());
    }
    task.delete()?;
    Ok(InteractiveHelperConnection {
        channel,
        process,
        process_id,
        connection_token,
    })
}

impl Drop for InteractiveHelperConnection {
    fn drop(&mut self) {
        // SAFETY: this guard owns a process handle opened with terminate rights.
        let _ = unsafe { windows::Win32::System::Threading::TerminateProcess(self.process, 0) };
        let _ = unsafe { CloseHandle(self.process) };
    }
}

pub fn launch(
    executable: &Path,
    session_id: u32,
    user_token: HANDLE,
) -> Result<InteractiveConnection, Box<dyn std::error::Error>> {
    let mut connection_token = [0; 16];
    OsRng.fill_bytes(&mut connection_token);
    let token_hex = hex(&connection_token);
    let suffix = &token_hex[..16];
    let control_name = format!(r"\\.\pipe\RC.{suffix}.c");
    let audio_name = format!(r"\\.\pipe\RC.{suffix}.a");
    let expected_sid = token_sid(user_token)?;
    let security = PipeSecurity::new(&expected_sid)?;
    let control = NamedPipeServer::new(&control_name, security.attributes())?;
    let audio = NamedPipeServer::new(&audio_name, security.attributes())?;

    let account = session_account(session_id)?;
    let task_name = format!("RustConsoleMediaWorker-{suffix}");
    let action = format!(
        "\"{}\" media-worker-named \"{control_name}\" \"{audio_name}\" {token_hex}",
        executable.display()
    );
    let mut task = create_and_run_task(&task_name, &account, &action)?;

    let control = control.connect()?;
    let audio = audio.connect()?;
    let mut process_id = 0;
    let mut audio_process_id = 0;
    // SAFETY: both handles are connected local named-pipe server endpoints.
    unsafe {
        GetNamedPipeClientProcessId(HANDLE(control.as_raw_handle()), &mut process_id)?;
        GetNamedPipeClientProcessId(HANDLE(audio.as_raw_handle()), &mut audio_process_id)?;
    }
    if process_id == 0 || audio_process_id != process_id {
        return Err("interactive worker connected pipes from different processes".into());
    }
    // SAFETY: process_id came from a connected local named-pipe endpoint.
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            false,
            process_id,
        )?
    };
    let mut process_token = HANDLE::default();
    // SAFETY: process is open and process_token is valid output storage.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut process_token)? };
    let actual_sid = token_sid(process_token);
    // SAFETY: OpenProcessToken transferred ownership of this handle.
    let _ = unsafe { CloseHandle(process_token) };
    if actual_sid? != expected_sid {
        // SAFETY: the process was opened with PROCESS_TERMINATE.
        let _ = unsafe { windows::Win32::System::Threading::TerminateProcess(process, 1) };
        // SAFETY: this function has not transferred process ownership yet.
        let _ = unsafe { CloseHandle(process) };
        return Err("interactive worker SID does not match the active console user".into());
    }
    let events = control.try_clone()?;
    task.delete()?;
    Ok(InteractiveConnection {
        command: control,
        events,
        audio_events: audio,
        process,
        process_id,
        connection_token,
    })
}

pub fn connect(
    control_name: &str,
    audio_name: &str,
) -> Result<(File, File, File), Box<dyn std::error::Error>> {
    let control = connect_pipe(control_name)?;
    let events = control.try_clone()?;
    let audio = connect_pipe(audio_name)?;
    Ok((control, events, audio))
}

fn connect_pipe(name: &str) -> Result<File, Box<dyn std::error::Error>> {
    let name = wide(name);
    let started = Instant::now();
    loop {
        // SAFETY: name is terminated UTF-16 and optional pointers are null.
        match unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        } {
            Ok(handle) => {
                // SAFETY: ownership of the new handle transfers to File.
                return Ok(unsafe { File::from_raw_handle(handle.0) });
            }
            Err(error)
                if started.elapsed() < CONNECT_TIMEOUT
                    && (error.code()
                        == windows::core::HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0)
                        || error.code()
                            == windows::core::HRESULT::from_win32(ERROR_PIPE_BUSY.0)) =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

struct NamedPipeServer(HANDLE);

impl NamedPipeServer {
    fn new(name: &str, security: &SECURITY_ATTRIBUTES) -> windows::core::Result<Self> {
        let name = wide(name);
        // SAFETY: name and security remain valid for the complete call.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                0,
                Some(security),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(windows::core::Error::from_thread())
        } else {
            Ok(Self(handle))
        }
    }

    fn connect(self) -> windows::core::Result<File> {
        let started = Instant::now();
        loop {
            // SAFETY: this object exclusively owns a listening pipe handle.
            match unsafe { ConnectNamedPipe(self.0, None) } {
                Ok(()) => break,
                Err(error)
                    if error.code()
                        == windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) =>
                {
                    break;
                }
                Err(error)
                    if error.code()
                        == windows::core::HRESULT::from_win32(ERROR_PIPE_LISTENING.0)
                        && started.elapsed() < CONNECT_TIMEOUT =>
                {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error),
            }
        }
        let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
        // SAFETY: the connected pipe and mode pointer are valid.
        unsafe { SetNamedPipeHandleState(self.0, Some(&mode), None, None)? };
        let handle = self.0;
        std::mem::forget(self);
        // SAFETY: ownership transfers from NamedPipeServer to File once.
        Ok(unsafe { File::from_raw_handle(handle.0) })
    }
}

impl Drop for NamedPipeServer {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the handle until a successful connection.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    fn new(sid: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: SDDL is terminated UTF-16 and descriptor is output storage.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1,
                &mut descriptor,
                None,
            )?;
        }
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())?,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            },
        })
    }

    fn for_authenticated_user() -> Result<Self, Box<dyn std::error::Error>> {
        let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;AU)");
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: SDDL is terminated UTF-16 and descriptor is output storage.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1,
                &mut descriptor,
                None,
            )?;
        }
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())?,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            },
        })
    }

    const fn attributes(&self) -> &SECURITY_ATTRIBUTES {
        &self.attributes
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        // SAFETY: LocalFree releases the descriptor allocated by conversion.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.descriptor.0))) };
    }
}

fn token_sid(token: HANDLE) -> Result<String, Box<dyn std::error::Error>> {
    let mut bytes = 0;
    // SAFETY: the zero-sized call obtains the required size.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut bytes) };
    let mut storage = vec![0_u8; usize::try_from(bytes)?];
    // SAFETY: storage has the reported size and contains TOKEN_USER on success.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(storage.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )?;
        let user = &*storage.as_ptr().cast::<TOKEN_USER>();
        let mut value = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut value)?;
        let sid = value.to_string()?;
        let _ = LocalFree(Some(HLOCAL(value.0.cast())));
        Ok(sid)
    }
}

fn session_account(session_id: u32) -> Result<String, Box<dyn std::error::Error>> {
    let domain = session_string(session_id, WTSDomainName)?;
    let user = session_string(session_id, WTSUserName)?;
    if user.is_empty() {
        return Err("active console session has no user name".into());
    }
    Ok(if domain.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    })
}

fn session_string(
    session_id: u32,
    class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut value = PWSTR::null();
    let mut bytes = 0;
    // SAFETY: output storage is valid and null server means local computer.
    unsafe { WTSQuerySessionInformationW(None, session_id, class, &mut value, &mut bytes)? };
    let result = if value.is_null() || bytes < 2 {
        String::new()
    } else {
        // SAFETY: WTS returned a terminated string in its allocated buffer.
        unsafe { value.to_string()? }
    };
    // SAFETY: WTS allocated this successful query buffer.
    unsafe { WTSFreeMemory(value.0.cast()) };
    Ok(result)
}

fn create_and_run_task(
    task_name: &str,
    account: &str,
    action: &str,
) -> Result<TaskGuard, Box<dyn std::error::Error>> {
    let output = Command::new("schtasks.exe")
        .args([
            "/Create", "/TN", task_name, "/TR", action, "/SC", "ONCE", "/ST", "00:00", "/RU",
            account, "/IT", "/RL", "LIMITED", "/F",
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "create interactive worker task failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let task = TaskGuard(Some(task_name.to_owned()));
    let output = Command::new("schtasks.exe")
        .args(["/Run", "/TN", task_name])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "run interactive worker task failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(task)
}

struct TaskGuard(Option<String>);

impl TaskGuard {
    fn delete(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(name) = self.0.as_deref() else {
            return Ok(());
        };
        let output = Command::new("schtasks.exe")
            .args(["/Delete", "/TN", name, "/F"])
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "delete interactive worker task failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        self.0 = None;
        Ok(())
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if let Some(name) = self.0.as_deref() {
            let _ = Command::new("schtasks.exe")
                .args(["/Delete", "/TN", name, "/F"])
                .output();
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn wide(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}
