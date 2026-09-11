use crate::{KeyboardLeds, WindowsRingError, WindowsRingPair};
use core::mem::size_of;
use rand::{RngCore, rngs::OsRng};
use std::path::Path;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DICD_GENERATE_ID, DICS_FLAG_GLOBAL, DIF_REGISTERDEVICE, DIREG_DEV, DiUninstallDevice,
    GUID_DEVCLASS_HIDCLASS, HDEVINFO, INSTALLFLAG_FORCE, SETUP_DI_GET_CLASS_DEVS_FLAGS,
    SP_DEVINFO_DATA, SPDRP_HARDWAREID, SetupDiCallClassInstaller, SetupDiCreateDevRegKeyW,
    SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiOpenDevRegKey,
    SetupDiSetDeviceRegistryPropertyW, UpdateDriverForPlugAndPlayDevicesW,
};
use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, HWND};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::{Error, HSTRING, PCWSTR};

const HARDWARE_ID: &str = "ROOT\\RUSTCONSOLEINPUT";
const INSTANCE_ID_PREFIX: &str = "ROOT\\RUST_CONSOLE_VIRTUAL_INPUT_";
const CONFIG_ROOT: &str = "SOFTWARE\\RustConsole\\VirtualInput";

pub struct VirtualInputOwner {
    token: String,
    generation: u64,
    instance_ids: [String; 2],
    sink: WindowsRingPair,
}

impl VirtualInputOwner {
    pub fn create(inf_path: &Path, generation: u64) -> Result<Self, Error> {
        if generation == 0 {
            return Err(invalid_argument("input generation must be nonzero"));
        }
        let existing = existing_devices()?;
        let token = existing
            .as_ref()
            .map_or_else(random_token, |devices| devices.token.clone());
        let sink = WindowsRingPair::create(&token, generation)?;
        write_config(0, 1, &token)?;
        if let Err(error) = write_config(1, 2, &token) {
            delete_config(0);
            return Err(error);
        }
        let Some(existing) = existing else {
            return create_devices(inf_path, generation, token, sink);
        };
        let mut instance_ids = existing.instance_ids;
        for (index, instance_id) in instance_ids.iter_mut().enumerate() {
            if instance_id.is_none() {
                let device = PnpDevice::create(index as u32, &token)?;
                *instance_id = Some(device.instance_id.clone());
            }
        }
        update_driver(inf_path)?;
        Ok(Self {
            token,
            generation,
            instance_ids: instance_ids.map(Option::unwrap),
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
        (&self.instance_ids[0], &self.instance_ids[1])
    }
}

pub fn remove_persistent_devices() -> Result<(), Error> {
    // SAFETY: the class GUID is static and no parent window or enumerator is used.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_HIDCLASS),
            PCWSTR::null(),
            None,
            SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
        )?
    };
    let result = (|| {
        let mut devices = Vec::new();
        let mut index = 0;
        loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..SP_DEVINFO_DATA::default()
            };
            // SAFETY: set is live and data is valid output storage.
            match unsafe { SetupDiEnumDeviceInfo(set, index, &mut data) } {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_NO_MORE_ITEMS.to_hresult() => break,
                Err(error) => return Err(error),
            }
            if owned_instance_id(&read_instance_id(set, &data)?) {
                devices.push(data);
            }
            index += 1;
        }
        for data in &devices {
            let mut reboot = false.into();
            // SAFETY: set and each device record remain live until the set is destroyed.
            unsafe { DiUninstallDevice(HWND::default(), set, data, 0, Some(&mut reboot))? };
            if reboot.as_bool() {
                return Err(Error::new(
                    windows::core::HRESULT(0x8007_0BC2u32 as i32),
                    "removing the virtual input devices requested a reboot",
                ));
            }
        }
        Ok(())
    })();
    // SAFETY: set was created above and is destroyed exactly once.
    let destroy = unsafe { SetupDiDestroyDeviceInfoList(set) };
    result.and_then(|()| destroy).map(|()| {
        delete_config(0);
        delete_config(1);
    })
}

