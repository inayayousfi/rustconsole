use crate::{KeyboardLeds, WindowsRingError, WindowsRingPair};
use core::mem::size_of;
use rand::{RngCore, rngs::OsRng};
use std::path::Path;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DI_REMOVEDEVICE_GLOBAL, DICD_GENERATE_ID, DICS_FLAG_GLOBAL, DIF_REGISTERDEVICE, DIF_REMOVE,
    DIREG_DEV, GUID_DEVCLASS_HIDCLASS, HDEVINFO, INSTALLFLAG_FORCE, SP_CLASSINSTALL_HEADER,
    SP_DEVINFO_DATA, SP_REMOVEDEVICE_PARAMS, SPDRP_HARDWAREID, SetupDiCallClassInstaller,
    SetupDiCreateDevRegKeyW, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiGetDeviceInstanceIdW, SetupDiSetClassInstallParamsW,
    SetupDiSetDeviceRegistryPropertyW, UpdateDriverForPlugAndPlayDevicesW,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::core::{Error, HSTRING, PCWSTR};

const HARDWARE_ID: &str = "ROOT\\RUSTCONSOLEINPUT";
const CONFIG_ROOT: &str = "SOFTWARE\\RustConsole\\VirtualInput";

pub struct VirtualInputOwner {
    token: String,
    generation: u64,
    mouse: Option<PnpDevice>,
    keyboard: Option<PnpDevice>,
    sink: WindowsRingPair,
}

impl VirtualInputOwner {
    pub fn create(inf_path: &Path, generation: u64) -> Result<Self, Error> {
        if generation == 0 {
            return Err(invalid_argument("input generation must be nonzero"));
        }
        let token = random_token();
        let sink = WindowsRingPair::create(&token, generation)?;
        write_config(0, 1, &token)?;
        if let Err(error) = write_config(1, 2, &token) {
            delete_config(0);
            return Err(error);
        }
        let mouse = match PnpDevice::create(0, &token) {
            Ok(device) => device,
            Err(error) => {
                delete_config(0);
                delete_config(1);
                return Err(error);
            }
        };
        let keyboard = match PnpDevice::create(1, &token) {
            Ok(device) => device,
            Err(error) => {
                drop(mouse);
                delete_config(0);
                delete_config(1);
                return Err(error);
            }
        };
        let inf = inf_path.canonicalize().map_err(|error| {
            Error::new(
                windows::core::HRESULT(0x8007_0002u32 as i32),
                error.to_string(),
            )
        })?;
        let inf = HSTRING::from(inf.as_os_str().to_string_lossy().as_ref());
        let hardware_id = HSTRING::from(HARDWARE_ID);
        let mut reboot = false.into();
        // SAFETY: both strings remain live and reboot points to valid output storage.
        if let Err(error) = unsafe {
            UpdateDriverForPlugAndPlayDevicesW(
                None,
                &hardware_id,
                &inf,
                INSTALLFLAG_FORCE,
                Some(&mut reboot),
            )
        } {
            drop(keyboard);
            drop(mouse);
            delete_config(0);
            delete_config(1);
            return Err(error);
        }
        if reboot.as_bool() {
            drop(keyboard);
            drop(mouse);
            delete_config(0);
            delete_config(1);
            return Err(Error::new(
                windows::core::HRESULT(0x8007_0BC2u32 as i32),
                "virtual input driver requested a reboot",
            ));
        }
        Ok(Self {
            token,
            generation,
            mouse: Some(mouse),
            keyboard: Some(keyboard),
            sink,
        })
    }

    pub fn sink_mut(&mut self) -> &mut WindowsRingPair {
        &mut self.sink
    }

