use crate::interactive_worker::{self, InteractiveHelperConnection};
use rustconsole_host_core::{HostSessionControlAction, HostSessionControlSource};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, IDI_APPLICATION, LoadIconW, MF_STRING, MSG, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTALIGN,
    TPM_RIGHTBUTTON, TPM_TOPALIGN, TRACK_POPUP_MENU_FLAGS, TrackPopupMenu, TranslateMessage,
    WINDOW_STYLE, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};
use windows::core::PCWSTR;

const NO_ACTIVE_SESSION: u32 = u32::MAX;
const TRAY_MESSAGE: u32 = WM_APP + 1;
const RELEASE_POINTER: usize = 1;

pub struct WindowsSessionControls {
    _connection: InteractiveHelperConnection,
    actions: Receiver<Result<HostSessionControlAction, String>>,
}

impl WindowsSessionControls {
    pub fn launch(executable: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // SAFETY: this query has no arguments.
        let session_id = unsafe { WTSGetActiveConsoleSessionId() };
        if session_id == NO_ACTIVE_SESSION {
            return Err("Windows has no active console session".into());
        }
        let mut token = HANDLE::default();
        // SAFETY: token is valid output storage.
        unsafe { WTSQueryUserToken(session_id, &mut token)? };
        let connection = interactive_worker::launch_session_controls(executable, session_id, token);
        // SAFETY: WTSQueryUserToken transferred ownership of this handle.
        let _ = unsafe { CloseHandle(token) };
        let connection = connection?;
        let mut channel = connection.channel.try_clone()?;
        let expected_token = connection.connection_token;
        let (tx, actions) = sync_channel(8);
        std::thread::spawn(move || {
            let result: Result<(), String> = (|| {
                let mut actual_token = [0; 16];
                channel
                    .read_exact(&mut actual_token)
                    .map_err(|error| error.to_string())?;
                if actual_token != expected_token {
                    return Err("session controls returned an invalid connection token".to_owned());
                }
                loop {
                    let mut action = [0; 1];
                    channel
                        .read_exact(&mut action)
                        .map_err(|error| error.to_string())?;
                    match action[0] {
                        1 => tx
                            .send(Ok(HostSessionControlAction::ReleasePointerCapture))
                            .map_err(|_| "session control receiver closed".to_owned())?,
                        _ => return Err("session controls returned an invalid action".to_owned()),
                    }
                }
            })();
            if let Err(error) = result {
                let _ = tx.send(Err(error));
            }
        });
        Ok(Self {
            _connection: connection,
            actions,
        })
    }
}

impl HostSessionControlSource for WindowsSessionControls {
    fn try_next_action(&mut self) -> Result<Option<HostSessionControlAction>, String> {
        match self.actions.try_recv() {
            Ok(action) => action.map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("session controls disconnected".to_owned()),
        }
    }
}

pub fn run(pipe_name: &str, token_hex: &str) -> Result<(), Box<dyn std::error::Error>> {
    let token = decode_token(token_hex)?;
    let mut channel = interactive_worker::connect_pipe(pipe_name)?;
    channel.write_all(&token)?;
    channel.flush()?;
    run_tray(channel)
}