fn create_devices(
    inf_path: &Path,
    generation: u64,
    token: String,
    sink: WindowsRingPair,
) -> Result<VirtualInputOwner, Error> {
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
            delete_config(0);
            delete_config(1);
            return Err(error);
        }
    };
    if let Err(error) = update_driver(inf_path) {
        delete_config(0);
        delete_config(1);
        return Err(error);
    }
    let instance_ids = [mouse.instance_id.clone(), keyboard.instance_id.clone()];
    Ok(VirtualInputOwner {
        token,
        generation,
        instance_ids,
        sink,
    })
}

fn update_driver(inf_path: &Path) -> Result<(), Error> {
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
    unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            None,
            &hardware_id,
            &inf,
            INSTALLFLAG_FORCE,
            Some(&mut reboot),
        )?
    };
    if reboot.as_bool() {
        return Err(Error::new(
            windows::core::HRESULT(0x8007_0BC2u32 as i32),
            "virtual input driver requested a reboot",
        ));
    }
    Ok(())
}

struct ExistingDevices {
    token: String,
    instance_ids: [Option<String>; 2],
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
        owner.instance_id = read_instance_id(owner.set, &owner.data)?;
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
}

impl Drop for PnpDevice {
    fn drop(&mut self) {
        // SAFETY: set is live and destroyed exactly once. The registered device persists.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.set);
        }
    }
}

fn existing_devices() -> Result<Option<ExistingDevices>, Error> {
    // SAFETY: the class GUID is static and no parent window or enumerator is used.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_HIDCLASS),
            PCWSTR::null(),
            None,
            SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
        )?
    };
    let result = (|| {
        let mut records = Vec::new();
        let mut index = 0;
        loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..SP_DEVINFO_DATA::default()
            };
            // SAFETY: set is live and data is valid output storage.
            match unsafe { SetupDiEnumDeviceInfo(set, index, &mut data) } {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_NO_MORE_ITEMS.to_hresult() => break,
                Err(error) => return Err(error),
            }
            let instance_id = read_instance_id(set, &data)?;
            if owned_instance_id(&instance_id) {
                let (device_index, token) = read_hardware_config(set, &data)?;
                records.push((device_index, token, instance_id));
            }
            index += 1;
        }
        select_existing_devices(records)
    })();
    // SAFETY: set was created above and is destroyed exactly once.
    let destroy = unsafe { SetupDiDestroyDeviceInfoList(set) };
    result.and_then(|devices| destroy.map(|()| devices))
}

fn select_existing_devices(
    records: Vec<(u32, String, String)>,
) -> Result<Option<ExistingDevices>, Error> {
    if records.is_empty() {
        return Ok(None);
    }
    let mut token = None::<String>;
    let mut instance_ids = [None::<String>, None::<String>];
    for (index, current_token, instance_id) in records {
        let index = usize::try_from(index).map_err(|_| invalid_existing_devices())?;
        let Some(slot) = instance_ids.get_mut(index) else {
            return Err(invalid_existing_devices());
        };
        if slot.is_some() || token.as_ref().is_some_and(|token| token != &current_token) {
            return Err(invalid_existing_devices());
        }
        token.get_or_insert_with(|| current_token.clone());
        *slot = Some(instance_id);
    }
    match token {
        Some(token) => Ok(Some(ExistingDevices {
            token,
            instance_ids,
        })),
        None => Err(invalid_existing_devices()),
    }
}

