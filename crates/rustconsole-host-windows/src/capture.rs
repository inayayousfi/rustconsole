use crate::desktop::attach_input_desktop;
use rustconsole_host_core::DesktopCapture;
use rustconsole_media::{MediaTimestampMicros, VideoFormat, VideoFrame};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::{
    DISPLAY_DEVICE_PRIMARY_DEVICE, DISPLAY_DEVICEW, EnumDisplayDevicesW,
};
use windows::core::{Error as WindowsError, Interface, PCWSTR};

pub struct DesktopDuplicationCapture {
    device: Option<ID3D11Device>,
    context: Option<ID3D11DeviceContext>,
    duplication: Option<IDXGIOutputDuplication>,
    format: VideoFormat,
    dxgi_format: DXGI_FORMAT,
    display_name: String,
    desktop_name: String,
    acquire_timeout_millis: u32,
    next_sequence: u64,
    started_at: Instant,
    frame_outstanding: Arc<AtomicBool>,
}

impl DesktopDuplicationCapture {
    pub fn new(acquire_timeout: Duration) -> Result<Self, DesktopDuplicationError> {
        let desktop_name = attach_input_desktop()?;
        let primary_name = primary_display_name()?;
        let output = Self::attached_outputs()?
            .into_iter()
            .find(|output| output.device_name.eq_ignore_ascii_case(&primary_name))
            .ok_or(DesktopDuplicationError::PrimaryOutputNotFound(primary_name))?;
        Self::for_output_on_desktop(&output, acquire_timeout, desktop_name)
    }

    pub fn attached_outputs() -> Result<Vec<DesktopOutput>, DesktopDuplicationError> {
        // SAFETY: CreateDXGIFactory1 initializes and returns an owned COM interface.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        attached_outputs(&factory)
    }

    pub fn for_output(
        output: &DesktopOutput,
        acquire_timeout: Duration,
    ) -> Result<Self, DesktopDuplicationError> {
        let desktop_name = attach_input_desktop()?;
        Self::for_output_on_desktop(output, acquire_timeout, desktop_name)
    }

