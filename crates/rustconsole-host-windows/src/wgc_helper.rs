use crate::interactive_worker::{self, InteractiveHelperConnection};
use crate::worker_protocol::{
    WorkerCaptureEngine, WorkerVideoColor, WorkerVideoConfiguration, WorkerVideoFormat,
};
use rustconsole_protocol::display::{AdapterId, DisplayId};
use std::fs;
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows::Win32::System::Pipes::PeekNamedPipe;
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, WaitForSingleObject,
};

const HELLO_MAGIC: u32 = 0x4847_4352;
const FRAME_MAGIC: u32 = 0x4647_4352;
const HELLO_SIZE: usize = 252;
const FRAME_SIZE: usize = 244;
const HELPER_BYTES: &[u8] = include_bytes!(env!("RUSTCONSOLE_WGC_HELPER"));

pub struct WgcHelper {
    connection: InteractiveHelperConnection,
    shared_texture: HANDLE,
    pub configuration: WorkerVideoConfiguration,
    configured: bool,
}

pub struct WgcFrame {
    pub last_present_time: i64,
    pub accumulated_frames: u32,
    pub protected_content_masked: bool,
    pub capture_acquisition_micros: u64,
    pub cross_adapter_copy_micros: u64,
    pub color_conversion_micros: u64,
}

impl WgcHelper {
    pub fn launch(
        display_id: &DisplayId,
        processing_adapter: AdapterId,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let helper_path = install_helper()?;
        // SAFETY: the function returns a value and does not retain pointers.
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        if session_id == u32::MAX {
            return Err("there is no active Windows console session for WGC".into());
        }
        let connection = interactive_worker::launch_helper(&helper_path, session_id)?;

        let name = display_id.as_str().encode_utf16().collect::<Vec<_>>();
        if name.len() >= 128 {
            return Err("display identity exceeds helper bound".into());
        }
        let mut request = vec![0_u8; 268];
        request[..4].copy_from_slice(&2_u32.to_le_bytes());
        request[4..12].copy_from_slice(&processing_adapter.0.to_le_bytes());
        for (index, value) in name.into_iter().enumerate() {
            request[12 + index * 2..14 + index * 2].copy_from_slice(&value.to_le_bytes());
        }
        (&connection.channel).write_all(&request)?;

        let mut hello = [0_u8; HELLO_SIZE];
        read_exact(&connection.channel, &mut hello).map_err(|error| {
            let mut exit_code = 0;
            // SAFETY: the connection owns a live process handle throughout the diagnostic query.
            unsafe {
                WaitForSingleObject(connection.process, 1000);
                let _ = GetExitCodeProcess(connection.process, &mut exit_code);
            }
            format!("WGC helper hello failed: {error}; exit code {exit_code:#010x}")
        })?;
        if u32_at(&hello, 0) != HELLO_MAGIC || u32_at(&hello, 4) != 2 {
            return Err("WGC helper returned an invalid protocol header".into());
        }
        if hello[8..24] != connection.connection_token {
            return Err("WGC helper returned an invalid connection token".into());
        }
        let result = i32_at(&hello, 56);
        if result < 0 {
            return Err(format!(
                "WGC helper initialization failed at {}: {}",
                stage_at(&hello, 60),
                windows::core::Error::from_hresult(windows::core::HRESULT(result))
            )
            .into());
        }
        let source_handle = u64_at(&hello, 24);
        if source_handle == 0 {
            return Err("WGC helper returned a null shared texture handle".into());
        }
        let mut shared_texture = HANDLE::default();
        // SAFETY: source handle belongs to the authenticated helper process; the destination
        // storage and current process pseudo-handle remain valid for the complete call.
        unsafe {
            DuplicateHandle(
                connection.process,
                HANDLE(source_handle as *mut _),
                GetCurrentProcess(),
                &mut shared_texture,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )?;
        }
        let configuration = WorkerVideoConfiguration {
            width: u32_at(&hello, 32),
            height: u32_at(&hello, 36),
            refresh_rate: u32_at(&hello, 40),
            capture_engine: match u32_at(&hello, 44) {
                1 => WorkerCaptureEngine::WindowsGraphicsCapture,
                _ => return Err("WGC helper returned an invalid capture engine".into()),
            },
            format: match u32_at(&hello, 48) {
                1 => WorkerVideoFormat::Nv12,
                2 => WorkerVideoFormat::P010,
                _ => return Err("WGC helper returned an invalid video format".into()),
            },
            color: match u32_at(&hello, 52) {
                1 => WorkerVideoColor::Bt709Limited,
                2 => WorkerVideoColor::Bt2020PqLimited,
                _ => return Err("WGC helper returned an invalid video color".into()),
            },
        };
        Ok(Self {
            connection,
            shared_texture,
            configuration,
            configured: false,
        })
    }

