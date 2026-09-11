use rand::{RngCore, rngs::OsRng};
use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
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
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_NOWAIT,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, SetNamedPipeHandleState,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, OpenProcessToken,
    PROCESS_INFORMATION, STARTUPINFOW,
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

// Windows kernel handles are process-wide, and this wrapper has sole ownership.
unsafe impl Send for InteractiveHelperConnection {}

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
    let user_token = user_token(session_id)?;
    let action = format!("\"{}\" \"{pipe_name}\" {token_hex}", executable.display());
    let mut launched = launch_as_user(executable, user_token.get(), &action)?;
    let channel = pipe.connect()?;
    let mut process_id = 0;
    // SAFETY: channel is a connected local named-pipe server endpoint.
    unsafe { GetNamedPipeClientProcessId(HANDLE(channel.as_raw_handle()), &mut process_id)? };
    if process_id == 0 {
        return Err("WGC helper returned an invalid process id".into());
    }
    if process_id != launched.process_id {
        return Err("a different process connected to the WGC helper pipe".into());
    }
    let mut actual_session_id = u32::MAX;
    // SAFETY: process_id identifies the connected helper and output storage is valid.
    unsafe { ProcessIdToSessionId(process_id, &mut actual_session_id)? };
    if actual_session_id != session_id {
        return Err("WGC helper is not running in the active console session".into());
    }
    let process = launched.take_process();
    Ok(InteractiveHelperConnection {
        channel,
        process,
        process_id,
        connection_token,
    })
}

pub fn launch_session_controls(
    executable: &Path,
    session_id: u32,
    user_token: HANDLE,
) -> Result<InteractiveHelperConnection, Box<dyn std::error::Error>> {
    let mut connection_token = [0; 16];
    OsRng.fill_bytes(&mut connection_token);
    let token_hex = hex(&connection_token);
    let suffix = &token_hex[..16];
    let pipe_name = format!(r"\\.\pipe\RC.{suffix}.s");
    let expected_sid = token_sid(user_token)?;
    let security = PipeSecurity::new(&expected_sid)?;
    let pipe = NamedPipeServer::new(&pipe_name, security.attributes())?;
    let action = format!(
        "\"{}\" session-controls \"{pipe_name}\" {token_hex}",
        executable.display()
    );
    let mut launched = launch_as_user(executable, user_token, &action)?;
    let channel = pipe.connect()?;
    let mut process_id = 0;
    // SAFETY: channel is a connected local named-pipe server endpoint.
    unsafe { GetNamedPipeClientProcessId(HANDLE(channel.as_raw_handle()), &mut process_id)? };
    if process_id == 0 {
        return Err("session controls returned an invalid process id".into());
    }
    if process_id != launched.process_id {
        return Err("a different process connected to the session controls pipe".into());
    }
    let process = launched.process();
    let mut actual_session_id = u32::MAX;
    // SAFETY: output storage is valid.
    unsafe { ProcessIdToSessionId(process_id, &mut actual_session_id)? };
    if actual_session_id != session_id {
        return Err("session controls are not running in the active console session".into());
    }
    let mut process_token = HANDLE::default();
    // SAFETY: process and output storage are valid.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut process_token)? };
    let actual_sid = token_sid(process_token);
    let _ = unsafe { CloseHandle(process_token) };
    if actual_sid? != expected_sid {
        return Err("session controls SID does not match the active console user".into());
    }
    let process = launched.take_process();
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

    let action = format!(
        "\"{}\" media-worker-named \"{control_name}\" \"{audio_name}\" {token_hex}",
        executable.display()
    );
    let mut launched = launch_as_user(executable, user_token, &action)?;

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
    if process_id != launched.process_id {
        return Err("a different process connected to the interactive worker pipes".into());
    }
    let process = launched.process();
    let mut actual_session_id = u32::MAX;
    // SAFETY: process_id identifies the connected worker and output storage is valid.
    unsafe { ProcessIdToSessionId(process_id, &mut actual_session_id)? };
    if actual_session_id != session_id {
        return Err("interactive worker is not running in the active console session".into());
    }
    let mut process_token = HANDLE::default();
    // SAFETY: process is open and process_token is valid output storage.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut process_token)? };
    let actual_sid = token_sid(process_token);
    // SAFETY: OpenProcessToken transferred ownership of this handle.
    let _ = unsafe { CloseHandle(process_token) };
    if actual_sid? != expected_sid {
        return Err("interactive worker SID does not match the active console user".into());
    }
    let events = control.try_clone()?;
    let process = launched.take_process();
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

pub(crate) fn connect_pipe(name: &str) -> Result<File, Box<dyn std::error::Error>> {
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

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn get(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this guard owns the handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn user_token(session_id: u32) -> Result<OwnedHandle, Box<dyn std::error::Error>> {
    let mut token = HANDLE::default();
    // SAFETY: token is valid output storage.
    unsafe { WTSQueryUserToken(session_id, &mut token)? };
    Ok(OwnedHandle(token))
}

struct EnvironmentBlock(*mut c_void);

impl Drop for EnvironmentBlock {
    fn drop(&mut self) {
        // SAFETY: CreateEnvironmentBlock allocated this block.
        let _ = unsafe { DestroyEnvironmentBlock(self.0) };
    }
}

struct LaunchedProcess {
    process: Option<HANDLE>,
    process_id: u32,
}

impl LaunchedProcess {
    fn process(&self) -> HANDLE {
        self.process.expect("launched process handle missing")
    }

    fn take_process(&mut self) -> HANDLE {
        self.process
            .take()
            .expect("launched process handle missing")
    }
}

impl Drop for LaunchedProcess {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            // SAFETY: the process remains owned until it is transferred to the connection.
            let _ = unsafe { windows::Win32::System::Threading::TerminateProcess(process, 1) };
            let _ = unsafe { CloseHandle(process) };
        }
    }
}

fn launch_as_user(
    executable: &Path,
    token: HANDLE,
    action: &str,
) -> Result<LaunchedProcess, Box<dyn std::error::Error>> {
    let mut environment = std::ptr::null_mut();
    // SAFETY: environment is valid output storage and token remains live.
    unsafe { CreateEnvironmentBlock(&mut environment, Some(token), false)? };
    let environment = EnvironmentBlock(environment);
    let executable_wide = wide(executable.as_os_str().to_string_lossy().as_ref());
    let mut command_line = wide(action);
    let current_directory = executable
        .parent()
        .ok_or("interactive executable has no parent directory")?;
    let current_directory = wide(current_directory.as_os_str().to_string_lossy().as_ref());
    let mut desktop = wide("winsta0\\default");
    let startup = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>())?,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: all strings, the environment block, startup data, and output remain live.
    unsafe {
        CreateProcessAsUserW(
            Some(token),
            PCWSTR(executable_wide.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            Some(environment.0.cast_const()),
            PCWSTR(current_directory.as_ptr()),
            &startup,
            &mut process,
        )?;
    }
    // SAFETY: the thread handle is not needed after process creation.
    let _ = unsafe { CloseHandle(process.hThread) };
    Ok(LaunchedProcess {
        process: Some(process.hProcess),
        process_id: process.dwProcessId,
    })
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
