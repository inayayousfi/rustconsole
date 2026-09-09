use crate::desktop::attach_input_desktop;
use rustconsole_codec_ffmpeg::{
    Av1ColorDescription, Av1EncoderConfiguration, Av1FrameFormat, Av1NvencEncoder,
    EncodedAv1Packet, HardwareDevice,
};
use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_void};
use std::fmt;
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT};
use windows::core::{HRESULT, Interface};

unsafe extern "C" {
    fn rustconsole_gpu_bridge_create(
        bridge: *mut *mut c_void,
        encoder_device: *mut *mut c_void,
        normal_desktop: i32,
        width: *mut u32,
        height: *mut u32,
        refresh_rate: *mut u32,
        capture_engine: *mut u32,
        video_format: *mut u32,
        video_color: *mut u32,
    ) -> i32;
    fn rustconsole_gpu_bridge_capture(
        bridge: *mut c_void,
        timeout_millis: u32,
        encoder_texture: *mut *mut c_void,
        last_present_time: *mut i64,
        accumulated_frames: *mut u32,
        protected_content_masked: *mut i32,
    ) -> i32;
    fn rustconsole_gpu_bridge_open_external(
        bridge: *mut *mut c_void,
        encoder_device: *mut *mut c_void,
        shared_texture: *mut c_void,
        width: u32,
        height: u32,
        refresh_rate: u32,
        video_format: u32,
        video_color: u32,
    ) -> i32;
    fn rustconsole_gpu_bridge_acquire_external(
        bridge: *mut c_void,
        timeout_millis: u32,
        encoder_texture: *mut *mut c_void,
    ) -> i32;
    fn rustconsole_gpu_bridge_release_encoder_texture(bridge: *mut c_void) -> i32;
    fn rustconsole_gpu_bridge_destroy(bridge: *mut c_void);
    fn rustconsole_gpu_bridge_failure_stage() -> *const c_char;
    fn rustconsole_gpu_bridge_reconfiguration_cause(bridge: *mut c_void) -> u32;
}

pub type VideoCaptureConfiguration = crate::worker_protocol::WorkerVideoConfiguration;

pub struct PreparedGpuCapture {
    bridge: GpuBridge,
    device: ID3D11Device,
    configuration: VideoCaptureConfiguration,
}

impl PreparedGpuCapture {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let desktop = attach_input_desktop()?;
        Self::for_desktop(&desktop)
    }

    pub fn for_desktop(desktop: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let normal_desktop = desktop.eq_ignore_ascii_case("Default");
        let (bridge, device, configuration) = if normal_desktop {
            GpuBridge::new_external()
                .map_err(|error| format!("interactive WGC helper initialization failed: {error}"))?
        } else {
            GpuBridge::new(false)
                .map_err(|error| format!("GPU bridge initialization failed: {error}"))?
        };
        Ok(Self {
            bridge,
            device,
            configuration,
        })
    }

    #[must_use]
    pub const fn configuration(&self) -> VideoCaptureConfiguration {
        self.configuration
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoReconfigurationRequired {
    pub cause: rustconsole_protocol::wire::VideoReconfigurationCause,
}

impl fmt::Display for VideoReconfigurationRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "video reconfiguration required: {:?}",
            self.cause
        )
    }
}

impl std::error::Error for VideoReconfigurationRequired {}

pub struct GpuAv1SnapshotEncoder {
    bridge: GpuBridge,
    device: ID3D11Device,
    encoder: Av1NvencEncoder,
    width: u32,
    height: u32,
    frames_per_second: u16,
    bitrate_bits_per_second: u64,
    next_presentation_timestamp: i64,
    pending_metadata: VecDeque<FrameMetadata>,
}

pub struct EncodedSnapshot {
    pub packet: EncodedAv1Packet,
    pub last_present_time: i64,
    pub accumulated_frames: u32,
    pub protected_content_masked: bool,
}

struct FrameMetadata {
    presentation_timestamp: i64,
    last_present_time: i64,
    accumulated_frames: u32,
    protected_content_masked: bool,
}

