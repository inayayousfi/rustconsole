//! Rust UMDF2 virtual input driver.

mod config;
mod descriptor;
mod ipc;

use core::mem::{size_of, size_of_val};
use core::ptr::{copy_nonoverlapping, null_mut};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;
use wdk_sys::{
    _WDF_EXECUTION_LEVEL, _WDF_IO_QUEUE_DISPATCH_TYPE, _WDF_SYNCHRONIZATION_SCOPE, _WDF_TRI_STATE,
    NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, ULONG, WDF_DEVICE_PNP_CAPABILITIES,
    WDF_DRIVER_CONFIG, WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
    WDF_OBJECT_ATTRIBUTES, WDFDEVICE, WDFDEVICE_INIT, WDFDRIVER, WDFOBJECT, WDFQUEUE, WDFREQUEST,
    call_unsafe_wdf_function_binding,
};

use config::{DeviceConfig, DeviceKind};
use ipc::InputIpc;

pub use descriptor::{
    KEYBOARD_PRODUCT_ID, KEYBOARD_REPORT_DESCRIPTOR, MOUSE_PRODUCT_ID, MOUSE_REPORT_DESCRIPTOR,
    VENDOR_ID,
};

const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_NOT_SUPPORTED: NTSTATUS = -1_073_741_637;
const STATUS_UNSUCCESSFUL: NTSTATUS = -1_073_741_823;
const STATUS_INVALID_PARAMETER: NTSTATUS = -1_073_741_811;

struct DeviceState {
    config: DeviceConfig,
    pending_reads: WDFQUEUE,
    ipc: Option<InputIpc>,
    pumping: bool,
    worker: Option<JoinHandle<()>>,
}

// SAFETY: WDF and Win32 handles are process-wide; DEVICES serializes state access.
unsafe impl Send for DeviceState {}

static DEVICES: OnceLock<Mutex<HashMap<usize, DeviceState>>> = OnceLock::new();

const fn hid_ctl_code(id: u32) -> u32 {
    (11 << 16) | (id << 2) | 3
}

const IOCTL_HID_GET_DEVICE_DESCRIPTOR: u32 = hid_ctl_code(0);
const IOCTL_HID_GET_REPORT_DESCRIPTOR: u32 = hid_ctl_code(1);
const IOCTL_HID_READ_REPORT: u32 = hid_ctl_code(2);
const IOCTL_HID_WRITE_REPORT: u32 = hid_ctl_code(3);
const IOCTL_HID_GET_STRING: u32 = hid_ctl_code(4);
const IOCTL_HID_ACTIVATE_DEVICE: u32 = hid_ctl_code(7);
const IOCTL_HID_DEACTIVATE_DEVICE: u32 = hid_ctl_code(8);
const IOCTL_HID_GET_DEVICE_ATTRIBUTES: u32 = hid_ctl_code(9);
const IOCTL_UMDF_HID_SET_OUTPUT_REPORT: u32 = hid_ctl_code(22);

fn devices() -> &'static Mutex<HashMap<usize, DeviceState>> {
    DEVICES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg_attr(not(test), unsafe(export_name = "DriverEntry"))]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    const { assert!(size_of::<WDF_DRIVER_CONFIG>() <= ULONG::MAX as usize) };
    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of::<WDF_DRIVER_CONFIG>() as ULONG,
        EvtDriverDeviceAdd: Some(evt_driver_device_add),
        ..WDF_DRIVER_CONFIG::default()
    };

    // SAFETY: WDF owns the entry-point arguments and consumes the configuration
    // synchronously. Null object attributes and output handle are permitted.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut config,
            WDF_NO_HANDLE.cast::<WDFDRIVER>(),
        )
    }
}

