use crate::config::{DeviceConfig, DeviceKind};
use core::mem::{offset_of, size_of};
use core::ptr::null_mut;
use std::sync::atomic::{AtomicU64, Ordering};
use wdk_sys::NTSTATUS;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NOT_ENOUGH_MEMORY,
    ERROR_OUTOFMEMORY, ERROR_PATH_NOT_FOUND, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW,
    UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForMultipleObjects,
};

pub const MAGIC: u32 = 0x4449_4856;
pub const VERSION: u32 = 1;
pub const SLOT_COUNT: u32 = 256;
pub const REPORT_CAPACITY: u32 = 80;
pub const OUTPUT_REPORT_OPERATION: u8 = 1;
const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
const FILE_MAP_READ_WRITE: u32 = FILE_MAP_READ | FILE_MAP_WRITE;
const STATUS_ACCESS_DENIED: NTSTATUS = -1_073_741_790;
const STATUS_INVALID_PARAMETER: NTSTATUS = -1_073_741_811;
const STATUS_NO_MEMORY: NTSTATUS = -1_073_741_801;
const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = -1_073_741_772;
const STATUS_UNSUCCESSFUL: NTSTATUS = -1_073_741_823;

#[repr(C, align(8))]
pub struct RingHeader {
    pub magic: u32,
    pub version: u32,
    pub slot_count: u32,
    pub report_capacity: u32,
    pub generation: AtomicU64,
    pub producer_sequence: AtomicU64,
    pub consumer_sequence: AtomicU64,
    pub overflow_count: AtomicU64,
}

#[repr(C, align(8))]
pub struct ReportSlot {
    pub committed_sequence: AtomicU64,
    pub length: u16,
    pub reserved: u16,
    pub data: [u8; REPORT_CAPACITY as usize],
}

#[repr(C, align(8))]
pub struct SharedReportRing {
    pub header: RingHeader,
    pub slots: [ReportSlot; SLOT_COUNT as usize],
}

const _: () = {
    assert!(size_of::<RingHeader>() == 48);
    assert!(size_of::<ReportSlot>() == 96);
    assert!(offset_of!(ReportSlot, data) == 12);
    assert!(size_of::<SharedReportRing>() == 24_624);
    assert!(offset_of!(SharedReportRing, slots) == 48);
};

pub struct InputIpc {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    pub input_event: HANDLE,
    pub stop_event: HANDLE,
    output: Option<OutputIpc>,
}

struct OutputIpc {
    mapping: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    event: HANDLE,
}

// SAFETY: handles are process-wide values and ring access is serialized by the device map mutex.
unsafe impl Send for InputIpc {}