impl GpuAv1SnapshotEncoder {
    pub fn new(
        frames_per_second: u16,
        bitrate_bits_per_second: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::from_prepared(
            PreparedGpuCapture::new()?,
            frames_per_second,
            bitrate_bits_per_second,
        )
    }

    pub fn from_prepared(
        prepared: PreparedGpuCapture,
        frames_per_second: u16,
        bitrate_bits_per_second: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let PreparedGpuCapture {
            bridge,
            device,
            configuration,
        } = prepared;
        let VideoCaptureConfiguration {
            width,
            height,
            refresh_rate: display_refresh_rate,
            format,
            color,
            ..
        } = configuration;
        if width != 2560 || height != 1440 {
            return Err(format!("GPU proof requires 2560x1440, found {width}x{height}").into());
        }
        if display_refresh_rate < u32::from(frames_per_second) {
            return Err(format!(
                "GPU proof requires {frames_per_second} Hz, display reports {display_refresh_rate} Hz"
            )
            .into());
        }
        let hardware = HardwareDevice::from_d3d11_device(&device)
            .map_err(|error| format!("FFmpeg D3D11 device import failed: {error}"))?;
        let encoder = Av1NvencEncoder::open(
            &hardware,
            Av1EncoderConfiguration {
                width,
                height,
                frames_per_second,
                bitrate_bits_per_second,
                frame_format: match format {
                    crate::worker_protocol::WorkerVideoFormat::Nv12 => Av1FrameFormat::Yuv420Eight,
                    crate::worker_protocol::WorkerVideoFormat::P010 => Av1FrameFormat::Yuv420Ten,
                },
                color_description: match color {
                    crate::worker_protocol::WorkerVideoColor::Bt709Limited => {
                        Av1ColorDescription::Bt709Limited
                    }
                    crate::worker_protocol::WorkerVideoColor::Bt2020PqLimited => {
                        Av1ColorDescription::Bt2020PqLimited
                    }
                },
            },
        )
        .map_err(|error| format!("NVENC AV1 encoder initialization failed: {error}"))?;
        Ok(Self {
            bridge,
            device,
            encoder,
            width,
            height,
            frames_per_second,
            bitrate_bits_per_second,
            next_presentation_timestamp: 0,
            pending_metadata: VecDeque::with_capacity(2),
        })
    }

    pub fn encode_next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<EncodedSnapshot>, Box<dyn std::error::Error>> {
        let capture = self.bridge.capture(timeout);
        if matches!(&capture, Err(error) if error.code == DXGI_ERROR_WAIT_TIMEOUT.0) {
            return Ok(None);
        }
        if matches!(&capture, Err(error) if error.code == DXGI_ERROR_ACCESS_LOST.0) {
            return Err(Box::new(VideoReconfigurationRequired {
                cause: self.bridge.reconfiguration_cause(),
            }));
        }
        let lease = capture.map_err(|error| format!("GPU bridge capture failed: {error}"))?;
        if lease.last_present_time == 0 {
            return Ok(None);
        }

        let presentation_timestamp = self.next_presentation_timestamp;
        self.next_presentation_timestamp = self
            .next_presentation_timestamp
            .checked_add(1)
            .ok_or("NVENC presentation timestamp exhausted")?;
        self.pending_metadata.push_back(FrameMetadata {
            presentation_timestamp,
            last_present_time: lease.last_present_time,
            accumulated_frames: lease.accumulated_frames,
            protected_content_masked: lease.protected_content_masked,
        });
        let packet = self
            .encoder
            .encode_d3d11_texture(&lease.texture, presentation_timestamp)
            .map_err(|error| format!("NVENC AV1 frame submission failed: {error}"))?;
        drop(lease);

        let Some(packet) = packet else {
            if self.pending_metadata.len() > 2 {
                self.rebuild_encoder(self.bitrate_bits_per_second)?;
            }
            return Ok(None);
        };
        let metadata = self
            .pending_metadata
            .pop_front()
            .ok_or("NVENC returned a packet without submitted frame metadata")?;
        if metadata.presentation_timestamp != packet.presentation_timestamp {
            return Err(format!(
                "NVENC returned presentation timestamp {}, expected {}",
                packet.presentation_timestamp, metadata.presentation_timestamp
            )
            .into());
        }
        Ok(Some(EncodedSnapshot {
            packet,
            last_present_time: metadata.last_present_time,
            accumulated_frames: metadata.accumulated_frames,
            protected_content_masked: metadata.protected_content_masked,
        }))
    }