fn run_tray(channel: File) -> Result<(), Box<dyn std::error::Error>> {
    static CHANNEL: std::sync::OnceLock<std::sync::Mutex<File>> = std::sync::OnceLock::new();
    CHANNEL
        .set(std::sync::Mutex::new(channel))
        .map_err(|_| "tray channel already set")?;

    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == TRAY_MESSAGE && matches!(lparam.0 as u32, WM_LBUTTONUP | WM_RBUTTONUP) {
            if let Ok(menu) = unsafe { CreatePopupMenu() } {
                let label = wide("Eject");
                let _ = unsafe {
                    AppendMenuW(menu, MF_STRING, RELEASE_POINTER, PCWSTR(label.as_ptr()))
                };
                let mut point = POINT::default();
                let _ = unsafe { GetCursorPos(&mut point) };
                let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
                let mut monitor_info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                let flags = if unsafe { GetMonitorInfoW(monitor, &mut monitor_info) }.as_bool() {
                    let (anchor, alignment) =
                        menu_anchor(point, monitor_info.rcMonitor, monitor_info.rcWork);
                    point = anchor;
                    alignment | TPM_RIGHTBUTTON
                } else {
                    TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON
                };
                let _ = unsafe { SetForegroundWindow(window) };
                unsafe {
                    let _ = TrackPopupMenu(menu, flags, point.x, point.y, None, window, None);
                }
                let _ = unsafe { DestroyMenu(menu) };
            }
            return LRESULT(0);
        }
        if message == WM_COMMAND && wparam.0 & 0xffff == RELEASE_POINTER {
            if let Some(channel) = CHANNEL.get() {
                let _ = channel.lock().map(|mut channel| channel.write_all(&[1]));
            }
            return LRESULT(0);
        }
        if message == WM_DESTROY {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    let class_name = wide("RustConsoleSessionControls");
    let window_name = wide("Rust Console session controls");
    // SAFETY: a null module name obtains the current executable module.
    let module = unsafe { GetModuleHandleW(None)? };
    let instance = HINSTANCE(module.0);
    let class = WNDCLASSW {
        hInstance: instance,
        lpfnWndProc: Some(window_proc),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // SAFETY: class strings and callback remain valid for the message-loop lifetime.
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(windows::core::Error::from_thread().into());
    }
    // SAFETY: the registered class and strings remain valid.
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?
    };
    let mut icon = NOTIFYICONDATAW {
        cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>())?,
        hWnd: window,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: TRAY_MESSAGE,
        hIcon: unsafe { LoadIconW(Some(instance), IDI_APPLICATION)? },
        ..Default::default()
    };
    let tip = wide("Rust Console: release anchored pointer");
    let tip_length = tip.len().min(icon.szTip.len());
    icon.szTip[..tip_length].copy_from_slice(&tip[..tip_length]);
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &icon) }.as_bool() {
        return Err(windows::core::Error::from_thread().into());
    }
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if result <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &icon) };
    Ok(())
}

fn menu_anchor(cursor: POINT, monitor: RECT, work_area: RECT) -> (POINT, TRACK_POPUP_MENU_FLAGS) {
    if work_area.bottom < monitor.bottom {
        (
            POINT {
                x: cursor.x,
                y: work_area.bottom,
            },
            TPM_LEFTALIGN | TPM_BOTTOMALIGN,
        )
    } else if work_area.top > monitor.top {
        (
            POINT {
                x: cursor.x,
                y: work_area.top,
            },
            TPM_LEFTALIGN | TPM_TOPALIGN,
        )
    } else if work_area.left > monitor.left {
        (
            POINT {
                x: work_area.left,
                y: cursor.y,
            },
            TPM_LEFTALIGN | TPM_TOPALIGN,
        )
    } else if work_area.right < monitor.right {
        (
            POINT {
                x: work_area.right,
                y: cursor.y,
            },
            TPM_RIGHTALIGN | TPM_TOPALIGN,
        )
    } else {
        (cursor, TPM_LEFTALIGN | TPM_BOTTOMALIGN)
    }
}

fn decode_token(value: &str) -> Result<[u8; 16], Box<dyn std::error::Error>> {
    if value.len() != 32 {
        return Err("invalid session controls token length".into());
    }
    let mut token = [0; 16];
    for (index, byte) in token.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)?;
    }
    Ok(token)
}

fn wide(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_menu_stays_inside_each_taskbar_edge() {
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let cursor = POINT { x: 1900, y: 1060 };

        let (bottom, bottom_flags) = menu_anchor(
            cursor,
            monitor,
            RECT {
                bottom: 1040,
                ..monitor
            },
        );
        assert_eq!(bottom.y, 1040);
        assert!(bottom_flags.contains(TPM_BOTTOMALIGN));

        let (top, top_flags) = menu_anchor(cursor, monitor, RECT { top: 40, ..monitor });
        assert_eq!(top.y, 40);
        assert!(top_flags.contains(TPM_TOPALIGN));

        let (left, left_flags) = menu_anchor(
            cursor,
            monitor,
            RECT {
                left: 40,
                ..monitor
            },
        );
        assert_eq!(left.x, 40);
        assert!(left_flags.contains(TPM_LEFTALIGN));

        let (right, right_flags) = menu_anchor(
            cursor,
            monitor,
            RECT {
                right: 1880,
                ..monitor
            },
        );
        assert_eq!(right.x, 1880);
        assert!(right_flags.contains(TPM_RIGHTALIGN));
    }
}
