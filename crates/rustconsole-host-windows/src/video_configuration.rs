//! Public capture configuration, independent of the worker pipe's encoding.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CaptureEngine {
    WindowsGraphicsCapture = 1,
    DesktopDuplication = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PixelFormat {
    Nv12 = 1,
    P010 = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ColorDescription {
    Bt709Limited = 1,
    Bt2020PqLimited = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoCaptureConfiguration {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub capture_engine: CaptureEngine,
    pub format: PixelFormat,
    pub color: ColorDescription,
}