    pub fn set_bitrate(
        &mut self,
        bitrate_bits_per_second: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if bitrate_bits_per_second == self.bitrate_bits_per_second {
            return Ok(());
        }
        self.encoder
            .set_bitrate(bitrate_bits_per_second)
            .map_err(|error| format!("NVENC AV1 bitrate reconfiguration failed: {error}"))?;
        self.bitrate_bits_per_second = bitrate_bits_per_second;
        Ok(())
    }

    pub fn request_keyframe(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.rebuild_encoder(self.bitrate_bits_per_second)
    }

    fn rebuild_encoder(
        &mut self,
        bitrate_bits_per_second: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let hardware = HardwareDevice::from_d3d11_device(&self.device)
            .map_err(|error| format!("FFmpeg D3D11 device import failed: {error}"))?;
        let encoder = Av1NvencEncoder::open(
            &hardware,
            Av1EncoderConfiguration {
                width: self.width,
                height: self.height,
                frames_per_second: self.frames_per_second,
                bitrate_bits_per_second,
                frame_format: match self.bridge.configuration.format {
                    crate::worker_protocol::WorkerVideoFormat::Nv12 => Av1FrameFormat::Yuv420Eight,
                    crate::worker_protocol::WorkerVideoFormat::P010 => Av1FrameFormat::Yuv420Ten,
                },
                color_description: match self.bridge.configuration.color {
                    crate::worker_protocol::WorkerVideoColor::Bt709Limited => {
                        Av1ColorDescription::Bt709Limited
                    }
                    crate::worker_protocol::WorkerVideoColor::Bt2020PqLimited => {
                        Av1ColorDescription::Bt2020PqLimited
                    }
                },
            },
        )
        .map_err(|error| format!("NVENC AV1 encoder reinitialization failed: {error}"))?;
        self.encoder = encoder;
        self.bitrate_bits_per_second = bitrate_bits_per_second;
        self.pending_metadata.clear();
        Ok(())
    }

    pub fn encode_snapshot(
        &mut self,
        timeout: Duration,
    ) -> Result<EncodedSnapshot, Box<dyn std::error::Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.encode_snapshot_once(remaining) {
                Ok(snapshot) => return Ok(snapshot),
                Err(SnapshotAttemptError::AccessLost) if Instant::now() < deadline => {
                    return Err(Box::new(VideoReconfigurationRequired {
                        cause: self.bridge.reconfiguration_cause(),
                    }));
                }
                Err(SnapshotAttemptError::AccessLost) => {
                    return Err("Desktop Duplication access remained lost for 5 seconds".into());
                }
                Err(SnapshotAttemptError::Fatal(error)) => return Err(error),
            }
        }
    }

    fn encode_snapshot_once(
        &mut self,
        timeout: Duration,
    ) -> Result<EncodedSnapshot, SnapshotAttemptError> {
        let lease = match self.bridge.capture(timeout) {
            Ok(lease) => lease,
            Err(error) if error.code == DXGI_ERROR_ACCESS_LOST.0 => {
                return Err(SnapshotAttemptError::AccessLost);
            }
            Err(error) => {
                return Err(SnapshotAttemptError::Fatal(
                    format!("GPU bridge capture failed: {error}").into(),
                ));
            }
        };
        if lease.last_present_time == 0 {
            return Err(SnapshotAttemptError::Fatal(
                "Desktop Duplication returned a pointer-only frame".into(),
            ));
        }
        let packet = self
            .encoder
            .encode_one_d3d11_texture(&lease.texture, lease.last_present_time)
            .map_err(|error| {
                SnapshotAttemptError::Fatal(
                    format!("NVENC AV1 frame submission failed: {error}").into(),
                )
            })?;
        let snapshot = EncodedSnapshot {
            packet,
            last_present_time: lease.last_present_time,
            accumulated_frames: lease.accumulated_frames,
            protected_content_masked: lease.protected_content_masked,
        };
        drop(lease);
        Ok(snapshot)
    }
}