extern "C" fn evt_driver_device_add(
    _driver: WDFDRIVER,
    mut device_init: *mut WDFDEVICE_INIT,
) -> NTSTATUS {
    const { assert!(size_of::<WDF_OBJECT_ATTRIBUTES>() <= ULONG::MAX as usize) };
    let mut attributes = WDF_OBJECT_ATTRIBUTES {
        Size: size_of::<WDF_OBJECT_ATTRIBUTES>() as ULONG,
        EvtCleanupCallback: Some(evt_device_cleanup),
        ExecutionLevel: _WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope: _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ..WDF_OBJECT_ATTRIBUTES::default()
    };
    let mut device: WDFDEVICE = WDF_NO_HANDLE.cast();
    // SAFETY: WDF supplies a live DeviceInit and attributes remain valid for the call.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut device_init,
            &mut attributes,
            &mut device,
        )
    };
    if status != STATUS_SUCCESS {
        return status;
    }
    const { assert!(size_of::<WDF_DEVICE_PNP_CAPABILITIES>() <= ULONG::MAX as usize) };
    let mut pnp_capabilities = WDF_DEVICE_PNP_CAPABILITIES {
        Size: size_of::<WDF_DEVICE_PNP_CAPABILITIES>() as ULONG,
        Removable: _WDF_TRI_STATE::WdfTrue,
        SurpriseRemovalOK: _WDF_TRI_STATE::WdfTrue,
        ..WDF_DEVICE_PNP_CAPABILITIES::default()
    };
    // SAFETY: device is live and WDF consumes the capabilities synchronously.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceSetPnpCapabilities,
            device,
            &mut pnp_capabilities,
        )
    };
    let config = match config::load(device) {
        Ok(config) => config,
        Err(status) => return status,
    };
    const { assert!(size_of::<WDF_IO_QUEUE_CONFIG>() <= ULONG::MAX as usize) };
    let mut queue_config = WDF_IO_QUEUE_CONFIG {
        Size: size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG,
        DispatchType: _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchParallel,
        PowerManaged: _WDF_TRI_STATE::WdfUseDefault,
        DefaultQueue: 1,
        EvtIoDeviceControl: Some(evt_io_device_control),
        ..WDF_IO_QUEUE_CONFIG::default()
    };
    queue_config.Settings.Parallel.NumberOfPresentedRequests = ULONG::MAX;
    // SAFETY: device is live, queue_config remains valid for the call, and a
    // null output handle is permitted for the default queue.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &mut queue_config,
            WDF_NO_OBJECT_ATTRIBUTES,
            core::ptr::null_mut(),
        )
    };
    if status != STATUS_SUCCESS {
        return status;
    }

    let mut pending_config = WDF_IO_QUEUE_CONFIG {
        Size: size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG,
        DispatchType: _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchManual,
        PowerManaged: _WDF_TRI_STATE::WdfUseDefault,
        ..WDF_IO_QUEUE_CONFIG::default()
    };
    let mut pending_reads: WDFQUEUE = WDF_NO_HANDLE.cast();
    // SAFETY: device is live and pending_reads points to valid output storage.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &mut pending_config,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut pending_reads,
        )
    };
    if status != STATUS_SUCCESS {
        return status;
    }

    let key = device as usize;
    let Ok(mut device_map) = devices().lock() else {
        return STATUS_UNSUCCESSFUL;
    };
    device_map.insert(
        key,
        DeviceState {
            config,
            pending_reads,
            ipc: None,
            pumping: false,
            worker: None,
        },
    );
    drop(device_map);
    let worker = match std::thread::Builder::new()
        .name("rustconsole-hid-input".into())
        .spawn(move || input_worker(key))
    {
        Ok(worker) => worker,
        Err(_) => {
            if let Ok(mut devices) = devices().lock() {
                devices.remove(&key);
            }
            return STATUS_UNSUCCESSFUL;
        }
    };
    let Ok(mut devices) = devices().lock() else {
        return STATUS_UNSUCCESSFUL;
    };
    let Some(state) = devices.get_mut(&key) else {
        return STATUS_UNSUCCESSFUL;
    };
    state.worker = Some(worker);
    STATUS_SUCCESS
}

