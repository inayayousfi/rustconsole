//! Owned FFmpeg codec and hardware-context integration.

pub mod opus;

use ffmpeg_next::ffi;
use std::ffi::{CStr, CString, NulError};
use std::fmt;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;
use std::ptr::{self, NonNull};
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
#[cfg(windows)]
use windows::core::Interface;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardwareDeviceType {
    VaApi,
    D3d11Va,
}

impl HardwareDeviceType {
    const fn ffi_type(self) -> ffi::AVHWDeviceType {
        match self {
            Self::VaApi => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            Self::D3d11Va => ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
        }
    }
}

impl fmt::Display for HardwareDeviceType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::VaApi => "VA-API",
            Self::D3d11Va => "D3D11VA",
        })
    }
}

#[derive(Debug)]
pub enum HardwareDeviceError {
    Initialization(ffmpeg_next::Error),
    InvalidDeviceName(NulError),
    Open {
        device_type: HardwareDeviceType,
        code: i32,
    },
    #[cfg(windows)]
    InitializeExternalD3d11(i32),
}

impl fmt::Display for HardwareDeviceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Initialization(error) => {
                write!(formatter, "FFmpeg initialization failed: {error}")
            }
            Self::InvalidDeviceName(error) => {
                write!(formatter, "invalid FFmpeg device name: {error}")
            }
            Self::Open { device_type, code } => write!(
                formatter,
                "FFmpeg could not open the requested {device_type} hardware device: {}",
                ffmpeg_next::Error::from(*code)
            ),
            #[cfg(windows)]
            Self::InitializeExternalD3d11(code) => write!(
                formatter,
                "FFmpeg could not initialize the supplied D3D11 device: {}",
                ffmpeg_next::Error::from(*code)
            ),
        }
    }
}

impl std::error::Error for HardwareDeviceError {}

pub struct HardwareDevice {
    context: NonNull<ffi::AVBufferRef>,
    device_type: HardwareDeviceType,
}

impl HardwareDevice {
    pub fn open(
        device_type: HardwareDeviceType,
        device_name: Option<&str>,
    ) -> Result<Self, HardwareDeviceError> {
        ffmpeg_next::init().map_err(HardwareDeviceError::Initialization)?;
        let device_name = device_name
            .map(CString::new)
            .transpose()
            .map_err(HardwareDeviceError::InvalidDeviceName)?;
        let mut context = ptr::null_mut();
        // SAFETY: context is valid output storage, the optional device string
        // remains alive for the call, and no options dictionary is supplied.
        let result = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut context,
                device_type.ffi_type(),
                device_name
                    .as_ref()
                    .map_or(ptr::null(), |name| name.as_ptr()),
                ptr::null_mut(),
                0,
            )
        };
        if result < 0 {
            return Err(HardwareDeviceError::Open {
                device_type,
                code: result,
            });
        }
        let context = NonNull::new(context).ok_or(HardwareDeviceError::Open {
            device_type,
            code: ffi::AVERROR_UNKNOWN,
        })?;
        Ok(Self {
            context,
            device_type,
        })
    }

    #[cfg(windows)]
    pub fn from_d3d11_device(device: &ID3D11Device) -> Result<Self, HardwareDeviceError> {
        ffmpeg_next::init().map_err(HardwareDeviceError::Initialization)?;
        // SAFETY: FFmpeg allocates a D3D11VA device context and its public
        // hardware context is writable until av_hwdevice_ctx_init succeeds.
        let context = NonNull::new(unsafe {
            ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA)
        })
        .ok_or(HardwareDeviceError::InitializeExternalD3d11(
            ffi::AVERROR_UNKNOWN,
        ))?;
        // Transfer one COM reference to FFmpeg, which releases it when the
        // hardware-device buffer is destroyed.
        let owned_device = device.clone();
        let raw_device = owned_device.as_raw();
        std::mem::forget(owned_device);
        // SAFETY: context data points to AVHWDeviceContext and hwctx points to
        // AVD3D11VADeviceContext for the allocated device type.
        unsafe {
            let public = (*context.as_ptr()).data.cast::<ffi::AVHWDeviceContext>();
            let d3d11 = (*public).hwctx.cast::<ffi::AVD3D11VADeviceContext>();
            (*d3d11).device = raw_device.cast();
        }
        // SAFETY: the mandatory D3D11 device field is initialized above.
        let result = unsafe { ffi::av_hwdevice_ctx_init(context.as_ptr()) };
        if result < 0 {
            let mut context = context.as_ptr();
            // SAFETY: this releases the allocated context and transferred COM reference.
            unsafe { ffi::av_buffer_unref(&mut context) };
            return Err(HardwareDeviceError::InitializeExternalD3d11(result));
        }
        Ok(Self {
            context,
            device_type: HardwareDeviceType::D3d11Va,
        })
    }

    #[must_use]
    pub const fn device_type(&self) -> HardwareDeviceType {
        self.device_type
    }

    fn reference(&self) -> Option<NonNull<ffi::AVBufferRef>> {
        // SAFETY: context is a live AVBufferRef for the lifetime of self.
        NonNull::new(unsafe { ffi::av_buffer_ref(self.context.as_ptr()) })
    }
}