    fn for_output_on_desktop(
        output: &DesktopOutput,
        acquire_timeout: Duration,
        desktop_name: String,
    ) -> Result<Self, DesktopDuplicationError> {
        let acquire_timeout_millis = u32::try_from(acquire_timeout.as_millis()).unwrap_or(u32::MAX);
        let resources = create_duplication_resources(output)?;
        Ok(Self {
            device: Some(resources.device),
            context: Some(resources.context),
            duplication: Some(resources.duplication),
            format: resources.format,
            dxgi_format: resources.dxgi_format,
            display_name: resources.display_name,
            desktop_name,
            acquire_timeout_millis,
            next_sequence: 0,
            started_at: Instant::now(),
            frame_outstanding: Arc::new(AtomicBool::new(false)),
        })
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    #[must_use]
    pub fn desktop_name(&self) -> &str {
        &self.desktop_name
    }

    #[must_use]
    pub const fn dxgi_format(&self) -> i32 {
        self.dxgi_format.0
    }

    pub fn read_bgra8(
        &self,
        frame: &DesktopTextureFrame,
    ) -> Result<CpuBgraFrame, DesktopDuplicationError> {
        let device = self
            .device
            .as_ref()
            .ok_or(DesktopDuplicationError::MissingDevice)?;
        let context = self
            .context
            .as_ref()
            .ok_or(DesktopDuplicationError::MissingDeviceContext)?;
        let mut description = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: description is a valid output and the captured texture is alive.
        unsafe { frame.texture.GetDesc(&mut description) };
        if description.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(DesktopDuplicationError::UnsupportedReadbackFormat(
                description.Format.0,
            ));
        }

        description.Usage = D3D11_USAGE_STAGING;
        description.BindFlags = 0;
        description.CPUAccessFlags = u32::try_from(D3D11_CPU_ACCESS_READ.0).unwrap_or(0);
        description.MiscFlags = 0;
        let mut staging = None;
        // SAFETY: description is copied from the source texture and adjusted to
        // create a CPU-readable staging resource with no initial data.
        unsafe {
            device.CreateTexture2D(&description, None, Some(&mut staging))?;
        }
        let staging = staging.ok_or(DesktopDuplicationError::MissingStagingTexture)?;
        // SAFETY: both resources have matching dimensions and format.
        unsafe { context.CopyResource(&staging, &frame.texture) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging was created with CPU read access and mapped is valid output storage.
        unsafe {
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let _mapping = TextureMapping {
            context,
            texture: &staging,
        };

        let row_bytes = description
            .Width
            .checked_mul(4)
            .ok_or(DesktopDuplicationError::ReadbackSizeOverflow)?;
        if mapped.RowPitch < row_bytes {
            return Err(DesktopDuplicationError::InvalidReadbackPitch {
                row_pitch: mapped.RowPitch,
                row_bytes,
            });
        }
        let pixel_count = usize::try_from(row_bytes)
            .ok()
            .and_then(|row| {
                usize::try_from(description.Height)
                    .ok()
                    .and_then(|height| row.checked_mul(height))
            })
            .ok_or(DesktopDuplicationError::ReadbackSizeOverflow)?;
        let mut pixels = Vec::with_capacity(pixel_count);
        for row in 0..description.Height {
            let offset = usize::try_from(row)
                .ok()
                .and_then(|row| {
                    usize::try_from(mapped.RowPitch)
                        .ok()
                        .and_then(|pitch| row.checked_mul(pitch))
                })
                .ok_or(DesktopDuplicationError::ReadbackSizeOverflow)?;
            // SAFETY: Map exposes at least RowPitch bytes for every texture row;
            // offset selects one row and row_bytes excludes any driver padding.
            let source = unsafe {
                std::slice::from_raw_parts(
                    mapped.pData.cast::<u8>().add(offset),
                    usize::try_from(row_bytes).unwrap_or(0),
                )
            };
            pixels.extend_from_slice(source);
        }

        Ok(CpuBgraFrame {
            width: description.Width,
            height: description.Height,
            pixels,
        })
    }

    fn acquire_frame(
        &mut self,
    ) -> Result<VideoFrame<DesktopTextureFrame>, DesktopDuplicationError> {
        if self.frame_outstanding.load(Ordering::Acquire) {
            return Err(DesktopDuplicationError::FrameOutstanding);
        }

        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: both output pointers remain valid for the call, and no frame is
        // currently held for this duplication object.
        let duplication = self
            .duplication
            .as_ref()
            .ok_or(DesktopDuplicationError::MissingDuplication)?;
        if let Err(error) = unsafe {
            duplication.AcquireNextFrame(
                self.acquire_timeout_millis,
                &mut frame_info,
                &mut resource,
            )
        } {
            return Err(classify_acquire_error(error));
        }

        self.frame_outstanding.store(true, Ordering::Release);
        let lease = FrameLease {
            duplication: duplication.clone(),
            outstanding: Arc::clone(&self.frame_outstanding),
        };
        let texture = resource
            .ok_or(DesktopDuplicationError::MissingDesktopResource)?
            .cast::<ID3D11Texture2D>()?;
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(DesktopDuplicationError::SequenceExhausted)?;
        let captured_at = MediaTimestampMicros(
            u64::try_from(self.started_at.elapsed().as_micros()).unwrap_or(u64::MAX),
        );

        Ok(VideoFrame {
            sequence,
            captured_at,
            format: self.format,
            frame: DesktopTextureFrame {
                texture,
                last_present_time: frame_info.LastPresentTime,
                accumulated_frames: frame_info.AccumulatedFrames,
                protected_content_masked: frame_info.ProtectedContentMaskedOut.as_bool(),
                _lease: lease,
            },
        })
    }
}

impl DesktopCapture for DesktopDuplicationCapture {
    type Frame = DesktopTextureFrame;
    type Error = DesktopDuplicationError;

    fn format(&self) -> VideoFormat {
        self.format
    }

    fn next_frame(&mut self) -> Result<VideoFrame<Self::Frame>, Self::Error> {
        self.acquire_frame()
    }

    fn reset(&mut self) -> Result<(), Self::Error> {
        if self.frame_outstanding.load(Ordering::Acquire) {
            return Err(DesktopDuplicationError::FrameOutstanding);
        }

        self.duplication = None;
        self.context = None;
        self.device = None;
        let desktop_name = attach_input_desktop()?;
        let primary_name = primary_display_name()?;
        let output = Self::attached_outputs()?
            .into_iter()
            .find(|output| output.device_name.eq_ignore_ascii_case(&primary_name))
            .ok_or(DesktopDuplicationError::PrimaryOutputNotFound(primary_name))?;
        let resources = create_duplication_resources(&output)?;
        self.device = Some(resources.device);
        self.context = Some(resources.context);
        self.duplication = Some(resources.duplication);
        self.format = resources.format;
        self.dxgi_format = resources.dxgi_format;
        self.display_name = resources.display_name;
        self.desktop_name = desktop_name;
        Ok(())
    }
}

pub struct DesktopOutput {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
    adapter_name: String,
    device_name: String,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl DesktopOutput {
    #[must_use]
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    #[must_use]
    pub const fn coordinates(&self) -> (i32, i32, i32, i32) {
        (self.left, self.top, self.right, self.bottom)
    }
}

pub struct CpuBgraFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub struct DesktopTextureFrame {
    texture: ID3D11Texture2D,
    last_present_time: i64,
    accumulated_frames: u32,
    protected_content_masked: bool,
    _lease: FrameLease,
}

impl DesktopTextureFrame {
    #[must_use]
    pub const fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    #[must_use]
    pub const fn last_present_time(&self) -> i64 {
        self.last_present_time
    }

    #[must_use]
    pub const fn accumulated_frames(&self) -> u32 {
        self.accumulated_frames
    }

    #[must_use]
    pub const fn protected_content_masked(&self) -> bool {
        self.protected_content_masked
    }
}

struct FrameLease {
    duplication: IDXGIOutputDuplication,
    outstanding: Arc<AtomicBool>,
}

struct TextureMapping<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}

impl Drop for TextureMapping<'_> {
    fn drop(&mut self) {
        // SAFETY: this guard is created only after Map succeeds for subresource 0.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

impl Drop for FrameLease {
    fn drop(&mut self) {
        // SAFETY: this lease exists only after a successful AcquireNextFrame,
        // and exactly one lease is constructed for that acquisition.
        let _ = unsafe { self.duplication.ReleaseFrame() };
        self.outstanding.store(false, Ordering::Release);
    }
}

struct DuplicationResources {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    format: VideoFormat,
    dxgi_format: DXGI_FORMAT,
    display_name: String,
}

fn create_duplication_resources(
    output: &DesktopOutput,
) -> Result<DuplicationResources, DesktopDuplicationError> {
    let mut device = None;
    let mut context = None;
    let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    // SAFETY: output slots are valid Options, the adapter is alive for the
    // call, and the requested feature-level slice is valid.
    unsafe {
        D3D11CreateDevice(
            &output.adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or(DesktopDuplicationError::MissingDevice)?;
    let context = context.ok_or(DesktopDuplicationError::MissingDeviceContext)?;
    let dxgi_output: IDXGIOutput1 = output.output.cast()?;
    // SAFETY: the D3D11 device was created for the adapter that owns output.
    let duplication =
        unsafe { dxgi_output.DuplicateOutput(&device) }.map_err(classify_duplicate_output_error)?;
    // SAFETY: GetDesc writes no caller-provided memory and duplication is valid.
    let description = unsafe { duplication.GetDesc() };
    let refresh_rate = description.ModeDesc.RefreshRate;
    let rounded_refresh_rate = refresh_rate
        .Numerator
        .saturating_add(refresh_rate.Denominator / 2)
        .checked_div(refresh_rate.Denominator)
        .unwrap_or(0);
    let frames_per_second = u16::try_from(rounded_refresh_rate).unwrap_or(u16::MAX);

    Ok(DuplicationResources {
        device,
        context,
        duplication,
        format: VideoFormat {
            width: description.ModeDesc.Width,
            height: description.ModeDesc.Height,
            frames_per_second,
        },
        dxgi_format: description.ModeDesc.Format,
        display_name: output.device_name.clone(),
    })
}

fn attached_outputs(
    factory: &IDXGIFactory1,
) -> Result<Vec<DesktopOutput>, DesktopDuplicationError> {
    let mut outputs = Vec::new();
    for adapter_index in 0.. {
        // SAFETY: enumeration returns a new owned interface or DXGI_ERROR_NOT_FOUND.
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error.into()),
        };
        // SAFETY: adapter is a valid owned interface.
        let adapter_description = unsafe { adapter.GetDesc1()? };
        let adapter_name = utf16_name(&adapter_description.Description);

        for output_index in 0.. {
            // SAFETY: enumeration returns a new owned interface or DXGI_ERROR_NOT_FOUND.
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            // SAFETY: output is a valid owned interface.
            let description = unsafe { output.GetDesc()? };
            if !description.AttachedToDesktop.as_bool() {
                continue;
            }

            let name = utf16_name(&description.DeviceName);
            outputs.push(DesktopOutput {
                adapter: adapter.clone(),
                output,
                adapter_name: adapter_name.clone(),
                device_name: name,
                left: description.DesktopCoordinates.left,
                top: description.DesktopCoordinates.top,
                right: description.DesktopCoordinates.right,
                bottom: description.DesktopCoordinates.bottom,
            });
        }
    }

    if outputs.is_empty() {
        Err(DesktopDuplicationError::NoAttachedOutput)
    } else {
        Ok(outputs)
    }
}

fn primary_display_name() -> Result<String, DesktopDuplicationError> {
    for device_index in 0.. {
        let mut device = DISPLAY_DEVICEW {
            cb: u32::try_from(std::mem::size_of::<DISPLAY_DEVICEW>()).unwrap_or(u32::MAX),
            ..Default::default()
        };
        // SAFETY: a null device name enumerates display adapters, and device is
        // initialized with the structure size required by EnumDisplayDevicesW.
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), device_index, &mut device, 0) }.as_bool() {
            break;
        }
        if device.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0 != 0 {
            return Ok(utf16_name(&device.DeviceName));
        }
    }