impl InputIpc {
    pub fn open(config: &DeviceConfig) -> Result<Self, NTSTATUS> {
        let kind = match config.kind {
            DeviceKind::Mouse => "mouse",
            DeviceKind::Keyboard => "keyboard",
        };
        let mapping_name = wide(&format!("Global\\RustConsoleInput-{}-{kind}", config.token));
        let event_name = wide(&format!(
            "Global\\RustConsoleInputEvent-{}-{kind}",
            config.token
        ));
        // SAFETY: both names are null terminated.
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ_WRITE, 0, mapping_name.as_ptr()) };
        if mapping.is_null() {
            return Err(last_error_status());
        }
        // SAFETY: mapping is live and expected to contain exactly one shared ring.
        let view = unsafe {
            MapViewOfFile(
                mapping,
                FILE_MAP_READ_WRITE,
                0,
                0,
                size_of::<SharedReportRing>(),
            )
        };
        if view.Value.is_null() {
            let status = last_error_status();
            // SAFETY: mapping is live and closed exactly once.
            unsafe { CloseHandle(mapping) };
            return Err(status);
        }
        // SAFETY: event name is null terminated.
        let input_event = unsafe {
            OpenEventW(
                EVENT_MODIFY_STATE | SYNCHRONIZE_ACCESS,
                0,
                event_name.as_ptr(),
            )
        };
        if input_event.is_null() {
            let status = last_error_status();
            // SAFETY: resources are live and released exactly once.
            unsafe {
                UnmapViewOfFile(view);
                CloseHandle(mapping);
            }
            return Err(status);
        }
        // SAFETY: unnamed event needs no security descriptor or name.
        let stop_event = unsafe { CreateEventW(null_mut(), 1, 0, null_mut()) };
        if stop_event.is_null() {
            let status = last_error_status();
            // SAFETY: resources are live and released exactly once.
            unsafe {
                CloseHandle(input_event);
                UnmapViewOfFile(view);
                CloseHandle(mapping);
            }
            return Err(status);
        }
        let mut ipc = Self {
            mapping,
            view,
            input_event,
            stop_event,
            output: None,
        };
        ipc.validate().map_err(|()| STATUS_INVALID_PARAMETER)?;
        if config.kind == DeviceKind::Keyboard {
            ipc.output = Some(OutputIpc::open(config)?);
        }
        Ok(ipc)
    }

    pub fn ring(&self) -> &SharedReportRing {
        // SAFETY: the view remains mapped for self's lifetime.
        unsafe { &*self.view.Value.cast::<SharedReportRing>() }
    }

    pub fn handles(&self) -> (HANDLE, HANDLE) {
        (self.stop_event, self.input_event)
    }

    pub fn signal_stop(&self) {
        // SAFETY: stop_event remains live while self is live.
        unsafe {
            let _ = SetEvent(self.stop_event);
        }
    }

    pub fn publish_output(
        &mut self,
        operation: u8,
        report_id: u8,
        payload: &[u8],
    ) -> Result<(), ()> {
        if operation != OUTPUT_REPORT_OPERATION || payload.is_empty() || payload.len() > 78 {
            return Err(());
        }
        let output = self.output.as_mut().ok_or(())?;
        // SAFETY: the keyboard DeviceState lock gives this producer exclusive access.
        let ring = unsafe { &mut *output.view.Value.cast::<SharedReportRing>() };
        validate_ring(ring)?;
        let producer = ring.header.producer_sequence.load(Ordering::Acquire);
        let consumer = ring.header.consumer_sequence.load(Ordering::Acquire);
        if producer.checked_sub(consumer).ok_or(())? >= u64::from(SLOT_COUNT) {
            ring.header.overflow_count.fetch_add(1, Ordering::AcqRel);
            return Err(());
        }
        let sequence = producer.checked_add(1).ok_or(())?;
        let slot = &mut ring.slots[((sequence - 1) % u64::from(SLOT_COUNT)) as usize];
        slot.length = (payload.len() + 2) as u16;
        slot.reserved = 0;
        slot.data.fill(0);
        slot.data[0] = operation;
        slot.data[1] = report_id;
        slot.data[2..2 + payload.len()].copy_from_slice(payload);
        slot.committed_sequence.store(sequence, Ordering::Release);
        ring.header
            .producer_sequence
            .store(sequence, Ordering::Release);
        // SAFETY: output event remains live while output is present.
        if unsafe { SetEvent(output.event) } == 0 {
            return Err(());
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ()> {
        validate_ring(self.ring())
    }
}

impl OutputIpc {
    fn open(config: &DeviceConfig) -> Result<Self, NTSTATUS> {
        let mapping_name = wide(&format!(
            "Global\\RustConsoleOutput-{}-keyboard",
            config.token
        ));
        let event_name = wide(&format!(
            "Global\\RustConsoleOutputEvent-{}-keyboard",
            config.token
        ));
        // SAFETY: both names are null terminated.
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ_WRITE, 0, mapping_name.as_ptr()) };
        if mapping.is_null() {
            return Err(last_error_status());
        }
        // SAFETY: mapping is live and expected to contain exactly one shared ring.
        let view = unsafe {
            MapViewOfFile(
                mapping,
                FILE_MAP_READ_WRITE,
                0,
                0,
                size_of::<SharedReportRing>(),
            )
        };
        if view.Value.is_null() {
            let status = last_error_status();
            unsafe { CloseHandle(mapping) };
            return Err(status);
        }
        // SAFETY: event name is null terminated.
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
        if event.is_null() {
            let status = last_error_status();
            unsafe {
                UnmapViewOfFile(view);
                CloseHandle(mapping);
            }
            return Err(status);
        }
        let output = Self {
            mapping,
            view,
            event,
        };
        // SAFETY: the mapped view remains live for output's lifetime.
        validate_ring(unsafe { &*output.view.Value.cast::<SharedReportRing>() })
            .map_err(|()| STATUS_INVALID_PARAMETER)?;
        Ok(output)
    }
}

