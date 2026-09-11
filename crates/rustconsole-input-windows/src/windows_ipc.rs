use crate::{
    HidReport, KeyboardLeds, OUTPUT_REPORT_OPERATION, ReportSink, RingError, SharedReportRing,
};
use core::mem::size_of;
use core::ptr::null_mut;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
    LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::core::{BOOL, Error, HSTRING, PCWSTR};

const SDDL_REVISION_1: u32 = 1;
const IPC_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;LS)";

pub struct WindowsRingPair {
    mouse: NamedRing,
    keyboard: NamedRing,
    keyboard_output: NamedRing,
}

struct NamedRing {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    event: HANDLE,
}

// SAFETY: each mapping has one producer protected by &mut WindowsRingPair.
unsafe impl Send for NamedRing {}

impl WindowsRingPair {
    pub fn create(token: &str, generation: u64) -> Result<Self, Error> {
        validate_token(token)?;
        let security = SecurityDescriptor::new()?;
        let mouse = NamedRing::create(token, "mouse", generation, security.attributes())?;
        let keyboard = NamedRing::create(token, "keyboard", generation, security.attributes())?;
        let keyboard_output = NamedRing::create_output(token, generation, security.attributes())?;
        Ok(Self {
            mouse,
            keyboard,
            keyboard_output,
        })
    }

    pub fn drain_keyboard_leds(&mut self) -> Result<Vec<(u64, KeyboardLeds)>, WindowsRingError> {
        // SAFETY: event remains live for this object and zero timeout does not block input.
        if unsafe { WaitForSingleObject(self.keyboard_output.event, 0) } != WAIT_OBJECT_0 {
            return Ok(Vec::new());
        }
        let mut leds = Vec::new();
        loop {
            // SAFETY: this service object is the sole consumer of the output ring.
            let ring = unsafe { &*self.keyboard_output.view.Value.cast::<SharedReportRing>() };
            let Some(record) = ring.consume_output().map_err(WindowsRingError::Ring)? else {
                return Ok(leds);
            };
            if record.operation != OUTPUT_REPORT_OPERATION || record.report_id != 0 {
                return Err(WindowsRingError::Ring(RingError::InvalidOutputRecord));
            }
            let state = KeyboardLeds::decode(&record.payload)
                .map_err(|_| WindowsRingError::Ring(RingError::InvalidOutputRecord))?;
            leds.push((record.sequence, state));
        }
    }
}

impl ReportSink for WindowsRingPair {
    type Error = WindowsRingError;

    fn submit(&mut self, report: HidReport) -> Result<(), Self::Error> {
        match report {
            HidReport::Mouse(bytes) => self.mouse.publish(&bytes),
            HidReport::Keyboard(bytes) => self.keyboard.publish(&bytes),
        }
    }
}

#[derive(Debug)]
pub enum WindowsRingError {
    Ring(RingError),
    Windows(Error),
}

impl NamedRing {
    fn create(
        token: &str,
        kind: &str,
        generation: u64,
        security: *const SECURITY_ATTRIBUTES,
    ) -> Result<Self, Error> {
        Self::create_named(
            format!("Global\\RustConsoleInput-{token}-{kind}"),
            format!("Global\\RustConsoleInputEvent-{token}-{kind}"),
            generation,
            security,
        )
    }

    fn create_output(
        token: &str,
        generation: u64,
        security: *const SECURITY_ATTRIBUTES,
    ) -> Result<Self, Error> {
        Self::create_named(
            format!("Global\\RustConsoleOutput-{token}-keyboard"),
            format!("Global\\RustConsoleOutputEvent-{token}-keyboard"),
            generation,
            security,
        )
    }

    fn create_named(
        mapping_name: String,
        event_name: String,
        generation: u64,
        security: *const SECURITY_ATTRIBUTES,
    ) -> Result<Self, Error> {
        let mapping_name = HSTRING::from(mapping_name);
        let event_name = HSTRING::from(event_name);
        // SAFETY: names and security attributes remain live for both calls.
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(security),
                PAGE_READWRITE,
                0,
                size_of::<SharedReportRing>() as u32,
                &mapping_name,
            )?
        };
        // SAFETY: called immediately after CreateFileMappingW succeeds.
        let mapping_existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // SAFETY: mapping is live and the requested view matches its complete size.
        let view = unsafe {
            MapViewOfFile(
                mapping,
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                size_of::<SharedReportRing>(),
            )
        };
        if view.Value.is_null() {
            // SAFETY: mapping was created successfully and is closed exactly once.
            unsafe { CloseHandle(mapping) }?;
            return Err(Error::from_thread());
        }
        // SAFETY: the mapped view is writable, aligned, and exactly one ring long.
        let ring = unsafe { &mut *view.Value.cast::<SharedReportRing>() };
        if mapping_existed {
            if ring.reopen(generation).is_err() {
                unsafe {
                    let _ = UnmapViewOfFile(view);
                    let _ = CloseHandle(mapping);
                }
                return Err(Error::new(
                    windows::core::HRESULT(0x8007_0057u32 as i32),
                    "existing virtual input ring is invalid",
                ));
            }
        } else {
            *ring = SharedReportRing::new(generation);
        }
        // SAFETY: name and security attributes remain live for the call.
        let event = match unsafe { CreateEventW(Some(security), false, false, &event_name) } {
            Ok(event) => event,
            Err(error) => {
                // SAFETY: both resources are live and released exactly once.
                unsafe {
                    let _ = UnmapViewOfFile(view);
                    let _ = CloseHandle(mapping);
                }
                return Err(error);
            }
        };
        Ok(Self {
            mapping,
            view,
            event,
        })
    }

    fn publish(&mut self, report: &[u8]) -> Result<(), WindowsRingError> {
        // SAFETY: this object uniquely owns the producer view for its lifetime.
        let ring = unsafe { &mut *self.view.Value.cast::<SharedReportRing>() };
        ring.publish(report).map_err(WindowsRingError::Ring)?;
        // SAFETY: event is live until this NamedRing is dropped.
        unsafe { SetEvent(self.event) }.map_err(WindowsRingError::Windows)
    }
}

impl Drop for NamedRing {
    fn drop(&mut self) {
        // SAFETY: resources are live and this destructor runs exactly once.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.mapping);
        }
    }
}

struct SecurityDescriptor {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

impl SecurityDescriptor {
    fn new() -> Result<Self, Error> {
        let sddl = HSTRING::from(IPC_SDDL);
        let mut descriptor = PSECURITY_DESCRIPTOR(null_mut());
        // SAFETY: sddl is valid and descriptor points to output storage.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )?;
        }
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: BOOL(0),
            },
        })
    }

    fn attributes(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attributes
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: ConvertStringSecurityDescriptor allocated this block with LocalAlloc.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.descriptor.0)));
        }
    }
}

fn validate_token(token: &str) -> Result<(), Error> {
    if !(16..=64).contains(&token.len())
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(Error::new(
            windows::core::HRESULT(0x8007_0057u32 as i32),
            "invalid virtual input token",
        ));
    }
    Ok(())
}