    Err(DesktopDuplicationError::NoPrimaryDisplay)
}

fn utf16_name(value: &[u16]) -> String {
    let length = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..length])
}

fn classify_acquire_error(error: WindowsError) -> DesktopDuplicationError {
    match error.code() {
        code if code == DXGI_ERROR_WAIT_TIMEOUT => DesktopDuplicationError::Timeout,
        code if code == DXGI_ERROR_ACCESS_LOST => DesktopDuplicationError::AccessLost,
        code if code == DXGI_ERROR_ACCESS_DENIED || code == E_ACCESSDENIED => {
            DesktopDuplicationError::AccessDenied
        }
        code if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => {
            DesktopDuplicationError::TemporarilyUnavailable
        }
        _ => DesktopDuplicationError::Windows(error),
    }
}

fn classify_duplicate_output_error(error: WindowsError) -> DesktopDuplicationError {
    if error.code() == DXGI_ERROR_INVALID_CALL {
        DesktopDuplicationError::DesktopNotReady
    } else {
        classify_acquire_error(error)
    }
}

#[derive(Debug)]
pub enum DesktopDuplicationError {
    NoAttachedOutput,
    NoPrimaryDisplay,
    PrimaryOutputNotFound(String),
    MissingDevice,
    MissingDeviceContext,
    MissingDesktopResource,
    MissingDuplication,
    MissingStagingTexture,
    FrameOutstanding,
    SequenceExhausted,
    Timeout,
    AccessLost,
    AccessDenied,
    DesktopNotReady,
    TemporarilyUnavailable,
    UnsupportedReadbackFormat(i32),
    InvalidReadbackPitch { row_pitch: u32, row_bytes: u32 },
    ReadbackSizeOverflow,
    Windows(WindowsError),
}

