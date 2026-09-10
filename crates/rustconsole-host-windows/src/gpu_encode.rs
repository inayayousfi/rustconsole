use crate::desktop::attach_input_desktop;
use rustconsole_codec_ffmpeg::{
    Av1ColorDescription, Av1D3d11Decoder, Av1EncoderConfiguration, Av1FrameFormat, Av1NvencEncoder,
    EncodedAv1Packet, HardwareDevice,
};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{CStr, c_char, c_void};
use std::fmt;
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010};
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
        capture_acquisition_micros: *mut u64,
        cross_adapter_copy_micros: *mut u64,
        color_conversion_micros: *mut u64,
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
    quality: Option<VideoQualityDiagnostics>,
}

pub struct EncodedSnapshot {
    pub packet: EncodedAv1Packet,
    pub last_present_time: i64,
    pub accumulated_frames: u32,
    pub protected_content_masked: bool,
    pub mirror_decode_micros: u64,
    pub capture_acquisition_micros: u64,
    pub cross_adapter_copy_micros: u64,
    pub color_conversion_micros: u64,
    pub encoder_call_micros: u64,
    pub quality: Option<VideoQualitySample>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoQualitySample {
    pub presentation_timestamp: i64,
    pub source_readback_micros: u64,
    pub mirror_decode_micros: u64,
    pub decoded_readback_micros: u64,
    pub scoring_micros: u64,
    pub readback_bytes: u64,
    pub luma_psnr_millidecibels: u64,
    pub luma_mean_absolute_error_ppm: u64,
}

struct FrameMetadata {
    presentation_timestamp: i64,
    last_present_time: i64,
    accumulated_frames: u32,
    protected_content_masked: bool,
    quality_source: Option<QualitySource>,
    capture_acquisition_micros: u64,
    cross_adapter_copy_micros: u64,
    color_conversion_micros: u64,
    encoder_call_micros: u64,
}

struct QualitySource {
    width: u32,
    height: u32,
    bit_depth: u16,
    samples: Vec<u16>,
    readback_micros: u64,
    readback_bytes: u64,
}

struct VideoQualityDiagnostics {
    decoder: Av1D3d11Decoder,
    next_sample: Instant,
    sources: BTreeMap<i64, QualitySource>,
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
            quality: None,
        })
    }

    pub fn enable_quality_diagnostics(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let hardware = HardwareDevice::from_d3d11_device(&self.device)
            .map_err(|error| format!("FFmpeg D3D11 device import failed: {error}"))?;
        let decoder = Av1D3d11Decoder::open(&hardware).map_err(|error| {
            format!("D3D11VA AV1 mirror decoder initialization failed: {error}")
        })?;
        self.quality = Some(VideoQualityDiagnostics {
            decoder,
            next_sample: Instant::now(),
            sources: BTreeMap::new(),
        });
        Ok(())
    }

    pub fn encode_next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<EncodedSnapshot>, Box<dyn std::error::Error>> {
        let diagnostics = self.quality.is_some();
        let capture = self.bridge.capture(timeout, diagnostics);
        if matches!(&capture, Err(error) if error.code == DXGI_ERROR_WAIT_TIMEOUT.0) {
            return Ok(None);
        }
        if matches!(&capture, Err(error) if error.code == DXGI_ERROR_ACCESS_LOST.0) {
            drop(capture);
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
        let quality_source = match self.quality.as_mut() {
            Some(quality) if Instant::now() >= quality.next_sample => {
                quality.next_sample = Instant::now() + Duration::from_millis(200);
                Some(read_texture_luma(&self.device, &lease.texture)?)
            }
            _ => None,
        };
        self.pending_metadata.push_back(FrameMetadata {
            presentation_timestamp,
            last_present_time: lease.last_present_time,
            accumulated_frames: lease.accumulated_frames,
            protected_content_masked: lease.protected_content_masked,
            quality_source,
            capture_acquisition_micros: lease.capture_acquisition_micros,
            cross_adapter_copy_micros: lease.cross_adapter_copy_micros,
            color_conversion_micros: lease.color_conversion_micros,
            encoder_call_micros: 0,
        });
        let encoder_started = diagnostics.then(Instant::now);
        let packet = self
            .encoder
            .encode_d3d11_texture(&lease.texture, presentation_timestamp)
            .map_err(|error| format!("NVENC AV1 frame submission failed: {error}"))?;
        if let Some(started) = encoder_started
            && let Some(metadata) = self.pending_metadata.back_mut()
        {
            metadata.encoder_call_micros = started.elapsed().as_micros() as u64;
        }
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
        let (mirror_decode_micros, quality) = match self.quality.as_mut() {
            Some(quality) => quality.observe(&packet, metadata.quality_source)?,
            None => (0, None),
        };
        Ok(Some(EncodedSnapshot {
            packet,
            last_present_time: metadata.last_present_time,
            accumulated_frames: metadata.accumulated_frames,
            protected_content_masked: metadata.protected_content_masked,
            mirror_decode_micros,
            capture_acquisition_micros: metadata.capture_acquisition_micros,
            cross_adapter_copy_micros: metadata.cross_adapter_copy_micros,
            color_conversion_micros: metadata.color_conversion_micros,
            encoder_call_micros: metadata.encoder_call_micros,
            quality,
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
        if self.quality.is_some() {
            self.enable_quality_diagnostics()?;
        }
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
        let lease = match self.bridge.capture(timeout, false) {
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
            mirror_decode_micros: 0,
            capture_acquisition_micros: 0,
            cross_adapter_copy_micros: 0,
            color_conversion_micros: 0,
            encoder_call_micros: 0,
            quality: None,
        };
        drop(lease);
        Ok(snapshot)
    }
}

impl VideoQualityDiagnostics {
    fn observe(
        &mut self,
        packet: &EncodedAv1Packet,
        source: Option<QualitySource>,
    ) -> Result<(u64, Option<VideoQualitySample>), Box<dyn std::error::Error>> {
        if let Some(source) = source {
            self.sources.insert(packet.presentation_timestamp, source);
            while self.sources.len() > 8 {
                self.sources.pop_first();
            }
        }

        let decode_started = Instant::now();
        let decoded = self
            .decoder
            .decode_packet(&packet.data, packet.presentation_timestamp)
            .map_err(|error| format!("D3D11VA AV1 mirror decode failed: {error}"))?;
        let mirror_decode_micros = decode_started.elapsed().as_micros() as u64;
        let Some(decoded) = decoded else {
            return Ok((mirror_decode_micros, None));
        };
        let presentation_timestamp = decoded.presentation_timestamp();
        let Some(source) = self.sources.remove(&presentation_timestamp) else {
            return Ok((mirror_decode_micros, None));
        };

        let readback_started = Instant::now();
        let decoded = decoded
            .download_luma()
            .map_err(|error| format!("D3D11VA decoded-frame readback failed: {error}"))?;
        let decoded_readback_micros = readback_started.elapsed().as_micros() as u64;
        if decoded.width != source.width
            || decoded.height != source.height
            || decoded.bit_depth != source.bit_depth
            || decoded.samples.len() != source.samples.len()
        {
            return Err("source and mirror-decoded luma formats differ".into());
        }

        let scoring_started = Instant::now();
        let mut squared_error = 0_u128;
        let mut absolute_error = 0_u128;
        for (&source, &decoded) in source.samples.iter().zip(&decoded.samples) {
            let difference = i64::from(source) - i64::from(decoded);
            squared_error += u128::from(difference.unsigned_abs()).pow(2);
            absolute_error += u128::from(difference.unsigned_abs());
        }
        let sample_count = source.samples.len() as f64;
        let maximum = f64::from((1_u16 << source.bit_depth) - 1);
        let mean_squared_error = squared_error as f64 / sample_count;
        let psnr = if mean_squared_error == 0.0 {
            100.0
        } else {
            10.0 * (maximum * maximum / mean_squared_error).log10()
        };
        let mean_absolute_error = absolute_error as f64 / sample_count;
        let scoring_micros = scoring_started.elapsed().as_micros() as u64;
        let decoded_readback_bytes = u64::from(decoded.width)
            * u64::from(decoded.height)
            * if decoded.bit_depth == 8 { 3 } else { 6 }
            / 2;
        Ok((
            mirror_decode_micros,
            Some(VideoQualitySample {
                presentation_timestamp,
                source_readback_micros: source.readback_micros,
                mirror_decode_micros,
                decoded_readback_micros,
                scoring_micros,
                readback_bytes: source.readback_bytes.saturating_add(decoded_readback_bytes),
                luma_psnr_millidecibels: (psnr * 1_000.0).round().clamp(0.0, u64::MAX as f64)
                    as u64,
                luma_mean_absolute_error_ppm: (mean_absolute_error * 1_000_000.0 / maximum)
                    .round()
                    .clamp(0.0, u64::MAX as f64)
                    as u64,
            }),
        ))
    }
}

fn read_texture_luma(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<QualitySource, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut description = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: description is valid output and texture remains live for the call.
    unsafe { texture.GetDesc(&mut description) };
    let (bit_depth, row_bytes, readback_bytes) = if description.Format == DXGI_FORMAT_NV12 {
        (
            8,
            usize::try_from(description.Width)?,
            u64::from(description.Width) * u64::from(description.Height) * 3 / 2,
        )
    } else if description.Format == DXGI_FORMAT_P010 {
        (
            10,
            usize::try_from(description.Width)? * 2,
            u64::from(description.Width) * u64::from(description.Height) * 3,
        )
    } else {
        return Err(format!(
            "quality readback requires NV12 or P010, found DXGI format {}",
            description.Format.0
        )
        .into());
    };
    description.Usage = D3D11_USAGE_STAGING;
    description.BindFlags = 0;
    description.CPUAccessFlags = u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0);
    description.MiscFlags = 0;
    let mut staging = None;
    // SAFETY: description is copied from the source and adjusted for CPU readback.
    unsafe { device.CreateTexture2D(&description, None, Some(&mut staging))? };
    let staging = staging.ok_or("D3D11 quality readback created no staging texture")?;
    // SAFETY: the immediate context belongs to this device.
    let context = unsafe { device.GetImmediateContext()? };
    // SAFETY: source and staging resources have matching dimensions and format.
    unsafe { context.CopyResource(&staging, texture) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: staging permits CPU reads and mapped is valid output storage.
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
    let _mapping = TextureMapping {
        context: &context,
        texture: &staging,
    };
    let row_pitch = usize::try_from(mapped.RowPitch)?;
    if row_pitch < row_bytes {
        return Err("D3D11 quality readback row pitch is too small".into());
    }
    let width = usize::try_from(description.Width)?;
    let height = usize::try_from(description.Height)?;
    let mut samples = Vec::with_capacity(
        width
            .checked_mul(height)
            .ok_or("D3D11 quality readback size overflowed")?,
    );
    for row in 0..height {
        // SAFETY: Map exposes row_pitch bytes for each luma row.
        let source = unsafe { mapped.pData.cast::<u8>().add(row * row_pitch) };
        if bit_depth == 8 {
            // SAFETY: width bytes are available in the validated row.
            samples.extend(
                unsafe { std::slice::from_raw_parts(source, width) }
                    .iter()
                    .map(|&value| u16::from(value)),
            );
        } else {
            for column in 0..width {
                // SAFETY: P010 stores one little-endian u16 luma sample per column.
                let sample = unsafe { source.add(column * 2) };
                samples.push(u16::from_le_bytes(unsafe { [*sample, *sample.add(1)] }) >> 6);
            }
        }
    }
    Ok(QualitySource {
        width: description.Width,
        height: description.Height,
        bit_depth,
        samples,
        readback_micros: started.elapsed().as_micros() as u64,
        readback_bytes,
    })
}

struct TextureMapping<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}

impl Drop for TextureMapping<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard is created immediately after a successful Map.
        unsafe { self.context.Unmap(self.texture, 0) };
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

    fn capture(
        &mut self,
        timeout: Duration,
        diagnostics: bool,
    ) -> Result<EncoderTextureLease<'_>, BridgeError> {
        let timeout_millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let mut texture = ptr::null_mut();
        let mut last_present_time = 0;
        let mut accumulated_frames = 0;
        let mut protected_content_masked = 0;
        let mut capture_acquisition_micros = 0;
        let mut cross_adapter_copy_micros = 0;
        let mut color_conversion_micros = 0;
        let result = if let Some(helper) = self.helper.as_mut() {
            let frame = helper
                .next_frame(diagnostics)
                .map_err(|error| BridgeError {
                    code: 0x8000_4005_u32 as i32,
                    stage: error.to_string(),
                })?;
            last_present_time = frame.last_present_time;
            accumulated_frames = frame.accumulated_frames;
            protected_content_masked = i32::from(frame.protected_content_masked);
            capture_acquisition_micros = frame.capture_acquisition_micros;
            cross_adapter_copy_micros = frame.cross_adapter_copy_micros;
            color_conversion_micros = frame.color_conversion_micros;
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
                    if diagnostics {
                        &mut capture_acquisition_micros
                    } else {
                        ptr::null_mut()
                    },
                    if diagnostics {
                        &mut cross_adapter_copy_micros
                    } else {
                        ptr::null_mut()
                    },
                    if diagnostics {
                        &mut color_conversion_micros
                    } else {
                        ptr::null_mut()
                    },
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
            capture_acquisition_micros,
            cross_adapter_copy_micros,
            color_conversion_micros,
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
    capture_acquisition_micros: u64,
    cross_adapter_copy_micros: u64,
    color_conversion_micros: u64,
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
