use core::mem::{size_of, size_of_val};
use core::ptr::null_mut;
use wdk_sys::{
    KEY_READ, NTSTATUS, PLUGPLAY_REGKEY_DEVICE, UNICODE_STRING, WDF_NO_HANDLE,
    WDF_NO_OBJECT_ATTRIBUTES, WDFDEVICE, WDFKEY, call_unsafe_wdf_function_binding,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_DWORD, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};

const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_INVALID_PARAMETER: NTSTATUS = -1_073_741_811;
const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = -1_073_741_776;
const DEVICE_INDEX_NAME: &[u16] = &[
    b'D' as u16,
    b'e' as u16,
    b'v' as u16,
    b'i' as u16,
    b'c' as u16,
    b'e' as u16,
    b'I' as u16,
    b'n' as u16,
    b'd' as u16,
    b'e' as u16,
    b'x' as u16,
    0,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceKind {
    Mouse,
    Keyboard,
}

#[derive(Clone, Debug)]
pub struct DeviceConfig {
    pub index: u32,
    pub kind: DeviceKind,
    pub token: String,
}

pub fn load(device: WDFDEVICE) -> Result<DeviceConfig, NTSTATUS> {
    let index = read_device_index(device)?;
    let (kind, token) = read_machine_config(index)?;
    Ok(DeviceConfig { index, kind, token })
}

fn read_device_index(device: WDFDEVICE) -> Result<u32, NTSTATUS> {
    let mut key: WDFKEY = WDF_NO_HANDLE.cast();
    // SAFETY: device is live and key points to valid output storage.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceOpenRegistryKey,
            device,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_READ,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut key,
        )
    };
    if status != STATUS_SUCCESS {
        return Err(status);
    }

    let mut index = 0;
    let name = UNICODE_STRING {
        Length: ((DEVICE_INDEX_NAME.len() - 1) * size_of::<u16>()) as u16,
        MaximumLength: (DEVICE_INDEX_NAME.len() * size_of::<u16>()) as u16,
        Buffer: DEVICE_INDEX_NAME.as_ptr().cast_mut(),
    };
    // SAFETY: key is open and name/index remain live for the synchronous call.
    let query_status =
        unsafe { call_unsafe_wdf_function_binding!(WdfRegistryQueryULong, key, &name, &mut index) };
    // SAFETY: key was returned successfully and is closed exactly once.
    unsafe { call_unsafe_wdf_function_binding!(WdfRegistryClose, key) };
    if query_status == STATUS_SUCCESS {
        Ok(index)
    } else {
        Err(query_status)
    }
}

fn read_machine_config(index: u32) -> Result<(DeviceKind, String), NTSTATUS> {
    let path = wide(&format!(
        "SOFTWARE\\RustConsole\\VirtualInput\\Device{index}"
    ));
    let mut key = null_mut();
    // SAFETY: path is null terminated and key points to valid output storage.
    let open_status =
        unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) };
    if open_status != 0 {
        return Err(STATUS_OBJECT_NAME_NOT_FOUND);
    }

    let name = wide("DeviceKind");
    let mut value_type = 0;
    let mut value = 0u32;
    let mut value_size = size_of::<u32>() as u32;
    // SAFETY: key is open and every buffer is valid for the supplied length.
    let query_status = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            null_mut(),
            &mut value_type,
            (&mut value as *mut u32).cast(),
            &mut value_size,
        )
    };
    // SAFETY: key was returned successfully and is closed exactly once.
    if query_status != 0 || value_type != REG_DWORD || value_size != size_of::<u32>() as u32 {
        // SAFETY: key was returned successfully and is closed exactly once.
        unsafe { RegCloseKey(key) };
        return Err(STATUS_INVALID_PARAMETER);
    }
    let kind = match value {
        1 => Ok(DeviceKind::Mouse),
        2 => Ok(DeviceKind::Keyboard),
        _ => Err(STATUS_INVALID_PARAMETER),
    }?;

    let token_name = wide("InstanceToken");
    let mut token_type = 0;
    let mut token = [0u16; 65];
    let mut token_size = size_of_val(&token) as u32;
    // SAFETY: key is open and token is valid for token_size bytes.
    let token_status = unsafe {
        RegQueryValueExW(
            key,
            token_name.as_ptr(),
            null_mut(),
            &mut token_type,
            token.as_mut_ptr().cast(),
            &mut token_size,
        )
    };
    // SAFETY: key was returned successfully and is closed exactly once.
    unsafe { RegCloseKey(key) };
    if token_status != 0 || token_type != REG_SZ || token_size < 2 || token_size % 2 != 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let length = token_size as usize / 2;
    if length > token.len() || token[length - 1] != 0 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let token = String::from_utf16(&token[..length - 1]).map_err(|_| STATUS_INVALID_PARAMETER)?;
    if !(16..=64).contains(&token.len())
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(STATUS_INVALID_PARAMETER);
    }
    Ok((kind, token))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}