impl From<WindowsError> for DesktopDuplicationError {
    fn from(error: WindowsError) -> Self {
        classify_acquire_error(error)
    }
}

impl fmt::Display for DesktopDuplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAttachedOutput => formatter.write_str("no attached desktop output was found"),
            Self::NoPrimaryDisplay => {
                formatter.write_str("Windows reported no primary desktop display")
            }
            Self::PrimaryOutputNotFound(name) => {
                write!(
                    formatter,
                    "primary display {name} has no attached DXGI output"
                )
            }
            Self::MissingDevice => formatter.write_str("D3D11 returned no device"),
            Self::MissingDeviceContext => formatter.write_str("D3D11 returned no device context"),
            Self::MissingDesktopResource => {
                formatter.write_str("Desktop Duplication returned no desktop resource")
            }
            Self::MissingDuplication => {
                formatter.write_str("desktop duplication must be reset before capture continues")
            }
            Self::MissingStagingTexture => formatter.write_str("D3D11 returned no staging texture"),
            Self::FrameOutstanding => {
                formatter.write_str("the previous desktop frame is still in use")
            }
            Self::SequenceExhausted => formatter.write_str("desktop frame sequence exhausted"),
            Self::Timeout => formatter.write_str("desktop frame acquisition timed out"),
            Self::AccessLost => formatter.write_str("desktop duplication access was lost"),
            Self::AccessDenied => formatter.write_str("desktop duplication access was denied"),
            Self::DesktopNotReady => {
                formatter.write_str("desktop output is not ready for duplication")
            }
            Self::TemporarilyUnavailable => {
                formatter.write_str("all Desktop Duplication slots are currently in use")
            }
            Self::UnsupportedReadbackFormat(format) => {
                write!(
                    formatter,
                    "desktop texture has unsupported DXGI format {format}"
                )
            }
            Self::InvalidReadbackPitch {
                row_pitch,
                row_bytes,
            } => write!(
                formatter,
                "mapped desktop row pitch {row_pitch} is smaller than {row_bytes} bytes"
            ),
            Self::ReadbackSizeOverflow => formatter.write_str("desktop readback size overflowed"),
            Self::Windows(error) => write!(formatter, "Desktop Duplication failed: {error}"),
        }
    }
}

impl std::error::Error for DesktopDuplicationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Windows(error) => Some(error),
            _ => None,
        }
    }
}