unsafe extern "C" fn evt_device_cleanup(object: WDFOBJECT) {
    if let Some(devices) = DEVICES.get() {
        let mut removed = devices
            .lock()
            .ok()
            .and_then(|mut devices| devices.remove(&(object as usize)));
        if let Some(state) = removed.as_mut() {
            if let Some(ipc) = state.ipc.as_ref() {
                ipc.signal_stop();
            }
            if let Some(worker) = state.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

unsafe extern "C" fn evt_io_device_control(
    queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_buffer_length: usize,
    _input_buffer_length: usize,
    io_control_code: ULONG,
) {
    // SAFETY: the callback receives a live queue owned by WDF.
    let device = unsafe { call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue) };
    let config = devices().lock().ok().and_then(|devices| {
        devices
            .get(&(device as usize))
            .map(|state| state.config.clone())
    });
    let Some(config) = config else {
        // SAFETY: this request has not been forwarded or completed.
        unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_UNSUCCESSFUL);
        }
        return;
    };

    match io_control_code {
        IOCTL_HID_GET_DEVICE_DESCRIPTOR => {
            let descriptor = hid_descriptor(config.kind);
            // SAFETY: WDF owns request and descriptor remains live for this call.
            unsafe { complete_bytes(request, &descriptor) };
        }
        IOCTL_HID_GET_REPORT_DESCRIPTOR => {
            let descriptor = match config.kind {
                DeviceKind::Mouse => MOUSE_REPORT_DESCRIPTOR,
                DeviceKind::Keyboard => KEYBOARD_REPORT_DESCRIPTOR,
            };
            // SAFETY: WDF owns request and the descriptor is static.
            unsafe { complete_bytes(request, descriptor) };
        }
        IOCTL_HID_READ_REPORT => {
            let pending_reads = devices().lock().ok().and_then(|devices| {
                devices
                    .get(&(device as usize))
                    .map(|state| state.pending_reads)
            });
            let Some(pending_reads) = pending_reads else {
                unsafe {
                    call_unsafe_wdf_function_binding!(
                        WdfRequestComplete,
                        request,
                        STATUS_UNSUCCESSFUL
                    );
                }
                return;
            };
            // SAFETY: request is live and pending_reads is this device's manual queue.
            let status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestForwardToIoQueue,
                    request,
                    pending_reads
                )
            };
            if status != STATUS_SUCCESS {
                unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status) };
            } else {
                pump_reports(device as usize);
            }
        }
        IOCTL_HID_WRITE_REPORT | IOCTL_UMDF_HID_SET_OUTPUT_REPORT => {
            if config.kind != DeviceKind::Keyboard {
                unsafe {
                    call_unsafe_wdf_function_binding!(
                        WdfRequestComplete,
                        request,
                        STATUS_NOT_SUPPORTED
                    );
                }
                return;
            }
            let mut input = null_mut();
            let mut input_length = 0;
            // SAFETY: request is live and metadata points to valid output storage.
            let status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestRetrieveInputBuffer,
                    request,
                    2,
                    &mut input,
                    &mut input_length,
                )
            };
            if status != STATUS_SUCCESS || input_length != 2 {
                unsafe {
                    call_unsafe_wdf_function_binding!(
                        WdfRequestComplete,
                        request,
                        if status == STATUS_SUCCESS {
                            STATUS_INVALID_PARAMETER
                        } else {
                            status
                        }
                    );
                }
                return;
            }
            // SAFETY: WDF guaranteed two readable bytes; byte alignment is one.
            let report = unsafe { core::slice::from_raw_parts(input.cast::<u8>(), 2) };
            let completion_status = if report[0] != 0 || report[1] & !0x1f != 0 {
                STATUS_INVALID_PARAMETER
            } else if devices().lock().ok().is_some_and(|mut devices| {
                devices.get_mut(&(device as usize)).is_some_and(|state| {
                    state.ipc.as_mut().is_some_and(|ipc| {
                        ipc.publish_output(
                            ipc::OUTPUT_REPORT_OPERATION,
                            report[0],
                            &report[1..],
                        )
                        .is_ok()
                    })
                })
            }) {
                STATUS_SUCCESS
            } else {
                STATUS_UNSUCCESSFUL
            };
            // SAFETY: this request has not been forwarded or completed.
            unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestCompleteWithInformation,
                    request,
                    completion_status,
                    if completion_status == STATUS_SUCCESS {
                        2
                    } else {
                        0
                    },
                );
            }
        }
        IOCTL_HID_GET_DEVICE_ATTRIBUTES => {
            let attributes = hid_attributes(config.kind);
            // SAFETY: WDF owns request and attributes remains live for this call.
            unsafe { complete_bytes(request, bytes_of(&attributes)) };
        }
        IOCTL_HID_GET_STRING => {
            let mut input = null_mut();
            let mut input_length = 0;
            // SAFETY: request is live and metadata points to valid output storage.
            let status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestRetrieveInputBuffer,
                    request,
                    size_of::<u32>(),
                    &mut input,
                    &mut input_length,
                )
            };
            if status != STATUS_SUCCESS {
                unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status) };
                return;
            }
            // SAFETY: WDF guaranteed at least four readable bytes; alignment is not guaranteed.
            let string_id = unsafe { (input.cast::<u32>()).read_unaligned() } & 0xffff;
            let value = match string_id {
                1 | 14 => "Rust Console".to_owned(),
                2 | 15 => match config.kind {
                    DeviceKind::Mouse => "Rust Console Virtual Mouse".to_owned(),
                    DeviceKind::Keyboard => "Rust Console Virtual Keyboard".to_owned(),
                },
                3 | 16 => format!("RC-{}-{}", config.token, config.index),
                _ => {
                    unsafe {
                        call_unsafe_wdf_function_binding!(
                            WdfRequestComplete,
                            request,
                            STATUS_INVALID_PARAMETER
                        );
                    }
                    return;
                }
            };
            let value = value.encode_utf16().chain([0]).collect::<Vec<_>>();
            // SAFETY: WDF owns request and value remains live for the call.
            unsafe { complete_bytes(request, bytes_of_slice(&value)) };
        }
        IOCTL_HID_ACTIVATE_DEVICE | IOCTL_HID_DEACTIVATE_DEVICE => {
            // SAFETY: this request has not been forwarded or completed.
            unsafe {
                call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_SUCCESS);
            }
        }
        _ => {
            // SAFETY: this request has not been forwarded or completed.
            unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestComplete,
                    request,
                    STATUS_NOT_SUPPORTED
                );
            }
        }
    }
}