enum SnapshotAttemptError {
    AccessLost,
    Fatal(Box<dyn std::error::Error>),
}

struct GpuBridge {
    raw: NonNull<c_void>,
    configuration: VideoCaptureConfiguration,
    helper: Option<crate::wgc_helper::WgcHelper>,
}

impl GpuBridge {
    fn new(
        normal_desktop: bool,
    ) -> Result<(Self, ID3D11Device, VideoCaptureConfiguration), BridgeError> {
        let mut bridge = ptr::null_mut();
        let mut device = ptr::null_mut();
        let mut width = 0;
        let mut height = 0;
        let mut refresh_rate = 0;
        let mut capture_engine = 0;
        let mut video_format = 0;
        let mut video_color = 0;
        // SAFETY: all outputs are initialized storage and ownership transfers on success.
        let result = unsafe {
            rustconsole_gpu_bridge_create(
                &mut bridge,
                &mut device,
                i32::from(normal_desktop),
                &mut width,
                &mut height,
                &mut refresh_rate,
                &mut capture_engine,
                &mut video_format,
                &mut video_color,
            )
        };
        check_bridge_hresult(result)?;
        let raw = NonNull::new(bridge).ok_or_else(|| BridgeError {
            code: 0x8000_4005_u32 as i32,
            stage: "GPU bridge returned a null object".to_owned(),
        })?;
        // SAFETY: the bridge returned one owned COM reference on success.
        let device = unsafe { ID3D11Device::from_raw(device) };
        let configuration = VideoCaptureConfiguration {
            width,
            height,
            refresh_rate,
            capture_engine: match capture_engine {
                1 => crate::worker_protocol::WorkerCaptureEngine::WindowsGraphicsCapture,
                2 => crate::worker_protocol::WorkerCaptureEngine::DesktopDuplication,
                _ => {
                    return Err(BridgeError::invalid(
                        "GPU bridge returned an invalid capture engine",
                    ));
                }
            },
            format: match video_format {
                1 => crate::worker_protocol::WorkerVideoFormat::Nv12,
                2 => crate::worker_protocol::WorkerVideoFormat::P010,
                _ => {
                    return Err(BridgeError::invalid(
                        "GPU bridge returned an invalid video format",
                    ));
                }
            },
            color: match video_color {
                1 => crate::worker_protocol::WorkerVideoColor::Bt709Limited,
                2 => crate::worker_protocol::WorkerVideoColor::Bt2020PqLimited,
                _ => {
                    return Err(BridgeError::invalid(
                        "GPU bridge returned an invalid video color",
                    ));
                }
            },
        };
        Ok((
            Self {
                raw,
                configuration,
                helper: None,
            },
            device,
            configuration,
        ))
    }

    fn new_external() -> Result<(Self, ID3D11Device, VideoCaptureConfiguration), BridgeError> {
        let helper = crate::wgc_helper::WgcHelper::launch().map_err(|error| BridgeError {
            code: 0x8000_4005_u32 as i32,
            stage: error.to_string(),
        })?;
        let configuration = helper.configuration;
        let mut bridge = ptr::null_mut();
        let mut device = ptr::null_mut();
        // SAFETY: the authenticated helper supplied the duplicated texture handle and all
        // output storage remains valid for the call.
        let result = unsafe {
            rustconsole_gpu_bridge_open_external(
                &mut bridge,
                &mut device,
                helper.shared_texture().0,
                configuration.width,
                configuration.height,
                configuration.refresh_rate,
                match configuration.format {
                    crate::worker_protocol::WorkerVideoFormat::Nv12 => 1,
                    crate::worker_protocol::WorkerVideoFormat::P010 => 2,
                },
                match configuration.color {
                    crate::worker_protocol::WorkerVideoColor::Bt709Limited => 1,
                    crate::worker_protocol::WorkerVideoColor::Bt2020PqLimited => 2,
                },
            )
        };
        check_bridge_hresult(result)?;
        let raw = NonNull::new(bridge)
            .ok_or_else(|| BridgeError::invalid("external GPU bridge returned a null object"))?;
        // SAFETY: the bridge returned one owned COM reference on success.
        let device = unsafe { ID3D11Device::from_raw(device) };
        Ok((
            Self {
                raw,
                configuration,
                helper: Some(helper),
            },
            device,
            configuration,
        ))
    }

