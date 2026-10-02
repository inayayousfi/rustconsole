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
const RESTART_MAGIC: u32 = 0x5247_4352;
const PROTOCOL_VERSION: u32 = 4;
const COMMAND_RESTART_CAPTURE: u8 = 1;
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
        request[..4].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
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
        if u32_at(&hello, 0) != HELLO_MAGIC || u32_at(&hello, 4) != PROTOCOL_VERSION {
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
        check_capture_response(&frame, "capture")?;
        Ok(Some(WgcFrame {
            last_present_time: i64_at(&frame, 8),
            accumulated_frames: u32_at(&frame, 16),
            protected_content_masked: i32_at(&frame, 20) != 0,
            capture_acquisition_micros: u64_at(&frame, 28),
            cross_adapter_copy_micros: u64_at(&frame, 36),
            color_conversion_micros: u64_at(&frame, 44),
        }))
    }

    pub fn restart_capture(
        &mut self,
        diagnostics: bool,
        pending_frame: &mut Option<WgcFrame>,
        release_frame: impl FnMut() -> Result<(), Box<dyn std::error::Error>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !self.configured {
            (&self.connection.channel).write_all(&[u8::from(diagnostics)])?;
            self.configured = true;
        }
        restart_capture_exchange(&mut &self.connection.channel, pending_frame, release_frame)
    }
}

fn restart_capture_exchange(
    channel: &mut (impl Read + Write),
    pending_frame: &mut Option<WgcFrame>,
    mut release_frame: impl FnMut() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Each successful FRAME_MAGIC transfers one shared-texture handoff. Even a discarded
    // frame must be acquired and released before the producer can publish another image.
    if pending_frame.is_some() {
        release_frame()?;
        *pending_frame = None;
    }
    channel.write_all(&[COMMAND_RESTART_CAPTURE])?;
    loop {
        let mut response = [0_u8; FRAME_SIZE];
        channel.read_exact(&mut response)?;
        match u32_at(&response, 0) {
            FRAME_MAGIC => {
                check_capture_response(&response, "capture during restart")?;
                release_frame()?;
            }
            RESTART_MAGIC => {
                check_capture_response(&response, "capture restart")?;
                return Ok(());
            }
            _ => return Err("WGC helper returned an invalid capture restart response".into()),
        }
    }
}