fn read_hardware_config(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Result<(u32, String), Error> {
    // SAFETY: set/data identify an enumerated device and request its hardware key read-only.
    let key =
        unsafe { SetupDiOpenDevRegKey(set, data, DICS_FLAG_GLOBAL.0, 0, DIREG_DEV, KEY_READ.0)? };
    let result = read_dword(key, "DeviceIndex")
        .and_then(|index| read_string(key, "InstanceToken").map(|token| (index, token)));
    // SAFETY: key was opened successfully and is closed exactly once.
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn read_dword(key: HKEY, name: &str) -> Result<u32, Error> {
    let mut value_type = Default::default();
    let mut value = 0_u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: key is live and all output buffers match their supplied sizes.
    unsafe {
        RegQueryValueExW(
            key,
            &HSTRING::from(name),
            None,
            Some(&mut value_type),
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
        .ok()?
    };
    if value_type != REG_DWORD || size != size_of::<u32>() as u32 {
        return Err(invalid_existing_devices());
    }
    Ok(value)
}

fn read_string(key: HKEY, name: &str) -> Result<String, Error> {
    let mut value_type = Default::default();
    let mut value = [0_u16; 65];
    let mut size = size_of_val(&value) as u32;
    // SAFETY: key is live and all output buffers match their supplied sizes.
    unsafe {
        RegQueryValueExW(
            key,
            &HSTRING::from(name),
            None,
            Some(&mut value_type),
            Some(value.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()?
    };
    if value_type != REG_SZ || size < 2 || !size.is_multiple_of(2) {
        return Err(invalid_existing_devices());
    }
    let length = size as usize / size_of::<u16>();
    if length > value.len() || value[length - 1] != 0 {
        return Err(invalid_existing_devices());
    }
    let value = String::from_utf16(&value[..length - 1]).map_err(|_| invalid_existing_devices())?;
    if !(16..=64).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(invalid_existing_devices());
    }
    Ok(value)
}

fn invalid_existing_devices() -> Error {
    invalid_argument("existing Rust Console virtual input devices are incomplete or inconsistent")
}

fn read_instance_id(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Result<String, Error> {
    let mut required = 0;
    // SAFETY: first call queries required length only.
    let _ = unsafe { SetupDiGetDeviceInstanceIdW(set, data, None, Some(&mut required)) };
    if !(2..=1024).contains(&required) {
        return Err(invalid_argument(
            "invalid generated device instance ID length",
        ));
    }
    let mut buffer = vec![0u16; required as usize];
    // SAFETY: buffer has the exact queried capacity.
    unsafe { SetupDiGetDeviceInstanceIdW(set, data, Some(&mut buffer), Some(&mut required))? };
    let end = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16(&buffer[..end]).map_err(|_| invalid_argument("invalid instance ID"))
}

fn owned_instance_id(instance_id: &str) -> bool {
    instance_id
        .get(..INSTANCE_ID_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(INSTANCE_ID_PREFIX))
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
    Error::new(windows::core::HRESULT(0x8007_0057u32 as i32), message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_instance_filter_excludes_other_root_hid_devices() {
        assert!(owned_instance_id(
            "ROOT\\RUST_CONSOLE_VIRTUAL_INPUT_1\\0010"
        ));
        assert!(owned_instance_id(
            "root\\rust_console_virtual_input_0\\0000"
        ));
        assert!(!owned_instance_id("ROOT\\RUSTCONSOLEINPUT\\0000"));
        assert!(!owned_instance_id(
            "HID\\RUST_CONSOLE_VIRTUAL_INPUT_1\\0000"
        ));
    }

    #[test]
    fn existing_devices_require_one_consistent_mouse_and_keyboard() {
        let selected = select_existing_devices(vec![
            (1, "0123456789abcdef".into(), "keyboard".into()),
            (0, "0123456789abcdef".into(), "mouse".into()),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(selected.token, "0123456789abcdef");
        assert_eq!(
            selected.instance_ids,
            [Some("mouse".into()), Some("keyboard".into())]
        );

        assert!(select_existing_devices(Vec::new()).unwrap().is_none());
        assert_eq!(
            select_existing_devices(vec![(0, "0123456789abcdef".into(), "mouse".into())])
                .unwrap()
                .unwrap()
                .instance_ids,
            [Some("mouse".into()), None]
        );
        assert!(
            select_existing_devices(vec![
                (0, "0123456789abcdef".into(), "mouse".into()),
                (1, "fedcba9876543210".into(), "keyboard".into()),
            ])
            .is_err()
        );
    }
}