impl Drop for HardwareDevice {
    fn drop(&mut self) {
        let mut context = self.context.as_ptr();
        // SAFETY: this object owns one non-null AVBufferRef and unreferences it once.
        unsafe { ffi::av_buffer_unref(&mut context) };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Av1FrameFormat {
    Yuv420Eight,
    Yuv420Ten,
    Yuv422Eight,
    Yuv422Ten,
    Yuv444Eight,
    Yuv444Ten,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Av1ColorDescription {
    Bt709Limited,
    Bt2020PqLimited,
}

impl Av1FrameFormat {
    const fn software_pixel_format(self) -> ffi::AVPixelFormat {
        match self {
            Self::Yuv420Eight => ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            Self::Yuv420Ten => ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
            Self::Yuv422Eight => ffi::AVPixelFormat::AV_PIX_FMT_NV16,
            Self::Yuv422Ten => ffi::AVPixelFormat::AV_PIX_FMT_P210LE,
            Self::Yuv444Eight => ffi::AVPixelFormat::AV_PIX_FMT_YUV444P,
            Self::Yuv444Ten => ffi::AVPixelFormat::AV_PIX_FMT_YUV444P16LE,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Av1EncoderConfiguration {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u16,
    pub bitrate_bits_per_second: u64,
    pub frame_format: Av1FrameFormat,
    pub color_description: Av1ColorDescription,
}

#[derive(Debug)]
pub enum Av1CodecError {
    Initialization(ffmpeg_next::Error),
    InvalidConfiguration(&'static str),
    DecoderNotFound,
    EncoderNotFound,
    WrongHardwareDevice {
        expected: HardwareDeviceType,
        actual: HardwareDeviceType,
    },
    UnsupportedHardwareConfiguration {
        codec: &'static str,
        device_type: HardwareDeviceType,
    },
    AllocateCodecContext(&'static str),
    ReferenceHardwareDevice,
    AllocateHardwareFrames,
    InitializeHardwareFrames(i32),
    Open {
        codec: &'static str,
        code: i32,
    },
    EncoderOption {
        name: &'static str,
        code: i32,
    },
    AllocateFrame,
    AllocatePacket,
    AllocatePacketPayload(i32),
    HardwareFrame(i32),
    SendFrame(i32),
    SendPacket(i32),
    ReceivePacket(i32),
    ReceiveFrame(i32),
    MapFrame(i32),
    TransferFrame(i32),
    InvalidHardwareFrame(&'static str),
}

impl fmt::Display for Av1CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Initialization(error) => {
                write!(formatter, "FFmpeg initialization failed: {error}")
            }
            Self::InvalidConfiguration(reason) => {
                write!(formatter, "invalid AV1 encoder configuration: {reason}")
            }
            Self::DecoderNotFound => formatter.write_str("FFmpeg AV1 decoder was not found"),
            Self::EncoderNotFound => formatter.write_str("FFmpeg av1_nvenc encoder was not found"),
            Self::WrongHardwareDevice { expected, actual } => write!(
                formatter,
                "AV1 codec requires a {expected} hardware device, not {actual}"
            ),
            Self::UnsupportedHardwareConfiguration { codec, device_type } => write!(
                formatter,
                "FFmpeg codec {codec} does not declare the required {device_type} hardware configuration"
            ),
            Self::AllocateCodecContext(codec) => {
                write!(
                    formatter,
                    "FFmpeg could not allocate a {codec} codec context"
                )
            }
            Self::ReferenceHardwareDevice => {
                formatter.write_str("FFmpeg could not reference the hardware device")
            }
            Self::AllocateHardwareFrames => {
                formatter.write_str("FFmpeg could not allocate a hardware-frame context")
            }
            Self::InitializeHardwareFrames(code) => write!(
                formatter,
                "FFmpeg could not initialize the hardware-frame context: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::Open { codec, code } => write!(
                formatter,
                "FFmpeg could not open {codec}: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::EncoderOption { name, code } => write!(
                formatter,
                "FFmpeg rejected the {name} encoder option: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::AllocateFrame => formatter.write_str("FFmpeg could not allocate a video frame"),
            Self::AllocatePacket => formatter.write_str("FFmpeg could not allocate a packet"),
            Self::AllocatePacketPayload(code) => write!(
                formatter,
                "FFmpeg could not allocate a packet payload: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::HardwareFrame(code) => write!(
                formatter,
                "FFmpeg could not allocate a hardware frame: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::SendFrame(code) => write!(
                formatter,
                "FFmpeg could not submit an encoder frame: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::SendPacket(code) => write!(
                formatter,
                "FFmpeg could not submit a decoder packet: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::ReceivePacket(code) => write!(
                formatter,
                "FFmpeg could not receive an encoded packet: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::ReceiveFrame(code) => write!(
                formatter,
                "FFmpeg could not receive a decoded frame: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::MapFrame(code) => write!(
                formatter,
                "FFmpeg could not map a decoded frame to DRM PRIME: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::TransferFrame(code) => write!(
                formatter,
                "FFmpeg could not transfer a decoded frame: {}",
                ffmpeg_next::Error::from(*code)
            ),
            Self::InvalidHardwareFrame(reason) => {
                write!(formatter, "invalid hardware frame: {reason}")
            }
        }
    }
}

impl std::error::Error for Av1CodecError {}

struct CodecContext(NonNull<ffi::AVCodecContext>);

impl CodecContext {
    fn allocate(
        codec: *const ffi::AVCodec,
        codec_name: &'static str,
    ) -> Result<Self, Av1CodecError> {
        // SAFETY: codec points to FFmpeg's process-lifetime codec descriptor.
        NonNull::new(unsafe { ffi::avcodec_alloc_context3(codec) })
            .map(Self)
            .ok_or(Av1CodecError::AllocateCodecContext(codec_name))
    }
}

struct Frame(NonNull<ffi::AVFrame>);

impl Frame {
    fn allocate() -> Result<Self, Av1CodecError> {
        // SAFETY: FFmpeg returns a newly allocated frame or null.
        NonNull::new(unsafe { ffi::av_frame_alloc() })
            .map(Self)
            .ok_or(Av1CodecError::AllocateFrame)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let mut frame = self.0.as_ptr();
        // SAFETY: this object owns one frame and frees it once.
        unsafe { ffi::av_frame_free(&mut frame) };
    }
}

struct Packet(NonNull<ffi::AVPacket>);

impl Packet {
    fn allocate() -> Result<Self, Av1CodecError> {
        // SAFETY: FFmpeg returns a newly allocated packet or null.
        NonNull::new(unsafe { ffi::av_packet_alloc() })
            .map(Self)
            .ok_or(Av1CodecError::AllocatePacket)
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        let mut packet = self.0.as_ptr();
        // SAFETY: this object owns one packet and frees it once.
        unsafe { ffi::av_packet_free(&mut packet) };
    }
}

pub struct DecodedAv1Frame {
    frame: Frame,
}

pub struct DecodedNv12Frame {
    pub width: u32,
    pub height: u32,
    pub y_plane: Vec<u8>,
    pub uv_plane: Vec<u8>,
}

pub struct DecodedLumaFrame {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u16,
    pub samples: Vec<u16>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaBufObject {
    pub fd: RawFd,
    pub size: usize,
    pub format_modifier: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaBufPlane {
    pub object_index: usize,
    pub offset: isize,
    pub pitch: isize,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmaBufLayer {
    pub format: u32,
    pub planes: Vec<DmaBufPlane>,
}

#[cfg(target_os = "linux")]
pub struct MappedDmaBufFrame {
    _frame: Frame,
    width: u32,
    height: u32,
    objects: Vec<DmaBufObject>,
    layers: Vec<DmaBufLayer>,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaBufFrameFormat {
    Nv12,
    P010,
}

// FFmpeg frames and their ref-counted buffers may move between threads when
// ownership moves with them and no API accesses them concurrently.
#[cfg(target_os = "linux")]
unsafe impl Send for MappedDmaBufFrame {}

#[cfg(target_os = "linux")]
impl MappedDmaBufFrame {
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub fn objects(&self) -> &[DmaBufObject] {
        &self.objects
    }

    #[must_use]
    pub fn layers(&self) -> &[DmaBufLayer] {
        &self.layers
    }

    #[must_use]
    pub fn frame_format(&self) -> Option<DmaBufFrameFormat> {
        const DRM_FORMAT_R8: u32 = u32::from_le_bytes(*b"R8  ");
        const DRM_FORMAT_GR88: u32 = u32::from_le_bytes(*b"GR88");
        const DRM_FORMAT_R16: u32 = u32::from_le_bytes(*b"R16 ");
        const DRM_FORMAT_GR32: u32 = u32::from_le_bytes(*b"GR32");
        match self.layers.as_slice() {
            [y, uv] if y.format == DRM_FORMAT_R8 && uv.format == DRM_FORMAT_GR88 => {
                Some(DmaBufFrameFormat::Nv12)
            }
            [y, uv] if y.format == DRM_FORMAT_R16 && uv.format == DRM_FORMAT_GR32 => {
                Some(DmaBufFrameFormat::P010)
            }
            _ => None,
        }
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct AvDrmObjectDescriptor {
    fd: i32,
    size: usize,
    format_modifier: u64,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
struct AvDrmPlaneDescriptor {
    object_index: i32,
    offset: isize,
    pitch: isize,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct AvDrmLayerDescriptor {
    format: u32,
    nb_planes: i32,
    planes: [AvDrmPlaneDescriptor; 4],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct AvDrmFrameDescriptor {
    nb_objects: i32,
    objects: [AvDrmObjectDescriptor; 4],
    nb_layers: i32,
    layers: [AvDrmLayerDescriptor; 4],
}

impl DecodedAv1Frame {
    #[must_use]
    pub fn presentation_timestamp(&self) -> i64 {
        // SAFETY: frame remains owned and the decoder initialized its timestamp.
        unsafe { (*self.frame.0.as_ptr()).pts }
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        // SAFETY: frame remains owned and its dimensions were validated on receipt.
        unsafe { (*self.frame.0.as_ptr()).width as u32 }
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        // SAFETY: frame remains owned and its dimensions were validated on receipt.
        unsafe { (*self.frame.0.as_ptr()).height as u32 }
    }

    #[must_use]
    pub fn color_description(&self) -> Option<Av1ColorDescription> {
        // SAFETY: the decoder initialized these public fields before returning the frame.
        let frame = unsafe { &*self.frame.0.as_ptr() };
        if frame.color_range != ffi::AVColorRange::AVCOL_RANGE_MPEG {
            return None;
        }
        match (frame.color_primaries, frame.color_trc, frame.colorspace) {
            (
                ffi::AVColorPrimaries::AVCOL_PRI_BT709,
                ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709,
                ffi::AVColorSpace::AVCOL_SPC_BT709,
            ) => Some(Av1ColorDescription::Bt709Limited),
            (
                ffi::AVColorPrimaries::AVCOL_PRI_BT2020,
                ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084,
                ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL,
            ) => Some(Av1ColorDescription::Bt2020PqLimited),
            _ => None,
        }
    }

    pub fn download_nv12(&self) -> Result<DecodedNv12Frame, Av1CodecError> {
        let software = Frame::allocate()?;
        // SAFETY: both frames are live; FFmpeg allocates a suitable software
        // destination and copies from the hardware frame.
        let result =
            unsafe { ffi::av_hwframe_transfer_data(software.0.as_ptr(), self.frame.0.as_ptr(), 0) };
        if result < 0 {
            return Err(Av1CodecError::TransferFrame(result));
        }
        // SAFETY: transfer succeeded and initialized the destination frame.
        let frame = unsafe { &*software.0.as_ptr() };
        if frame.format != ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32 {
            return Err(Av1CodecError::InvalidHardwareFrame(
                "decoded software format is not NV12",
            ));
        }
        let width = usize::try_from(frame.width)
            .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative decoded width"))?;
        let height = usize::try_from(frame.height)
            .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative decoded height"))?;
        let y_plane = copy_plane(frame.data[0], frame.linesize[0], width, height)?;
        let uv_plane = copy_plane(frame.data[1], frame.linesize[1], width, height / 2)?;
        Ok(DecodedNv12Frame {
            width: width as u32,
            height: height as u32,
            y_plane,
            uv_plane,
        })
    }

    pub fn download_luma_samples(
        &self,
        coordinates: &[(u32, u32)],
    ) -> Result<(Vec<u16>, u16, u64), Av1CodecError> {
        let software = Frame::allocate()?;
        // SAFETY: both frames are live; FFmpeg transfers the complete hardware frame.
        let result =
            unsafe { ffi::av_hwframe_transfer_data(software.0.as_ptr(), self.frame.0.as_ptr(), 0) };
        if result < 0 {
            return Err(Av1CodecError::TransferFrame(result));
        }
        // SAFETY: transfer succeeded and initialized the destination frame.
        let frame = unsafe { &*software.0.as_ptr() };
        let width = u32::try_from(frame.width)
            .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative decoded width"))?;
        let height = u32::try_from(frame.height)
            .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative decoded height"))?;
        let stride = usize::try_from(frame.linesize[0])
            .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative decoded luma stride"))?;
        if coordinates.iter().any(|&(x, y)| x >= width || y >= height) {
            return Err(Av1CodecError::InvalidHardwareFrame(
                "luma sample is outside the decoded frame",
            ));
        }
        match frame.format {
            value if value == ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32 => {
                if stride < width as usize {
                    return Err(Av1CodecError::InvalidHardwareFrame(
                        "decoded NV12 luma stride is too small",
                    ));
                }
                // SAFETY: the validated coordinates address bytes in the transferred Y plane.
                let samples = coordinates
                    .iter()
                    .map(|&(x, y)| unsafe {
                        u16::from(*frame.data[0].add(y as usize * stride + x as usize))
                    })
                    .collect();
                let bytes = u64::from(width) * u64::from(height) * 3 / 2;
                Ok((samples, 8, bytes))
            }
            value if value == ffi::AVPixelFormat::AV_PIX_FMT_P010LE as i32 => {
                if stride < width as usize * 2 {
                    return Err(Av1CodecError::InvalidHardwareFrame(
                        "decoded P010 luma stride is too small",
                    ));
                }
                // SAFETY: the validated coordinates address two-byte samples in the transferred Y plane.
                let samples = coordinates
                    .iter()
                    .map(|&(x, y)| {
                        let pointer =
                            unsafe { frame.data[0].add(y as usize * stride + x as usize * 2) };
                        u16::from_le_bytes(unsafe { [*pointer, *pointer.add(1)] }) >> 6
                    })
                    .collect();
                let bytes = u64::from(width) * u64::from(height) * 3;
                Ok((samples, 10, bytes))
            }
            _ => Err(Av1CodecError::InvalidHardwareFrame(
                "decoded software format is neither NV12 nor P010LE",
            )),
        }
    }

    pub fn download_luma(&self) -> Result<DecodedLumaFrame, Av1CodecError> {
        let software = Frame::allocate()?;
        // SAFETY: both frames are live; FFmpeg allocates and fills a software frame.
        let result =
            unsafe { ffi::av_hwframe_transfer_data(software.0.as_ptr(), self.frame.0.as_ptr(), 0) };
        if result < 0 {
            return Err(Av1CodecError::TransferFrame(result));
        }
        // SAFETY: transfer succeeded and initialized the destination frame.
        let frame = unsafe { &*software.0.as_ptr() };
        let width = usize::try_from(frame.width)
            .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative decoded width"))?;
        let height = usize::try_from(frame.height)
            .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative decoded height"))?;
        let stride = usize::try_from(frame.linesize[0])
            .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative decoded luma stride"))?;
        let (bit_depth, row_bytes) = match frame.format {
            value if value == ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32 => (8, width),
            value if value == ffi::AVPixelFormat::AV_PIX_FMT_P010LE as i32 => (10, width * 2),
            _ => {
                return Err(Av1CodecError::InvalidHardwareFrame(
                    "decoded software format is neither NV12 nor P010LE",
                ));
            }
        };
        if stride < row_bytes {
            return Err(Av1CodecError::InvalidHardwareFrame(
                "decoded luma stride is too small",
            ));
        }
        let mut samples = Vec::with_capacity(width.checked_mul(height).ok_or(
            Av1CodecError::InvalidHardwareFrame("decoded luma size overflowed"),
        )?);
        for row in 0..height {
            // SAFETY: the validated stride and dimensions keep each row in the frame plane.
            let source = unsafe { frame.data[0].add(row * stride) };
            if bit_depth == 8 {
                // SAFETY: row_bytes bytes are available in this row.
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
        Ok(DecodedLumaFrame {
            width: width as u32,
            height: height as u32,
            bit_depth,
            samples,
        })
    }

    #[cfg(target_os = "linux")]
    pub fn map_dma_buf(&self) -> Result<MappedDmaBufFrame, Av1CodecError> {
        let mapped = Frame::allocate()?;
        // SAFETY: mapped is exclusively owned and setting the requested output
        // format before av_hwframe_map follows FFmpeg's hardware-frame contract.
        unsafe {
            (*mapped.0.as_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        }
        // SAFETY: both frames are live for the call; mapped owns the references
        // created by FFmpeg until it is dropped.
        let result = unsafe {
            ffi::av_hwframe_map(
                mapped.0.as_ptr(),
                self.frame.0.as_ptr(),
                ffi::AV_HWFRAME_MAP_READ as i32,
            )
        };
        if result < 0 {
            return Err(Av1CodecError::MapFrame(result));
        }
        // SAFETY: a successful DRM PRIME map stores AVDRMFrameDescriptor in data[0].
        let frame = unsafe { &*mapped.0.as_ptr() };
        if frame.format != ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
            || frame.data[0].is_null()
        {
            return Err(Av1CodecError::InvalidHardwareFrame(
                "mapped frame is not DRM PRIME",
            ));
        }
        // SAFETY: the format and non-null data pointer were validated above.
        let descriptor = unsafe { &*frame.data[0].cast::<AvDrmFrameDescriptor>() };
        let object_count = usize::try_from(descriptor.nb_objects)
            .ok()
            .filter(|count| (1..=4).contains(count))
            .ok_or(Av1CodecError::InvalidHardwareFrame(
                "invalid DRM object count",
            ))?;
        let layer_count = usize::try_from(descriptor.nb_layers)
            .ok()
            .filter(|count| (1..=4).contains(count))
            .ok_or(Av1CodecError::InvalidHardwareFrame(
                "invalid DRM layer count",
            ))?;
        let objects = descriptor.objects[..object_count]
            .iter()
            .map(|object| DmaBufObject {
                fd: object.fd,
                size: object.size,
                format_modifier: object.format_modifier,
            })
            .collect::<Vec<_>>();
        let mut layers = Vec::with_capacity(layer_count);
        for layer in &descriptor.layers[..layer_count] {
            let plane_count = usize::try_from(layer.nb_planes)
                .ok()
                .filter(|count| (1..=4).contains(count))
                .ok_or(Av1CodecError::InvalidHardwareFrame(
                    "invalid DRM plane count",
                ))?;
            let planes = layer.planes[..plane_count]
                .iter()
                .map(|plane| {
                    let object_index = usize::try_from(plane.object_index).map_err(|_| {
                        Av1CodecError::InvalidHardwareFrame("negative DRM object index")
                    })?;
                    if object_index >= object_count || plane.offset < 0 || plane.pitch <= 0 {
                        return Err(Av1CodecError::InvalidHardwareFrame(
                            "invalid DRM plane layout",
                        ));
                    }
                    Ok(DmaBufPlane {
                        object_index,
                        offset: plane.offset,
                        pitch: plane.pitch,
                    })
                })
                .collect::<Result<Vec<_>, Av1CodecError>>()?;
            layers.push(DmaBufLayer {
                format: layer.format,
                planes,
            });
        }
        Ok(MappedDmaBufFrame {
            _frame: mapped,
            width: frame.width as u32,
            height: frame.height as u32,
            objects,
            layers,
        })
    }
}

fn copy_plane(
    data: *mut u8,
    line_size: i32,
    row_bytes: usize,
    rows: usize,
) -> Result<Vec<u8>, Av1CodecError> {
    let line_size = usize::try_from(line_size)
        .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative plane stride"))?;
    if data.is_null() || line_size < row_bytes {
        return Err(Av1CodecError::InvalidHardwareFrame(
            "missing plane or short plane stride",
        ));
    }
    let length = row_bytes
        .checked_mul(rows)
        .ok_or(Av1CodecError::InvalidHardwareFrame("plane size overflow"))?;
    let mut output = Vec::with_capacity(length);
    for row in 0..rows {
        // SAFETY: FFmpeg exposes `rows` rows of at least `line_size` bytes.
        let source = unsafe { std::slice::from_raw_parts(data.add(row * line_size), row_bytes) };
        output.extend_from_slice(source);
    }
    Ok(output)
}

impl Drop for CodecContext {
    fn drop(&mut self) {
        let mut context = self.0.as_ptr();
        // SAFETY: this object owns the codec context and frees it once.
        unsafe { ffi::avcodec_free_context(&mut context) };
    }
}

pub struct Av1VaApiDecoder {
    context: CodecContext,
}

impl Av1VaApiDecoder {
    pub fn open(device: &HardwareDevice) -> Result<Self, Av1CodecError> {
        const CODEC_NAME: &str = "av1";
        require_device(device, HardwareDeviceType::VaApi)?;
        ffmpeg_next::init().map_err(Av1CodecError::Initialization)?;

        let codec_name = c"av1";
        // SAFETY: codec_name is a static null-terminated string.
        let codec = unsafe { ffi::avcodec_find_decoder_by_name(codec_name.as_ptr()) };
        if codec.is_null() {
            return Err(Av1CodecError::DecoderNotFound);
        }
        require_hardware_configuration(
            codec,
            CODEC_NAME,
            HardwareDeviceType::VaApi,
            ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
            ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32,
        )?;

        let context = CodecContext::allocate(codec, CODEC_NAME)?;
        let device_reference = device
            .reference()
            .ok_or(Av1CodecError::ReferenceHardwareDevice)?;
        // SAFETY: context is exclusively owned, and FFmpeg takes ownership of
        // the new device reference. The callback selects VA-API or fails.
        unsafe {
            (*context.0.as_ptr()).hw_device_ctx = device_reference.as_ptr();
            (*context.0.as_ptr()).get_format = Some(select_vaapi_format);
        }
        // SAFETY: context and codec match and all required fields are initialized.
        let result = unsafe { ffi::avcodec_open2(context.0.as_ptr(), codec, ptr::null_mut()) };
        if result < 0 {
            return Err(Av1CodecError::Open {
                codec: CODEC_NAME,
                code: result,
            });
        }

        Ok(Self { context })
    }

    pub fn decode_one_packet(&mut self, payload: &[u8]) -> Result<DecodedAv1Frame, Av1CodecError> {
        self.submit_packet(payload)?;
        if let Some(frame) = self.receive_frame()? {
            return Ok(frame);
        }
        // A one-frame proof has no later packet to release decoder delay.
        // SAFETY: a null packet flushes the initialized decoder.
        let result = unsafe { ffi::avcodec_send_packet(self.context.0.as_ptr(), ptr::null()) };
        if result < 0 {
            return Err(Av1CodecError::SendPacket(result));
        }
        self.receive_frame()?
            .ok_or(Av1CodecError::ReceiveFrame(-11))
    }

    pub fn decode_packet(
        &mut self,
        payload: &[u8],
    ) -> Result<Option<DecodedAv1Frame>, Av1CodecError> {
        self.submit_packet(payload)?;
        self.receive_frame()
    }

    fn submit_packet(&mut self, payload: &[u8]) -> Result<(), Av1CodecError> {
        if payload.is_empty() || payload.len() > i32::MAX as usize {
            return Err(Av1CodecError::InvalidHardwareFrame(
                "invalid AV1 packet size",
            ));
        }
        let packet = Packet::allocate()?;
        // SAFETY: packet is exclusively owned and the requested size is valid.
        let result = unsafe { ffi::av_new_packet(packet.0.as_ptr(), payload.len() as i32) };
        if result < 0 {
            return Err(Av1CodecError::AllocatePacketPayload(result));
        }
        // SAFETY: av_new_packet allocated at least payload.len() bytes.
        unsafe {
            ptr::copy_nonoverlapping(payload.as_ptr(), (*packet.0.as_ptr()).data, payload.len());
        }
        // SAFETY: codec and packet are initialized and exclusively accessed.
        let result =
            unsafe { ffi::avcodec_send_packet(self.context.0.as_ptr(), packet.0.as_ptr()) };
        if result < 0 {
            return Err(Av1CodecError::SendPacket(result));
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedAv1Frame>, Av1CodecError> {
        let frame = Frame::allocate()?;
        // SAFETY: codec and frame are initialized and exclusively accessed.
        let result =
            unsafe { ffi::avcodec_receive_frame(self.context.0.as_ptr(), frame.0.as_ptr()) };
        if result == -11 {
            return Ok(None);
        }
        if result < 0 {
            return Err(Av1CodecError::ReceiveFrame(result));
        }
        // SAFETY: successful receive initialized all public frame fields.
        let received = unsafe { &*frame.0.as_ptr() };
        if received.format != ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32
            || received.width <= 0
            || received.height <= 0
        {
            return Err(Av1CodecError::InvalidHardwareFrame(
                "decoder did not return a VA-API frame",
            ));
        }
        Ok(Some(DecodedAv1Frame { frame }))
    }
}

#[cfg(windows)]
pub struct Av1D3d11Decoder {
    context: CodecContext,
}

#[cfg(windows)]
impl Av1D3d11Decoder {
    pub fn open(device: &HardwareDevice) -> Result<Self, Av1CodecError> {
        const CODEC_NAME: &str = "av1";
        require_device(device, HardwareDeviceType::D3d11Va)?;
        ffmpeg_next::init().map_err(Av1CodecError::Initialization)?;

        let codec_name = c"av1";
        // SAFETY: codec_name is a static null-terminated string.
        let codec = unsafe { ffi::avcodec_find_decoder_by_name(codec_name.as_ptr()) };
        if codec.is_null() {
            return Err(Av1CodecError::DecoderNotFound);
        }
        require_hardware_configuration(
            codec,
            CODEC_NAME,
            HardwareDeviceType::D3d11Va,
            ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
            ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32,
        )?;

        let context = CodecContext::allocate(codec, CODEC_NAME)?;
        let device_reference = device
            .reference()
            .ok_or(Av1CodecError::ReferenceHardwareDevice)?;
        // SAFETY: context is exclusively owned, and FFmpeg takes ownership of
        // the device reference. The callback selects D3D11VA or fails.
        unsafe {
            (*context.0.as_ptr()).hw_device_ctx = device_reference.as_ptr();
            (*context.0.as_ptr()).get_format = Some(select_d3d11_format);
        }
        // SAFETY: context and codec match and all required fields are initialized.
        let result = unsafe { ffi::avcodec_open2(context.0.as_ptr(), codec, ptr::null_mut()) };
        if result < 0 {
            return Err(Av1CodecError::Open {
                codec: CODEC_NAME,
                code: result,
            });
        }
        Ok(Self { context })
    }

    pub fn decode_packet(
        &mut self,
        payload: &[u8],
        presentation_timestamp: i64,
    ) -> Result<Option<DecodedAv1Frame>, Av1CodecError> {
        if payload.is_empty() || payload.len() > i32::MAX as usize {
            return Err(Av1CodecError::InvalidHardwareFrame(
                "invalid AV1 packet size",
            ));
        }
        let packet = Packet::allocate()?;
        // SAFETY: packet is exclusively owned and the requested size is valid.
        let result = unsafe { ffi::av_new_packet(packet.0.as_ptr(), payload.len() as i32) };
        if result < 0 {
            return Err(Av1CodecError::AllocatePacketPayload(result));
        }
        // SAFETY: av_new_packet allocated the payload and packet is exclusively owned.
        unsafe {
            ptr::copy_nonoverlapping(payload.as_ptr(), (*packet.0.as_ptr()).data, payload.len());
            (*packet.0.as_ptr()).pts = presentation_timestamp;
        }
        // SAFETY: codec and packet are initialized and exclusively accessed.
        let result =
            unsafe { ffi::avcodec_send_packet(self.context.0.as_ptr(), packet.0.as_ptr()) };
        if result < 0 {
            return Err(Av1CodecError::SendPacket(result));
        }

        let frame = Frame::allocate()?;
        // SAFETY: codec and frame are initialized and exclusively accessed.
        let result =
            unsafe { ffi::avcodec_receive_frame(self.context.0.as_ptr(), frame.0.as_ptr()) };
        if result == -11 {
            return Ok(None);
        }
        if result < 0 {
            return Err(Av1CodecError::ReceiveFrame(result));
        }
        // SAFETY: successful receive initialized all public frame fields.
        let received = unsafe { &*frame.0.as_ptr() };
        if received.format != ffi::AVPixelFormat::AV_PIX_FMT_D3D11 as i32
            || received.width <= 0
            || received.height <= 0
        {
            return Err(Av1CodecError::InvalidHardwareFrame(
                "decoder did not return a D3D11VA frame",
            ));
        }
        Ok(Some(DecodedAv1Frame { frame }))
    }
}

pub struct Av1NvencEncoder {
    _context: CodecContext,
    #[cfg(windows)]
    next_packet_is_keyframe: bool,
    #[cfg(windows)]
    device_context: ID3D11DeviceContext,
}

impl Av1NvencEncoder {
    pub fn open(
        device: &HardwareDevice,
        configuration: Av1EncoderConfiguration,
    ) -> Result<Self, Av1CodecError> {
        const CODEC_NAME: &str = "av1_nvenc";
        require_device(device, HardwareDeviceType::D3d11Va)?;
        validate_encoder_configuration(configuration)?;
        ffmpeg_next::init().map_err(Av1CodecError::Initialization)?;

        let codec_name = c"av1_nvenc";
        // SAFETY: codec_name is a static null-terminated string.
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(codec_name.as_ptr()) };
        if codec.is_null() {
            return Err(Av1CodecError::EncoderNotFound);
        }
        require_hardware_configuration(
            codec,
            CODEC_NAME,
            HardwareDeviceType::D3d11Va,
            ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
            ffi::AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX as i32,
        )?;

        let context = CodecContext::allocate(codec, CODEC_NAME)?;
        let frames = allocate_d3d11_frames(device, configuration)?;
        // SAFETY: context is exclusively owned. FFmpeg takes ownership of the
        // initialized frames reference and validates the complete mode on open.
        unsafe {
            let raw = context.0.as_ptr();
            (*raw).width = configuration.width as i32;
            (*raw).height = configuration.height as i32;
            (*raw).time_base = ffi::AVRational {
                num: 1,
                den: i32::from(configuration.frames_per_second),
            };
            (*raw).framerate = ffi::AVRational {
                num: i32::from(configuration.frames_per_second),
                den: 1,
            };
            (*raw).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*raw).bit_rate = configuration.bitrate_bits_per_second as i64;
            (*raw).rc_max_rate = configuration.bitrate_bits_per_second as i64;
            (*raw).rc_buffer_size = rate_control_buffer_size(
                configuration.bitrate_bits_per_second,
                configuration.frames_per_second,
            )?;
            (*raw).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
            (*raw).chroma_sample_location = ffi::AVChromaLocation::AVCHROMA_LOC_LEFT;
            match configuration.color_description {
                Av1ColorDescription::Bt709Limited => {
                    (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
                    (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
                    (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
                }
                Av1ColorDescription::Bt2020PqLimited => {
                    (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT2020;
                    (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
                    (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL;
                }
            }
            (*raw).max_b_frames = 0;
            (*raw).gop_size = i32::from(configuration.frames_per_second) * 2;
            (*raw).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*raw).hw_frames_ctx = frames.as_ptr();
        }
        set_encoder_option(&context, "preset", c"preset", c"p1")?;
        set_encoder_option(&context, "tune", c"tune", c"ull")?;
        set_encoder_option(&context, "rate control", c"rc", c"cbr")?;
        set_encoder_option(&context, "zero latency", c"zerolatency", c"1")?;
        set_encoder_option(&context, "encoder delay", c"delay", c"0")?;
        set_encoder_option(&context, "forced IDR", c"forced-idr", c"1")?;
        // SAFETY: context and codec match and all required fields are initialized.
        let result = unsafe { ffi::avcodec_open2(context.0.as_ptr(), codec, ptr::null_mut()) };
        if result < 0 {
            return Err(Av1CodecError::Open {
                codec: CODEC_NAME,
                code: result,
            });
        }

        #[cfg(windows)]
        let device_context = d3d11_device_context(device)?;
        Ok(Self {
            _context: context,
            #[cfg(windows)]
            next_packet_is_keyframe: true,
            #[cfg(windows)]
            device_context,
        })
    }

    pub fn set_bitrate(&mut self, bitrate_bits_per_second: u64) -> Result<(), Av1CodecError> {
        let context = self._context.0.as_ptr();
        // SAFETY: the encoder exclusively owns this open context. FFmpeg's
        // NVENC wrapper reads these mutable rate-control fields before frames.
        unsafe {
            let frames_per_second = u16::try_from((*context).framerate.num)
                .map_err(|_| Av1CodecError::InvalidConfiguration("invalid frame rate"))?;
            let buffer_size = rate_control_buffer_size(bitrate_bits_per_second, frames_per_second)?;
            (*context).bit_rate = bitrate_bits_per_second as i64;
            (*context).rc_max_rate = bitrate_bits_per_second as i64;
            (*context).rc_buffer_size = buffer_size;
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn encode_d3d11_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        presentation_timestamp: i64,
    ) -> Result<Option<EncodedAv1Packet>, Av1CodecError> {
        self.submit_d3d11_texture(texture, presentation_timestamp)?;
        self.receive_packet()
    }

    #[cfg(windows)]
    pub fn encode_one_d3d11_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        presentation_timestamp: i64,
    ) -> Result<EncodedAv1Packet, Av1CodecError> {
        self.submit_d3d11_texture(texture, presentation_timestamp)?;
        if let Some(packet) = self.receive_packet()? {
            return Ok(packet);
        }
        // The one-frame proof has no later input frame to release encoder delay.
        // SAFETY: a null frame flushes the initialized encoder.
        let result = unsafe { ffi::avcodec_send_frame(self._context.0.as_ptr(), ptr::null()) };
        if result < 0 {
            return Err(Av1CodecError::SendFrame(result));
        }
        self.receive_packet()?
            .ok_or(Av1CodecError::ReceivePacket(-11))
    }

    #[cfg(windows)]
    fn submit_d3d11_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        presentation_timestamp: i64,
    ) -> Result<(), Av1CodecError> {
        let frame = Frame::allocate()?;
        // SAFETY: the encoder owns an initialized hardware-frames context.
        let result = unsafe {
            ffi::av_hwframe_get_buffer(
                (*self._context.0.as_ptr()).hw_frames_ctx,
                frame.0.as_ptr(),
                0,
            )
        };
        if result < 0 {
            return Err(Av1CodecError::HardwareFrame(result));
        }
        // SAFETY: av_hwframe_get_buffer returned a D3D11 frame for this encoder.
        let destination = unsafe { (*frame.0.as_ptr()).data[0].cast::<std::ffi::c_void>() };
        if destination.is_null() {
            return Err(Av1CodecError::InvalidHardwareFrame(
                "encoder allocated no D3D11 texture",
            ));
        }
        // SAFETY: FFmpeg's D3D11 frame data[0] is an ID3D11Texture2D pointer
        // owned by the frame. Borrowing it does not change its reference count.
        let destination = unsafe {
            ID3D11Texture2D::from_raw_borrowed(&destination).ok_or(
                Av1CodecError::InvalidHardwareFrame("encoder returned an invalid D3D11 texture"),
            )?
        };
        // SAFETY: source and destination are same-device NV12 textures with
        // dimensions validated by the host pipeline and encoder configuration.
        unsafe { self.device_context.CopyResource(destination, texture) };
        // SAFETY: the frame is exclusively owned.
        unsafe { (*frame.0.as_ptr()).pts = presentation_timestamp };
        // SAFETY: codec and hardware frame are initialized.
        let result = unsafe { ffi::avcodec_send_frame(self._context.0.as_ptr(), frame.0.as_ptr()) };
        if result < 0 {
            return Err(Av1CodecError::SendFrame(result));
        }
        Ok(())
    }

    #[cfg(windows)]
    fn receive_packet(&mut self) -> Result<Option<EncodedAv1Packet>, Av1CodecError> {
        let packet = Packet::allocate()?;
        // SAFETY: codec and packet are initialized.
        let result =
            unsafe { ffi::avcodec_receive_packet(self._context.0.as_ptr(), packet.0.as_ptr()) };
        if result == -11 {
            return Ok(None);
        }
        if result < 0 {
            return Err(Av1CodecError::ReceivePacket(result));
        }
        // SAFETY: successful receive initialized packet data and size.
        let received = unsafe { &*packet.0.as_ptr() };
        let size = usize::try_from(received.size)
            .map_err(|_| Av1CodecError::InvalidHardwareFrame("negative packet size"))?;
        if received.data.is_null() || size == 0 {
            return Err(Av1CodecError::InvalidHardwareFrame("empty encoded packet"));
        }
        // SAFETY: FFmpeg exposes size bytes until the packet is unreferenced.
        let data = unsafe { std::slice::from_raw_parts(received.data, size) }.to_vec();
        let keyframe = self.next_packet_is_keyframe || received.flags & ffi::AV_PKT_FLAG_KEY != 0;
        self.next_packet_is_keyframe = false;
        Ok(Some(EncodedAv1Packet {
            presentation_timestamp: received.pts,
            keyframe,
            data,
        }))
    }
}

fn set_encoder_option(
    context: &CodecContext,
    name: &'static str,
    key: &CStr,
    value: &CStr,
) -> Result<(), Av1CodecError> {
    // SAFETY: the codec context owns a live private option object until open.
    let result = unsafe {
        ffi::av_opt_set(
            (*context.0.as_ptr()).priv_data,
            key.as_ptr(),
            value.as_ptr(),
            0,
        )
    };
    if result < 0 {
        Err(Av1CodecError::EncoderOption { name, code: result })
    } else {
        Ok(())
    }
}

pub struct EncodedAv1Packet {
    pub presentation_timestamp: i64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

#[cfg(windows)]
fn d3d11_device_context(device: &HardwareDevice) -> Result<ID3D11DeviceContext, Av1CodecError> {
    // SAFETY: device context layout is fixed by the allocated D3D11VA type.
    let raw_device = unsafe {
        let public = (*device.context.as_ptr())
            .data
            .cast::<ffi::AVHWDeviceContext>();
        let d3d11 = (*public).hwctx.cast::<ffi::AVD3D11VADeviceContext>();
        (*d3d11).device.cast::<std::ffi::c_void>()
    };
    // SAFETY: FFmpeg owns a live ID3D11Device for the hardware-device lifetime.
    let device = unsafe {
        ID3D11Device::from_raw_borrowed(&raw_device).ok_or(Av1CodecError::InvalidHardwareFrame(
            "hardware context has no D3D11 device",
        ))?
    };
    // SAFETY: the device is live for the complete call.
    unsafe { device.GetImmediateContext() }
        .map_err(|_| Av1CodecError::InvalidHardwareFrame("D3D11 device has no immediate context"))
}

fn require_device(
    device: &HardwareDevice,
    expected: HardwareDeviceType,
) -> Result<(), Av1CodecError> {
    if device.device_type() == expected {
        Ok(())
    } else {
        Err(Av1CodecError::WrongHardwareDevice {
            expected,
            actual: device.device_type(),
        })
    }
}

fn validate_encoder_configuration(
    configuration: Av1EncoderConfiguration,
) -> Result<(), Av1CodecError> {
    if configuration.width == 0 || configuration.width > i32::MAX as u32 {
        return Err(Av1CodecError::InvalidConfiguration("invalid width"));
    }
    if configuration.height == 0 || configuration.height > i32::MAX as u32 {
        return Err(Av1CodecError::InvalidConfiguration("invalid height"));
    }
    if configuration.frames_per_second == 0 {
        return Err(Av1CodecError::InvalidConfiguration("zero frame rate"));
    }
    if configuration.bitrate_bits_per_second == 0
        || configuration.bitrate_bits_per_second > i64::MAX as u64
    {
        return Err(Av1CodecError::InvalidConfiguration("invalid bitrate"));
    }
    rate_control_buffer_size(
        configuration.bitrate_bits_per_second,
        configuration.frames_per_second,
    )?;
    Ok(())
}

fn rate_control_buffer_size(
    bitrate_bits_per_second: u64,
    frames_per_second: u16,
) -> Result<i32, Av1CodecError> {
    if bitrate_bits_per_second == 0 || bitrate_bits_per_second > i64::MAX as u64 {
        return Err(Av1CodecError::InvalidConfiguration("invalid bitrate"));
    }
    if frames_per_second == 0 {
        return Err(Av1CodecError::InvalidConfiguration("zero frame rate"));
    }
    let bits = (u128::from(bitrate_bits_per_second) * 4).div_ceil(u128::from(frames_per_second));
    i32::try_from(bits)
        .map_err(|_| Av1CodecError::InvalidConfiguration("rate-control buffer is too large"))
}

fn require_hardware_configuration(
    codec: *const ffi::AVCodec,
    codec_name: &'static str,
    device_type: HardwareDeviceType,
    pixel_format: ffi::AVPixelFormat,
    required_method: i32,
) -> Result<(), Av1CodecError> {
    let mut index = 0;
    loop {
        // SAFETY: codec is a live FFmpeg descriptor and indices are queried
        // until FFmpeg returns null.
        let configuration = unsafe { ffi::avcodec_get_hw_config(codec, index) };
        if configuration.is_null() {
            return Err(Av1CodecError::UnsupportedHardwareConfiguration {
                codec: codec_name,
                device_type,
            });
        }
        // SAFETY: FFmpeg returned a non-null process-lifetime descriptor.
        let configuration = unsafe { &*configuration };
        if configuration.device_type == device_type.ffi_type()
            && configuration.pix_fmt == pixel_format
            && configuration.methods & required_method != 0
        {
            return Ok(());
        }
        index += 1;
    }
}

fn allocate_d3d11_frames(
    device: &HardwareDevice,
    configuration: Av1EncoderConfiguration,
) -> Result<NonNull<ffi::AVBufferRef>, Av1CodecError> {
    // SAFETY: device owns a live hardware-device reference.
    let frames = NonNull::new(unsafe { ffi::av_hwframe_ctx_alloc(device.context.as_ptr()) })
        .ok_or(Av1CodecError::AllocateHardwareFrames)?;
    // SAFETY: av_hwframe_ctx_alloc returns an AVBufferRef whose data points to
    // an exclusively owned AVHWFramesContext before initialization.
    unsafe {
        let context = (*frames.as_ptr()).data.cast::<ffi::AVHWFramesContext>();
        (*context).format = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
        (*context).sw_format = configuration.frame_format.software_pixel_format();
        (*context).width = configuration.width as i32;
        (*context).height = configuration.height as i32;
        let result = ffi::av_hwframe_ctx_init(frames.as_ptr());
        if result < 0 {
            let mut frames = frames.as_ptr();
            ffi::av_buffer_unref(&mut frames);
            return Err(Av1CodecError::InitializeHardwareFrames(result));
        }
    }
    Ok(frames)
}

unsafe extern "C" fn select_vaapi_format(
    _context: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    let mut format = formats;
    // SAFETY: FFmpeg provides an AV_PIX_FMT_NONE-terminated format array.
    unsafe {
        while *format != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *format == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *format;
            }
            format = format.add(1);
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

#[cfg(windows)]
unsafe extern "C" fn select_d3d11_format(
    _context: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    let mut format = formats;
    // SAFETY: FFmpeg provides an AV_PIX_FMT_NONE-terminated format array.
    unsafe {
        while *format != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *format == ffi::AVPixelFormat::AV_PIX_FMT_D3D11 {
                return *format;
            }
            format = format.add(1);
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

#[must_use]
pub fn library_version() -> &'static CStr {
    // SAFETY: FFmpeg returns a process-lifetime null-terminated version string.
    unsafe { CStr::from_ptr(ffi::av_version_info()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_device_names_are_stable() {
        assert_eq!(HardwareDeviceType::VaApi.to_string(), "VA-API");
        assert_eq!(HardwareDeviceType::D3d11Va.to_string(), "D3D11VA");
    }

    #[test]
    fn embedded_nul_is_rejected_before_ffi() {
        assert!(matches!(
            HardwareDevice::open(HardwareDeviceType::VaApi, Some("bad\0device")),
            Err(HardwareDeviceError::InvalidDeviceName(_))
        ));
    }

    #[test]
    fn encoder_configuration_rejects_zero_values() {
        let configuration = Av1EncoderConfiguration {
            width: 0,
            height: 1440,
            frames_per_second: 120,
            bitrate_bits_per_second: 20_000_000,
            frame_format: Av1FrameFormat::Yuv420Eight,
            color_description: Av1ColorDescription::Bt709Limited,
        };

        assert!(matches!(
            validate_encoder_configuration(configuration),
            Err(Av1CodecError::InvalidConfiguration("invalid width"))
        ));
    }

    #[test]
    fn rate_control_buffer_holds_four_frame_budgets() {
        assert_eq!(rate_control_buffer_size(20_000_000, 120).unwrap(), 666_667);
        assert!(matches!(
            rate_control_buffer_size(u64::MAX, 1),
            Err(Av1CodecError::InvalidConfiguration("invalid bitrate"))
        ));
    }

    #[test]
    fn software_pixel_formats_are_distinct() {
        let formats = [
            Av1FrameFormat::Yuv420Eight,
            Av1FrameFormat::Yuv420Ten,
            Av1FrameFormat::Yuv422Eight,
            Av1FrameFormat::Yuv422Ten,
            Av1FrameFormat::Yuv444Eight,
            Av1FrameFormat::Yuv444Ten,
        ]
        .map(Av1FrameFormat::software_pixel_format);

        for (index, format) in formats.iter().enumerate() {
            assert!(!formats[..index].contains(format));
        }
    }
}