fn input_worker(key: usize) {
    loop {
        let (handles, config) = {
            let Ok(devices) = devices().lock() else {
                return;
            };
            let Some(state) = devices.get(&key) else {
                return;
            };
            (
                state.ipc.as_ref().map(InputIpc::handles),
                state.config.clone(),
            )
        };
        let Some(handles) = handles else {
            let opened = InputIpc::open(&config).ok().is_some_and(|ipc| {
                devices().lock().ok().is_some_and(|mut devices| {
                    devices.get_mut(&key).is_some_and(|state| {
                        if state.ipc.is_some() {
                            return false;
                        }
                        state.ipc = Some(ipc);
                        true
                    })
                })
            });
            if opened {
                pump_reports(key);
                continue;
            }
            std::thread::sleep(Duration::from_millis(250));
            continue;
        };
        match ipc::wait(handles.0, handles.1) {
            ipc::WaitResult::Stop | ipc::WaitResult::Failed => return,
            ipc::WaitResult::InputOrTimeout => pump_reports(key),
        }
    }
}

fn pump_reports(key: usize) {
    let pending_reads = {
        let Ok(mut devices) = devices().lock() else {
            return;
        };
        let Some(state) = devices.get_mut(&key) else {
            return;
        };
        if state.pumping {
            return;
        }
        state.pumping = true;
        state.pending_reads
    };
    loop {
        let report = {
            let Ok(mut devices) = devices().lock() else {
                return;
            };
            let Some(state) = devices.get_mut(&key) else {
                return;
            };
            let Some(ipc) = state.ipc.as_ref() else {
                state.pumping = false;
                return;
            };
            match ipc::read_report(ipc, state.config.kind) {
                Ok(Some(report)) => report,
                Ok(None) | Err(()) => {
                    state.pumping = false;
                    return;
                }
            }
        };
        let mut request: WDFREQUEST = WDF_NO_HANDLE.cast();
        // SAFETY: pending_reads is live and request points to output storage.
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfIoQueueRetrieveNextRequest,
                pending_reads,
                &mut request,
            )
        };
        if status != STATUS_SUCCESS {
            if let Ok(mut devices) = devices().lock()
                && let Some(state) = devices.get_mut(&key)
            {
                state.pumping = false;
            }
            return;
        }
        // SAFETY: the retrieved request is exclusively owned until completion.
        if unsafe { complete_bytes(request, &report.1) } {
            let Ok(devices) = devices().lock() else {
                return;
            };
            let Some(state) = devices.get(&key) else {
                return;
            };
            let Some(ipc) = state.ipc.as_ref() else {
                return;
            };
            ipc::consume(ipc, report.0);
        } else {
            if let Ok(mut devices) = devices().lock()
                && let Some(state) = devices.get_mut(&key)
            {
                state.pumping = false;
            }
            return;
        }
    }
}