    pub fn shared_texture(&self) -> HANDLE {
        self.shared_texture
    }

    pub fn next_frame(
        &mut self,
        timeout: Duration,
        diagnostics: bool,
    ) -> Result<Option<WgcFrame>, Box<dyn std::error::Error>> {
        if !self.configured {
            (&self.connection.channel).write_all(&[u8::from(diagnostics)])?;
            self.configured = true;
        }
        if !wait_for_frame(&self.connection.channel, timeout)? {
            return Ok(None);
        }
        let mut frame = [0_u8; FRAME_SIZE];
        read_exact(&self.connection.channel, &mut frame)?;
        if u32_at(&frame, 0) != FRAME_MAGIC {
            return Err("WGC helper returned an invalid frame header".into());
        }
        let result = i32_at(&frame, 4);
        if result < 0 {
            return Err(format!(
                "WGC helper capture failed at {}: {}",
                stage_at(&frame, 52),
                windows::core::Error::from_hresult(windows::core::HRESULT(result))
            )
            .into());
        }
        Ok(Some(WgcFrame {
            last_present_time: i64_at(&frame, 8),
            accumulated_frames: u32_at(&frame, 16),
            protected_content_masked: i32_at(&frame, 20) != 0,
            capture_acquisition_micros: u64_at(&frame, 28),
            cross_adapter_copy_micros: u64_at(&frame, 36),
            color_conversion_micros: u64_at(&frame, 44),
        }))
    }
}

impl Drop for WgcHelper {
    fn drop(&mut self) {
        // SAFETY: DuplicateHandle transferred ownership of this handle.
        let _ = unsafe { CloseHandle(self.shared_texture) };
    }
}

fn install_helper() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let current = std::env::current_exe()?;
    let directory = current
        .parent()
        .ok_or("host executable has no parent directory")?;
    let path = directory.join("rustconsole-wgc-helper.exe");
    let current = fs::read(&path).unwrap_or_default();
    if current != HELPER_BYTES {
        let temporary = directory.join("rustconsole-wgc-helper.exe.new");
        fs::write(&temporary, HELPER_BYTES)?;
        fs::rename(temporary, &path)?;
    }
    Ok(path)
}

fn read_exact(file: &fs::File, buffer: &mut [u8]) -> std::io::Result<()> {
    let mut file = file;
    file.read_exact(buffer)
}

fn wait_for_frame(file: &fs::File, timeout: Duration) -> std::io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut available = 0;
        // SAFETY: the helper channel is an open named-pipe handle and available is valid output
        // storage.
        unsafe {
            PeekNamedPipe(
                HANDLE(file.as_raw_handle()),
                None,
                0,
                None,
                Some(&mut available),
                None,
            )?;
        }
        if available as usize >= FRAME_SIZE {
            return Ok(true);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(1).min(deadline - now));
    }
}

fn u32_at(value: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        value[offset..offset + 4]
            .try_into()
            .expect("fixed protocol field"),
    )
}

fn i32_at(value: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(
        value[offset..offset + 4]
            .try_into()
            .expect("fixed protocol field"),
    )
}

fn u64_at(value: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        value[offset..offset + 8]
            .try_into()
            .expect("fixed protocol field"),
    )
}

fn i64_at(value: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(
        value[offset..offset + 8]
            .try_into()
            .expect("fixed protocol field"),
    )
}

fn stage_at(value: &[u8], offset: usize) -> String {
    let bytes = &value[offset..];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
