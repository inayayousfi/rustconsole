//! Windows display inventory. Discovery does not create a capture device or encoder.

use rustconsole_protocol::display::{
    AdapterId, Display, DisplayId, DisplayInventory, GraphicsAdapter, MAX_ADAPTERS, MAX_DISPLAYS,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIFactory1,
};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW,
    GetMonitorInfoW, MONITORINFO,
};
use windows::core::PCWSTR;

/// Enumerates through a short-lived console worker, using the host service's privileges.
pub fn discover_active_console(
    host_executable: &std::path::Path,
) -> Result<DisplayInventory, Box<dyn std::error::Error>> {
    crate::worker::MediaWorker::discover_displays(host_executable)
}

/// Enumerates the calling session without opening a capture device or encoder.
pub fn discover() -> Result<DisplayInventory, Box<dyn std::error::Error>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut adapters = Vec::new();
    let mut displays = Vec::new();
    for index in 0..=MAX_ADAPTERS as u32 {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error.into()),
        };
        if adapters.len() == MAX_ADAPTERS {
            return Err("too many graphics adapters".into());
        }
        let description = unsafe { adapter.GetDesc1()? };
        let id = AdapterId(
            u64::from(description.AdapterLuid.LowPart)
                | ((description.AdapterLuid.HighPart as u32 as u64) << 32),
        );
        adapters.push(GraphicsAdapter {
            id,
            name: text(&description.Description),
            vendor_id: description.VendorId,
            device_id: description.DeviceId,
            software: description.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        });
        for output_index in 0..=MAX_DISPLAYS as u32 {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            if output_index as usize == MAX_DISPLAYS {
                return Err("too many display outputs".into());
            }
            let output = unsafe { output.GetDesc()? };
            if !output.AttachedToDesktop.as_bool() {
                continue;
            }
            if displays.len() == MAX_DISPLAYS {
                return Err("too many attached displays".into());
            }
            let mut monitor = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // EDD_GET_DEVICE_INTERFACE_NAME returns a monitor identity, not its enumeration index.
            if !unsafe {
                EnumDisplayDevicesW(PCWSTR(output.DeviceName.as_ptr()), 0, &mut monitor, 1)
            }
            .as_bool()
            {
                return Err("display has no monitor interface identity".into());
            }
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            if !unsafe {
                EnumDisplaySettingsW(
                    PCWSTR(output.DeviceName.as_ptr()),
                    ENUM_CURRENT_SETTINGS,
                    &mut mode,
                )
            }
            .as_bool()
            {
                return Err("cannot query display mode".into());
            }
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !unsafe { GetMonitorInfoW(output.Monitor, &mut info) }.as_bool() {
                return Err("cannot query display placement".into());
            }
            displays.push(Display {
                id: DisplayId::new(text(&monitor.DeviceID).to_ascii_lowercase())?,
                name: text(&monitor.DeviceString),
                adapter: id,
                width: u32::try_from(
                    output.DesktopCoordinates.right - output.DesktopCoordinates.left,
                )?,
                height: u32::try_from(
                    output.DesktopCoordinates.bottom - output.DesktopCoordinates.top,
                )?,
                refresh_rate: mode.dmDisplayFrequency,
                primary: info.dwFlags & 1 != 0,
            });
        }
    }
    Ok(DisplayInventory::new(displays, adapters)?)
}

fn text(value: &[u16]) -> String {
    String::from_utf16_lossy(
        &value[..value
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(value.len())],
    )
}