    fn capture(&self, timeout: Duration) -> Result<EncoderTextureLease<'_>, BridgeError> {
        let timeout_millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let mut texture = ptr::null_mut();
        let mut last_present_time = 0;
        let mut accumulated_frames = 0;
        let mut protected_content_masked = 0;
        let result = if let Some(helper) = self.helper.as_ref() {
            let frame = helper.next_frame().map_err(|error| BridgeError {
                code: 0x8000_4005_u32 as i32,
                stage: error.to_string(),
            })?;
            last_present_time = frame.last_present_time;
            accumulated_frames = frame.accumulated_frames;
            protected_content_masked = i32::from(frame.protected_content_masked);
            // SAFETY: the bridge owns the opened shared texture and output storage is valid.
            unsafe {
                rustconsole_gpu_bridge_acquire_external(
                    self.raw.as_ptr(),
                    timeout_millis,
                    &mut texture,
                )
            }
        } else {
            // SAFETY: the bridge is live and all output storage remains valid.
            unsafe {
                rustconsole_gpu_bridge_capture(
                    self.raw.as_ptr(),
                    timeout_millis,
                    &mut texture,
                    &mut last_present_time,
                    &mut accumulated_frames,
                    &mut protected_content_masked,
                )
            }
        };
        check_bridge_hresult(result)?;
        // SAFETY: capture returned one owned COM reference on success.
        let texture = unsafe { ID3D11Texture2D::from_raw(texture) };
        Ok(EncoderTextureLease {
            bridge: self,
            texture,
            last_present_time,
            accumulated_frames,
            protected_content_masked: protected_content_masked != 0,
        })
    }

    fn reconfiguration_cause(&self) -> rustconsole_protocol::wire::VideoReconfigurationCause {
        let value = unsafe { rustconsole_gpu_bridge_reconfiguration_cause(self.raw.as_ptr()) };
        rustconsole_protocol::wire::VideoReconfigurationCause::try_from(value as i32)
            .ok()
            .filter(|cause| {
                *cause != rustconsole_protocol::wire::VideoReconfigurationCause::Unspecified
            })
            .unwrap_or(rustconsole_protocol::wire::VideoReconfigurationCause::CaptureEngine)
    }
}

impl Drop for GpuBridge {
    fn drop(&mut self) {
        // SAFETY: this object owns the bridge allocation and destroys it once.
        unsafe { rustconsole_gpu_bridge_destroy(self.raw.as_ptr()) };
    }
}

struct EncoderTextureLease<'a> {
    bridge: &'a GpuBridge,
    texture: ID3D11Texture2D,
    last_present_time: i64,
    accumulated_frames: u32,
    protected_content_masked: bool,
}

impl Drop for EncoderTextureLease<'_> {
    fn drop(&mut self) {
        // SAFETY: a successful capture holds exactly one encoder mutex lease.
        let _ = unsafe { rustconsole_gpu_bridge_release_encoder_texture(self.bridge.raw.as_ptr()) };
    }
}

#[derive(Debug)]
struct BridgeError {
    code: i32,
    stage: String,
}

impl BridgeError {
    fn invalid(stage: &str) -> Self {
        Self {
            code: 0x8000_4005_u32 as i32,
            stage: stage.to_owned(),
        }
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {}",
            self.stage,
            windows::core::Error::from_hresult(HRESULT(self.code))
        )
    }
}

impl std::error::Error for BridgeError {}

fn check_bridge_hresult(result: i32) -> Result<(), BridgeError> {
    if result >= 0 {
        return Ok(());
    }
    // SAFETY: the bridge returns either null or a process-lifetime ASCII string.
    let stage = unsafe {
        let stage = rustconsole_gpu_bridge_failure_stage();
        (!stage.is_null()).then(|| CStr::from_ptr(stage).to_string_lossy().into_owned())
    }
    .unwrap_or_else(|| "unknown operation".to_owned());
    Err(BridgeError {
        code: result,
        stage,
    })
}