fn hid_descriptor(kind: DeviceKind) -> [u8; 9] {
    let length = match kind {
        DeviceKind::Mouse => MOUSE_REPORT_DESCRIPTOR.len(),
        DeviceKind::Keyboard => KEYBOARD_REPORT_DESCRIPTOR.len(),
    } as u16;
    let [length_low, length_high] = length.to_le_bytes();
    [9, 0x21, 0x11, 0x01, 0, 1, 0x22, length_low, length_high]
}

#[repr(C)]
struct HidDeviceAttributes {
    size: u32,
    vendor_id: u16,
    product_id: u16,
    version_number: u16,
    padding: u16,
}

fn hid_attributes(kind: DeviceKind) -> HidDeviceAttributes {
    HidDeviceAttributes {
        size: size_of::<HidDeviceAttributes>() as u32,
        vendor_id: VENDOR_ID,
        product_id: match kind {
            DeviceKind::Mouse => MOUSE_PRODUCT_ID,
            DeviceKind::Keyboard => KEYBOARD_PRODUCT_ID,
        },
        version_number: 0x0100,
        padding: 0,
    }
}

fn bytes_of<T>(value: &T) -> &[u8] {
    // SAFETY: a shared reference is valid for reads of its complete object size.
    unsafe { core::slice::from_raw_parts((value as *const T).cast(), size_of::<T>()) }
}

fn bytes_of_slice<T>(value: &[T]) -> &[u8] {
    // SAFETY: a shared slice is valid for reads of its complete byte extent.
    unsafe { core::slice::from_raw_parts(value.as_ptr().cast(), size_of_val(value)) }
}

unsafe fn complete_bytes(request: WDFREQUEST, bytes: &[u8]) -> bool {
    let mut output = null_mut();
    let mut output_length = 0;
    // SAFETY: request is live and output metadata points to valid storage.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            bytes.len(),
            &mut output,
            &mut output_length,
        )
    };
    if status != STATUS_SUCCESS {
        // SAFETY: request has not otherwise been completed.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status) };
        return false;
    }
    debug_assert!(output_length >= bytes.len());
    // SAFETY: WDF guaranteed a writable output buffer of at least bytes.len().
    unsafe { copy_nonoverlapping(bytes.as_ptr(), output.cast(), bytes.len()) };
    // SAFETY: request has not otherwise been completed.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            STATUS_SUCCESS,
            bytes.len() as u64,
        );
    }
    true
}