    pub fn drain_keyboard_leds(&mut self) -> Result<Vec<(u64, KeyboardLeds)>, WindowsRingError> {
        self.sink.drain_keyboard_leds()
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn instance_ids(&self) -> (&str, &str) {
        (
            &self
                .mouse
                .as_ref()
                .expect("mouse owner missing")
                .instance_id,
            &self
                .keyboard
                .as_ref()
                .expect("keyboard owner missing")
                .instance_id,
        )
    }
}

impl Drop for VirtualInputOwner {
    fn drop(&mut self) {
        drop(self.keyboard.take());
        drop(self.mouse.take());
        delete_config(1);
        delete_config(0);
    }
}

struct PnpDevice {
    set: HDEVINFO,
    data: SP_DEVINFO_DATA,
    instance_id: String,
}

// SAFETY: this owner is not shared, and SetupAPI handles are released on the same process.
unsafe impl Send for PnpDevice {}

impl PnpDevice {
    fn create(index: u32, token: &str) -> Result<Self, Error> {
        // SAFETY: class GUID is static and no parent window is used.
        let set = unsafe { SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_HIDCLASS), None)? };
        let mut data = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..SP_DEVINFO_DATA::default()
        };
        let name = HSTRING::from(format!("Rust Console Virtual Input {index}"));
        let description = HSTRING::from(if index == 0 {
            "Rust Console Virtual Mouse"
        } else {
            "Rust Console Virtual Keyboard"
        });
        // SAFETY: set and all strings are live; data is valid output storage.
        let created = unsafe {
            SetupDiCreateDeviceInfoW(
                set,
                &name,
                &GUID_DEVCLASS_HIDCLASS,
                &description,
                None,
                DICD_GENERATE_ID,
                Some(&mut data),
            )
        };
        if let Err(error) = created {
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(set);
            }
            return Err(error);
        }
        let hardware_id = wide_multi_sz(HARDWARE_ID);
        // SAFETY: set/data are live and property bytes contain a valid UTF-16 MULTI_SZ.
        if let Err(error) = unsafe {
            SetupDiSetDeviceRegistryPropertyW(
                set,
                &mut data,
                SPDRP_HARDWAREID,
                Some(as_bytes(&hardware_id)),
            )
        } {
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(set);
            }
            return Err(error);
        }
        // SAFETY: set/data identify the unregistered device node.
        if let Err(error) =
            unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, Some(&data)) }
        {
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(set);
            }
            return Err(error);
        }
        let mut owner = Self {
            set,
            data,
            instance_id: String::new(),
        };
        owner.write_hardware_config(index, token)?;
        owner.instance_id = owner.read_instance_id()?;
        Ok(owner)
    }

    fn write_hardware_config(&self, index: u32, token: &str) -> Result<(), Error> {
        // SAFETY: set/data identify a registered device and request its hardware key.
        let key = unsafe {
            SetupDiCreateDevRegKeyW(
                self.set,
                &self.data,
                DICS_FLAG_GLOBAL.0,
                0,
                DIREG_DEV,
                None,
                PCWSTR::null(),
            )?
        };
        let result = set_dword(key, "DeviceIndex", index)
            .and_then(|()| set_string(key, "InstanceToken", token));
        // SAFETY: key is live and closed exactly once.
        unsafe {
            let _ = RegCloseKey(key);
        }
        result
    }

    fn read_instance_id(&self) -> Result<String, Error> {
        let mut required = 0;
        // SAFETY: first call queries required length only.
        let _ =
            unsafe { SetupDiGetDeviceInstanceIdW(self.set, &self.data, None, Some(&mut required)) };
        if required < 2 || required > 1024 {
            return Err(invalid_argument(
                "invalid generated device instance ID length",
            ));
        }
        let mut buffer = vec![0u16; required as usize];
        // SAFETY: buffer has the exact queried capacity.
        unsafe {
            SetupDiGetDeviceInstanceIdW(
                self.set,
                &self.data,
                Some(&mut buffer),
                Some(&mut required),
            )?
        };
        let end = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        String::from_utf16(&buffer[..end]).map_err(|_| invalid_argument("invalid instance ID"))
    }
}

impl Drop for PnpDevice {
    fn drop(&mut self) {
        let params = SP_REMOVEDEVICE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                InstallFunction: DIF_REMOVE,
            },
            Scope: DI_REMOVEDEVICE_GLOBAL,
            HwProfile: 0,
        };
        // SAFETY: set/data remain live through both removal calls.
        unsafe {
            let _ = SetupDiSetClassInstallParamsW(
                self.set,
                Some(&self.data),
                Some(&params.ClassInstallHeader),
                size_of::<SP_REMOVEDEVICE_PARAMS>() as u32,
            );
            let _ = SetupDiCallClassInstaller(DIF_REMOVE, self.set, Some(&self.data));
            let _ = SetupDiDestroyDeviceInfoList(self.set);
        }
    }
}

fn write_config(index: u32, kind: u32, token: &str) -> Result<(), Error> {
    let path = format!("{CONFIG_ROOT}\\Device{index}");
    let mut key = Default::default();
    // SAFETY: path is valid and key is output storage.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(HSTRING::from(path).as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if status.is_err() {
        return Err(Error::from_hresult(status.to_hresult()));
    }
    let result =
        set_dword(key, "DeviceKind", kind).and_then(|()| set_string(key, "InstanceToken", token));
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn set_dword(
    key: windows::Win32::System::Registry::HKEY,
    name: &str,
    value: u32,
) -> Result<(), Error> {
    let value = value.to_le_bytes();
    let status =
        unsafe { RegSetValueExW(key, &HSTRING::from(name), None, REG_DWORD, Some(&value)) };
    status.ok()
}

fn set_string(
    key: windows::Win32::System::Registry::HKEY,
    name: &str,
    value: &str,
) -> Result<(), Error> {
    let value = wide(value);
    let status = unsafe {
        RegSetValueExW(
            key,
            &HSTRING::from(name),
            None,
            REG_SZ,
            Some(as_bytes(&value)),
        )
    };
    status.ok()
}

fn delete_config(index: u32) {
    let path = HSTRING::from(format!("{CONFIG_ROOT}\\Device{index}"));
    unsafe {
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, &path);
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn wide_multi_sz(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0, 0]).collect()
}

fn as_bytes(values: &[u16]) -> &[u8] {
    unsafe { core::slice::from_raw_parts(values.as_ptr().cast(), size_of_val(values)) }
}

fn invalid_argument(message: &str) -> Error {
    Error::new(
        windows::core::HRESULT(0x8007_0057u32 as i32),
        message.to_owned(),
    )
}