fn check_capture_response(
    response: &[u8; FRAME_SIZE],
    operation: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = i32_at(response, 4);
    if result >= 0 {
        return Ok(());
    }
    if result == windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST.0 {
        use rustconsole_protocol::wire::VideoReconfigurationCause;
        let cause = i32::try_from(u32_at(response, 24))
            .ok()
            .and_then(|value| VideoReconfigurationCause::try_from(value).ok())
            .ok_or("WGC helper returned an invalid video reconfiguration cause")?;
        let cause = if cause == VideoReconfigurationCause::Unspecified {
            VideoReconfigurationCause::CaptureEngine
        } else {
            cause
        };
        return Err(Box::new(crate::gpu_encode::VideoReconfigurationRequired {
            cause,
        }));
    }
    Err(format!(
        "WGC helper {operation} failed at {}: {}",
        stage_at(response, 52),
        windows::core::Error::from_hresult(windows::core::HRESULT(result))
    )
    .into())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu_encode::VideoReconfigurationRequired;
    use rustconsole_protocol::wire::VideoReconfigurationCause;
    use windows::Win32::Graphics::Dxgi::DXGI_ERROR_ACCESS_LOST;

    fn response(result: i32, cause: u32) -> [u8; FRAME_SIZE] {
        let mut bytes = [0; FRAME_SIZE];
        bytes[4..8].copy_from_slice(&result.to_le_bytes());
        bytes[24..28].copy_from_slice(&cause.to_le_bytes());
        bytes
    }

    struct RestartChannel {
        responses: std::io::Cursor<Vec<u8>>,
        commands: Vec<u8>,
    }

    impl Read for RestartChannel {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.responses.read(buffer)
        }
    }

    impl Write for RestartChannel {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.commands.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn restart_channel(responses: &[(u32, i32)]) -> RestartChannel {
        RestartChannel {
            responses: std::io::Cursor::new(
                responses
                    .iter()
                    .flat_map(|&(magic, result)| {
                        let mut bytes = response(result, 0);
                        bytes[..4].copy_from_slice(&magic.to_le_bytes());
                        bytes
                    })
                    .collect(),
            ),
            commands: Vec::new(),
        }
    }

    fn pending_frame() -> WgcFrame {
        WgcFrame {
            last_present_time: 1,
            accumulated_frames: 1,
            protected_content_masked: false,
            capture_acquisition_micros: 0,
            cross_adapter_copy_micros: 0,
            color_conversion_micros: 0,
        }
    }

    #[test]
    fn restart_releases_pending_and_in_transit_images_before_resuming_capture() {
        for pending in [false, true] {
            for in_transit in [0, 1, 2] {
                let mut responses = vec![(FRAME_MAGIC, 0); in_transit];
                responses.push((RESTART_MAGIC, 0));
                let mut channel = restart_channel(&responses);
                let mut pending_frame = pending.then(pending_frame);
                let mut outstanding_handoffs = in_transit + usize::from(pending);
                restart_capture_exchange(&mut channel, &mut pending_frame, || {
                    assert!(outstanding_handoffs > 0);
                    outstanding_handoffs -= 1;
                    Ok(())
                })
                .unwrap();
                assert_eq!(
                    outstanding_handoffs, 0,
                    "producer must be able to publish again"
                );
                assert!(pending_frame.is_none());
                assert_eq!(channel.commands, [COMMAND_RESTART_CAPTURE]);
            }
        }
    }

    #[test]
    fn pending_image_release_failure_prevents_restart_and_preserves_ownership() {
        let mut channel = restart_channel(&[(RESTART_MAGIC, 0)]);
        let mut pending = Some(pending_frame());
        let error = restart_capture_exchange(&mut channel, &mut pending, || {
            Err("shared texture release failed".into())
        })
        .unwrap_err();
        assert!(error.to_string().contains("shared texture release failed"));
        assert!(pending.is_some());
        assert!(channel.commands.is_empty());
        assert_eq!(channel.responses.position(), 0);
    }

    #[test]
    fn in_transit_image_release_failure_does_not_report_a_successful_restart() {
        let mut channel = restart_channel(&[(FRAME_MAGIC, 0), (RESTART_MAGIC, 0)]);
        let error = restart_capture_exchange(&mut channel, &mut None, || {
            Err("shared texture release failed".into())
        })
        .unwrap_err();
        assert!(error.to_string().contains("shared texture release failed"));
        assert_eq!(channel.responses.position(), FRAME_SIZE as u64);
    }

    #[test]
    fn restart_preserves_capture_failures_and_rejects_invalid_or_missing_responses() {
        for responses in [
            vec![(FRAME_MAGIC, 0x8000_4005_u32 as i32)],
            vec![(RESTART_MAGIC, 0x8000_4005_u32 as i32)],
            vec![(0, 0)],
            vec![],
        ] {
            let mut channel = restart_channel(&responses);
            restart_capture_exchange(&mut channel, &mut None, || {
                panic!("invalid responses must not release an image")
            })
            .unwrap_err();
        }
    }

    #[test]
    fn capture_and_restart_preserve_every_reconfiguration_cause() {
        for cause in [
            VideoReconfigurationCause::CaptureEngine,
            VideoReconfigurationCause::Dimensions,
            VideoReconfigurationCause::RefreshRate,
            VideoReconfigurationCause::PixelFormat,
            VideoReconfigurationCause::Color,
        ] {
            for operation in ["capture", "capture during restart", "capture restart"] {
                let error = check_capture_response(
                    &response(DXGI_ERROR_ACCESS_LOST.0, cause as u32),
                    operation,
                )
                .unwrap_err();
                assert_eq!(
                    error
                        .downcast_ref::<VideoReconfigurationRequired>()
                        .unwrap()
                        .cause,
                    cause
                );
            }
        }
    }

    #[test]
    fn access_loss_without_a_specific_cause_reconfigures_the_capture_engine() {
        let error =
            check_capture_response(&response(DXGI_ERROR_ACCESS_LOST.0, 0), "capture").unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<VideoReconfigurationRequired>()
                .unwrap()
                .cause,
            VideoReconfigurationCause::CaptureEngine
        );
    }

    #[test]
    fn malformed_reconfiguration_causes_are_not_normal_transitions() {
        for cause in [99, u32::MAX] {
            let error =
                check_capture_response(&response(DXGI_ERROR_ACCESS_LOST.0, cause), "capture")
                    .unwrap_err();
            assert!(
                error
                    .downcast_ref::<VideoReconfigurationRequired>()
                    .is_none()
            );
            assert!(
                error
                    .to_string()
                    .contains("invalid video reconfiguration cause")
            );
        }
    }

    #[test]
    fn other_failures_keep_the_operation_and_native_stage() {
        let mut bytes = response(0x8000_4005_u32 as i32, 1);
        bytes[52..56].copy_from_slice(b"test");
        let error = check_capture_response(&bytes, "capture during restart").unwrap_err();
        assert!(
            error
                .downcast_ref::<VideoReconfigurationRequired>()
                .is_none()
        );
        assert!(
            error
                .to_string()
                .contains("capture during restart failed at test")
        );
    }

    #[test]
    fn successful_capture_response_needs_no_reconfiguration_cause() {
        assert!(check_capture_response(&response(0, u32::MAX), "capture").is_ok());
    }
}
