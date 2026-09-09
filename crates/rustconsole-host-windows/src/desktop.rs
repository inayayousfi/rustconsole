use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW, HDESK,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};

pub fn attach_input_desktop() -> windows::core::Result<String> {
    // SAFETY: the returned handle is owned by this function and is not inheritable.
    let desktop = OwnedDesktop(unsafe {
        OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)?
    });
    let name = desktop_name(desktop.0)?;
    // SAFETY: this capture thread creates no windows or hooks, and the desktop
    // belongs to the process window station selected at worker launch.
    unsafe { SetThreadDesktop(desktop.0)? };
    Ok(name)
}

fn desktop_name(desktop: HDESK) -> windows::core::Result<String> {
    let mut bytes = 0;
    // SAFETY: the zero-sized call obtains the required UTF-16 buffer size.
    let _ = unsafe {
        GetUserObjectInformationW(HANDLE(desktop.0), UOI_NAME, None, 0, Some(&mut bytes))
    };
    if bytes == 0 {
        return Err(windows::core::Error::from_thread());
    }
    let mut name = vec![0_u16; usize::try_from(bytes).unwrap_or(0).div_ceil(2)];
    // SAFETY: name has the byte capacity reported by the first call.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            bytes,
            Some(&mut bytes),
        )?;
    }
    let length = name
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..length]))
}

struct OwnedDesktop(HDESK);

impl Drop for OwnedDesktop {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the handle returned by OpenInputDesktop.
        let _ = unsafe { CloseDesktop(self.0) };
    }
}