impl Drop for OutputIpc {
    fn drop(&mut self) {
        // SAFETY: all resources are live and released exactly once.
        unsafe {
            CloseHandle(self.event);
            UnmapViewOfFile(self.view);
            CloseHandle(self.mapping);
        }
    }
}

fn validate_ring(ring: &SharedReportRing) -> Result<(), ()> {
    let header = &ring.header;
    if header.magic != MAGIC
        || header.version != VERSION
        || header.slot_count != SLOT_COUNT
        || header.report_capacity != REPORT_CAPACITY
        || header.generation.load(Ordering::Acquire) == 0
    {
        return Err(());
    }
    Ok(())
}

fn last_error_status() -> NTSTATUS {
    // SAFETY: called immediately after a Win32 function reports failure.
    match unsafe { GetLastError() } {
        ERROR_ACCESS_DENIED => STATUS_ACCESS_DENIED,
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => STATUS_OBJECT_NAME_NOT_FOUND,
        ERROR_NOT_ENOUGH_MEMORY | ERROR_OUTOFMEMORY => STATUS_NO_MEMORY,
        _ => STATUS_UNSUCCESSFUL,
    }
}

pub enum WaitResult {
    Stop,
    InputOrTimeout,
    Failed,
}

pub fn wait(stop_event: HANDLE, input_event: HANDLE) -> WaitResult {
    let handles = [stop_event, input_event];
    // SAFETY: the owning InputIpc keeps both handles live until the worker is joined.
    match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, 5_000) } {
        WAIT_OBJECT_0 => WaitResult::Stop,
        result if result == WAIT_OBJECT_0 + 1 || result == WAIT_TIMEOUT => {
            WaitResult::InputOrTimeout
        }
        _ => WaitResult::Failed,
    }
}

impl Drop for InputIpc {
    fn drop(&mut self) {
        // SAFETY: all resources are live and released exactly once.
        unsafe {
            let _ = SetEvent(self.stop_event);
            CloseHandle(self.stop_event);
            CloseHandle(self.input_event);
            UnmapViewOfFile(self.view);
            CloseHandle(self.mapping);
        }
    }
}

pub fn read_report(ipc: &InputIpc, kind: DeviceKind) -> Result<Option<(u64, Vec<u8>)>, ()> {
    ipc.validate()?;
    let ring = ipc.ring();
    let consumer = ring.header.consumer_sequence.load(Ordering::Acquire);
    let sequence = consumer.checked_add(1).ok_or(())?;
    if sequence > ring.header.producer_sequence.load(Ordering::Acquire) {
        return Ok(None);
    }
    let slot = &ring.slots[((sequence - 1) % u64::from(SLOT_COUNT)) as usize];
    let committed = slot.committed_sequence.load(Ordering::Acquire);
    if committed != sequence {
        return Ok(None);
    }
    let length = usize::from(slot.length);
    let expected = match kind {
        DeviceKind::Mouse => 8,
        DeviceKind::Keyboard => 29,
    };
    if length != expected || slot.reserved != 0 {
        return Err(());
    }
    let report = slot.data[..length].to_vec();
    if kind == DeviceKind::Mouse && !matches!(report[0], 1 | 2) {
        return Err(());
    }
    if slot.committed_sequence.load(Ordering::Acquire) != committed {
        return Ok(None);
    }
    Ok(Some((sequence, report)))
}

pub fn consume(ipc: &InputIpc, sequence: u64) {
    ipc.ring()
        .header
        .consumer_sequence
        .store(sequence, Ordering::Release);
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}
