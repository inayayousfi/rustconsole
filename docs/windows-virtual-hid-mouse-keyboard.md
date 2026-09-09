# Independent Windows Virtual HID Mouse and Keyboard

## Purpose

This document describes the difficult logic and Windows boundaries needed to
create virtual mouse and keyboard devices that Windows treats as normal HID
devices. It focuses on:

- The Windows driver stack.
- Device installation and Plug and Play creation.
- HID descriptors and exact report formats.
- Communication between an application or service and the driver.
- Pending HID reads, report delivery, output reports, and teardown.
- Security, failure handling, and validation.

The public API presented by the finished component is intentionally outside
scope. The application side may be a service, DLL, executable, or part of a
larger program as long as it obeys the contracts below.

## Terms

- **HID report descriptor**: bytes that tell Windows which controls a device
  has and how its input and output bytes are arranged.
- **HIDClass**: the standard Windows subsystem that turns a conforming HID
  device into mouse, keyboard, Raw Input, and HID interfaces.
- **UMDF2**: Microsoft's framework for drivers that run in a user-mode host
  process.
- **PnP device node**: a device instance managed by Windows Plug and Play and
  visible in Device Manager.
- **INF**: the package file that tells Windows which driver binary and services
  belong to a hardware ID.
- **Report ID**: a nonzero first byte that distinguishes multiple report
  layouts in one HID descriptor.
- **VID/PID**: the USB vendor and product identifiers reported by a HID device.
- **IOCTL**: a numbered request exchanged between Windows driver layers.

## Architecture

Use a UMDF2 HID minidriver below Microsoft's HID pass-through driver:

```text
Input producer
  -> shared report queue and event
  -> UMDF2 virtual HID minidriver
  -> MsHidUmdf.sys
  -> HIDClass.sys
  -> MouHid/KbdHid, Raw Input, and HID clients
```

The solution has four independently testable parts:

1. **Driver package**: the UMDF2 DLL, INF, signed catalog, and installation
   procedure.
2. **Device manager**: elevated code that writes configuration, creates the PnP
   parent, binds the driver, waits for the HID child, and removes both.
3. **Input transport**: a restricted shared-memory report queue plus an event.
4. **Report encoder**: code that turns held mouse or keyboard state into exact
   bytes declared by the descriptor.

Create the mouse and keyboard as separate PnP devices. This keeps their report
lengths, boot behavior, access rules, and failure handling independent. The
mouse descriptor can still contain separate relative and absolute reports.

## Operating-System Boundary

### Driver Stack

The INF gives the device the standard HID class GUID:

```text
{745a17a0-74d3-11d0-b6fe-00a0c90f57da}
```

It installs:

- `mshidumdf` as the HID pass-through function driver.
- `WUDFRd` as the UMDF reflector.
- The custom UMDF2 DLL as a lower filter.

The custom DLL does not implement a mouse or keyboard class driver. It answers
the HID requests forwarded by `MsHidUmdf.sys`; the standard Windows stack does
the class-specific work after reading the descriptor.

### Process and Privilege Boundary

Separate installation from ordinary report submission:

- Driver package installation, machine registry changes, certificate changes,
  and PnP creation require an elevated administrator or service.
- Report submission should run through a narrowly permissioned service or IPC
  endpoint. It should not require every calling application to run elevated.
- The UMDF driver normally runs in `WUDFHost.exe`, commonly under
  `LocalService`. Confirm the actual host identity and grant only that identity
  the shared-memory rights it needs.

Creating a named mapping in the Windows `Global\` namespace requires
`SeCreateGlobalPrivilege`. A Windows service is a more predictable owner than
an interactive application. The service should create the mapping and input
event before the PnP device is started so driver startup is deterministic.

## Driver Package

### Minimal INF Shape

Replace every `Your...` name and ID with a project-owned value. Keep the HID
class, Microsoft service names, and UMDF directives exact.

```ini
[Version]
Signature   = "$WINDOWS NT$"
Class       = HIDClass
ClassGuid   = {745a17a0-74d3-11d0-b6fe-00a0c90f57da}
Provider    = %ProviderName%
CatalogFile = YourVirtualHid.cat
DriverVer   = 09/04/2026,1.0.0.0
PnpLockdown = 1

[DestinationDirs]
DefaultDestDir = 13
UMDriverCopy   = 13

[SourceDisksNames]
1 = %DiskName%

[SourceDisksFiles]
YourVirtualHid.dll = 1

[Manufacturer]
%ManufacturerName% = Models,NTamd64

[Models.NTamd64]
%DeviceDesc% = VirtualHid_Install, ROOT\YourVirtualHid

[VirtualHid_Install.NT]
CopyFiles = UMDriverCopy

[UMDriverCopy]
YourVirtualHid.dll

[VirtualHid_Install.NT.HW]
AddReg = VirtualHid_HW_AddReg

[VirtualHid_HW_AddReg]
HKR,,"LowerFilters",0x00010008,"WUDFRd"
; Add a reviewed device ACL here. Do not grant Everyone generic-all access.

[VirtualHid_Install.NT.Services]
AddService = mshidumdf,0x000001fa,mshidumdf_Service
AddService = WUDFRd,0x000001f8,WUDFRD_Service

[mshidumdf_Service]
ServiceType   = 1
StartType     = 3
ErrorControl  = 1
ServiceBinary = %10%\System32\Drivers\mshidumdf.sys

[WUDFRD_Service]
ServiceType   = 1
StartType     = 3
ErrorControl  = 1
ServiceBinary = %10%\System32\Drivers\WUDFRd.sys

[VirtualHid_Install.NT.Wdf]
UmdfService                = YourVirtualHid,VirtualHid_UmdfService
UmdfServiceOrder           = YourVirtualHid
UmdfKernelModeClientPolicy = AllowKernelModeClients
UmdfFileObjectPolicy       = AllowNullAndUnknownFileObjects
UmdfMethodNeitherAction    = Copy
UmdfFsContextUsePolicy     = CanUseFsContext2
UmdfHostProcessSharing     = ProcessSharingDisabled

[VirtualHid_UmdfService]
UmdfLibraryVersion = 2.15.0
ServiceBinary      = %13%\YourVirtualHid.dll

[Strings]
ProviderName     = "Your Provider"
ManufacturerName = "Your Provider"
DeviceDesc       = "Virtual HID Device"
DiskName         = "Virtual HID Installation Disk"
```

`UmdfHostProcessSharing = ProcessSharingDisabled` isolates each device in its
own host process. This uses more processes but prevents one device's blocking or
CPU load from affecting another. Pooling can be evaluated later with measured
multi-device tests.

### Build and Signing

Build the DLL as a UMDF2 x64 driver with the Visual Studio C/C++ toolchain and
the Windows Driver Kit. Link the UMDF driver stub and required Windows system
libraries. Generate a catalog with `Inf2Cat`, then sign the DLL and catalog.

For local development, a test certificate can be placed in the machine Root
and TrustedPublisher stores. Do not ship that process as production signing.
A production driver package should use an appropriate Microsoft-supported
driver signing route and should not add a private self-signed root certificate
to customer machines.

Install the completed package with:

```powershell
pnputil.exe /add-driver YourVirtualHid.inf /install
```

Treat a nonzero exit code, missing DriverStore package, or requested reboot as
an explicit result. Do not continue to device creation after a failed install.

## Device Configuration

The driver needs immutable per-device configuration before PnP starts it. Store
this under an administrator-writable machine key:

```text
HKLM\SOFTWARE\YourVendor\YourVirtualHid\Device<N>
```

Recommended values:

| Name | Registry type | Meaning |
|---|---|---|
| `DeviceKind` | `REG_DWORD` | `1` mouse, `2` keyboard |
| `ReportDescriptor` | `REG_BINARY` | Exact HID report descriptor |
| `VendorId` | `REG_DWORD` | VID, limited to 16 bits |
| `ProductId` | `REG_DWORD` | PID, limited to 16 bits |
| `VersionNumber` | `REG_DWORD` | HID device version, 16 bits |
| `ManufacturerString` | `REG_SZ` | Manufacturer returned to HIDClass |
| `ProductString` | `REG_SZ` | Product returned to HIDClass |
| `SerialString` | `REG_SZ` | Stable unique serial for this instance |
| `InputReportByteLength` | `REG_DWORD` | Fixed read size, including report ID |
| `InstanceToken` | `REG_SZ` | Random ownership marker for safe cleanup |

Keep the key writable only by administrators and the owning service. Validate
every value again in the driver. Reject absent descriptors, zero report length,
descriptors above the chosen limit, and inconsistent report lengths.

Store `DeviceIndex` as `REG_DWORD` in the PnP device's hardware key. The driver
opens its own key with `WdfDeviceOpenRegistryKey(..., PLUGPLAY_REGKEY_DEVICE,
KEY_READ, ...)`, reads the index, derives the per-device configuration path and
IPC names, and then loads the immutable values.

Do not default a missing index to zero. Fail device startup. A silent default
can connect one PnP device to another device's input stream.

## PnP Device Creation

Use SetupAPI for a root-enumerated parent. `SwDeviceCreate` is not required for
this design.

### Creation Order

1. Allocate a unique device index and random instance token.
2. Validate the HID descriptor and report lengths.
3. Write the per-device machine configuration key.
4. Create and initialize the shared report queue and named input event.
5. Call `SetupDiCreateDeviceInfoList` for `GUID_DEVCLASS_HIDCLASS`.
6. Call `SetupDiCreateDeviceInfoW` with `DICD_GENERATE_ID`.
7. Set `SPDRP_HARDWAREID` to a double-null-terminated `MULTI_SZ` containing
   `ROOT\YourVirtualHid`.
8. Call `SetupDiCallClassInstaller(DIF_REGISTERDEVICE, ...)`.
9. Get the exact generated instance ID from the same `SP_DEVINFO_DATA`.
10. Open that device's hardware key and write `DeviceIndex` and
    `InstanceToken`.
11. Bind the INF with `UpdateDriverForPlugAndPlayDevicesW`, using the hardware
    ID that the INF actually matches.
12. Wait for the parent to reach `DN_STARTED` and for its HID child to appear.
13. Open the HID child only for validation; input submission continues through
    the private shared transport.

The hardware ID property must be encoded as:

```text
R O O T \ Y o u r V i r t u a l H i d \0 \0
```

where each character and null is UTF-16. Passing the byte count for a normal
single-null string creates a malformed `MULTI_SZ`.

### Ownership

Record all of these together:

- Generated parent instance ID.
- Random instance token.
- Device index.
- Expected private hardware ID.
- Enumerated HID child IDs.

Removal code must verify the private hardware ID and instance token before
calling a destructive SetupAPI operation. Never remove every `ROOT\HIDCLASS`
device or every device with the same VID/PID.

### Creation Failure

If any step after registration fails:

1. Stop report submission.
2. Remove the exact parent created by this attempt.
3. Wait until both live and phantom instances are absent.
4. Close and unlink the IPC objects.
5. Delete only this attempt's configuration key.
6. Return the original error plus cleanup errors.

Do not return a usable handle until the HID child is present and started.

## Driver Initialization

### Required Entry Points

The UMDF2 DLL needs these WDF callbacks:

```c
DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD EvtDeviceAdd;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL EvtIoDeviceControl;
EVT_WDF_OBJECT_CONTEXT_CLEANUP EvtDeviceContextCleanup;
```

Minimal `DriverEntry`:

```c
NTSTATUS DriverEntry(PDRIVER_OBJECT object, PUNICODE_STRING registry_path)
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, EvtDeviceAdd);
    return WdfDriverCreate(object, registry_path,
                           WDF_NO_OBJECT_ATTRIBUTES,
                           &config, WDF_NO_HANDLE);
}
```

### Device Context

The per-device context should contain only device-owned state:

```c
typedef struct DEVICE_CONTEXT {
    WDFDEVICE Device;

    UCHAR ReportDescriptor[4096];
    ULONG ReportDescriptorSize;
    HID_DESCRIPTOR HidDescriptor;
    HID_DEVICE_ATTRIBUTES Attributes;
    ULONG InputReportByteLength;

    WCHAR Manufacturer[128];
    WCHAR Product[128];
    WCHAR Serial[64];

    WDFQUEUE DefaultQueue;       // parallel HID control requests
    WDFQUEUE PendingReadQueue;   // manual IOCTL_HID_READ_REPORT queue
    WDFWAITLOCK InputLock;

    HANDLE InputMapping;
    void *InputView;
    HANDLE InputEvent;
    HANDLE StopEvent;
    HANDLE WorkerThread;

    volatile LONG TearingDown;
    LONG64 ConsumerSequence;
    ULONG DeviceIndex;
} DEVICE_CONTEXT;
```

Use explicit descriptor and report capacity limits. A 4 KiB descriptor and a
1 KiB report are generous for mouse and keyboard devices, but the private IPC
capacity can be smaller if descriptors are rejected when they exceed it.

### `EvtDeviceAdd` Order

```text
mark DeviceInit as a filter with WdfFdoInitSetFilter
attach DEVICE_CONTEXT and cleanup callback
WdfDeviceCreate
read DeviceIndex from the PnP hardware key
read and validate immutable machine configuration
initialize HID_DESCRIPTOR and HID_DEVICE_ATTRIBUTES
assign DEVPKEY_Device_BusReportedDeviceDesc
create InputLock
create parallel default queue with EvtIoDeviceControl
create manual PendingReadQueue
open the already-created input mapping and event
create a private manual-reset stop event
start the input worker
return success
```

Failure to open IPC, create the stop event, or start the worker must fail device
startup. A started HID device with no report worker is a broken device, not a
degraded mode.

Initialize the standard HID descriptor as:

```c
ctx->HidDescriptor.bLength = 0x09;
ctx->HidDescriptor.bDescriptorType = 0x21;
ctx->HidDescriptor.bcdHID = 0x0111;
ctx->HidDescriptor.bCountry = 0;
ctx->HidDescriptor.bNumDescriptors = 1;
ctx->HidDescriptor.DescriptorList[0].bReportType = 0x22;
ctx->HidDescriptor.DescriptorList[0].wReportLength =
    (USHORT)ctx->ReportDescriptorSize;
```

Set `HID_DEVICE_ATTRIBUTES.Size`, `VendorID`, `ProductID`, and
`VersionNumber` from validated configuration.

## HID Request Contract

Create the default WDF queue with `WdfIoQueueDispatchParallel`. Forward only
pending input reads to a separate manual queue. Complete every other request
exactly once.

Use the IOCTL values from the installed WDK's `hidport.h`. Do not reproduce
numeric values from memory.

| Request | Required behavior |
|---|---|
| `IOCTL_HID_GET_DEVICE_DESCRIPTOR` | Return the 9-byte `HID_DESCRIPTOR`. |
| `IOCTL_HID_GET_REPORT_DESCRIPTOR` | Return the exact configured report descriptor. |
| `IOCTL_HID_GET_DEVICE_ATTRIBUTES` | Return `HID_DEVICE_ATTRIBUTES`. |
| `IOCTL_HID_GET_STRING` | Return the requested UTF-16 manufacturer, product, or serial string. |
| `IOCTL_HID_GET_INDEXED_STRING` | Return only supported indexed strings; reject unknown indexes. |
| `IOCTL_HID_READ_REPORT` | Pair one pending request with one queued input report. |
| `IOCTL_HID_WRITE_REPORT` | Accept host output such as keyboard LEDs, if declared. |
| `IOCTL_UMDF_HID_SET_OUTPUT_REPORT` | Same logical output path as write-report. |
| `IOCTL_UMDF_HID_SET_FEATURE` | Support only descriptor-declared feature reports. |
| `IOCTL_UMDF_HID_GET_FEATURE` | Return only descriptor-declared feature reports. |
| `IOCTL_UMDF_HID_GET_INPUT_REPORT` | Return the latest report for the requested ID, if supported. |
| Activate, deactivate, idle notification | Return success unless the design has explicit lifecycle work. |
| Unknown request | Return `STATUS_NOT_SUPPORTED` or `STATUS_NOT_IMPLEMENTED`. |

For descriptor, attribute, and string requests:

1. Retrieve the WDF output memory.
2. Compare its size with the exact result size.
3. Return `STATUS_BUFFER_TOO_SMALL` or `STATUS_INVALID_BUFFER_SIZE` when needed.
4. Copy the result.
5. Set `WdfRequestSetInformation` to the number of bytes copied.
6. Complete the request with the final status.

Do not assume manufacturer and product string indexes are interchangeable.
Test the observed IDs from `MsHidUmdf.sys` on every supported Windows release
and map both the documented and observed forms deliberately.

### Pending Read Rule

`IOCTL_HID_READ_REPORT` is asynchronous:

- If a complete report is already queued, copy it to the request and complete
  immediately.
- Otherwise move the request to the manual pending-read queue and do not
  complete it in the dispatch callback.
- When a report arrives, complete one pending read with one report.
- Never complete several pending reads with duplicate copies of one transition.

The report copied to HIDClass must have the exact descriptor-defined read
length. For the descriptors below, every report is 8 bytes. Reject a request
whose output buffer is too small. Zero any required padding before completion.

## Input Transport

### Why a Queue Is Required

A single shared "latest report" slot is simple but can lose events. Windows
auto-reset events coalesce repeated signals. If key-down and key-up are both
written before the driver reads the slot, it may observe only key-up and the
keypress disappears. Mouse wheel ticks and fast clicks have the same problem.

Use a bounded single-producer, single-consumer report ring. Mouse movement may
be coalesced deliberately before enqueueing, but keyboard transitions, button
transitions, and wheel deltas must retain their order.

### Shared Layout

Define a fixed binary layout shared by the service and driver. Pin its size and
offsets with compile-time assertions in both implementations.

```c
#define VHID_MAGIC           0x44494856u  /* "VHID" little-endian */
#define VHID_VERSION         1
#define VHID_SLOT_COUNT      256
#define VHID_REPORT_CAPACITY 80

typedef struct VHID_REPORT_SLOT {
    volatile LONG64 CommittedSequence;
    USHORT Length;
    USHORT Reserved;
    UCHAR Data[VHID_REPORT_CAPACITY];
} VHID_REPORT_SLOT;

typedef struct VHID_INPUT_RING {
    ULONG Magic;
    ULONG Version;
    ULONG SlotCount;
    ULONG ReportCapacity;
    volatile LONG64 ProducerSequence;
    volatile LONG64 ConsumerSequence;
    volatile LONG64 OverflowCount;
    VHID_REPORT_SLOT Slots[VHID_SLOT_COUNT];
} VHID_INPUT_RING;
```

Use 64-bit sequences and require an x64 build so aligned interlocked operations
are available. Initialize all sequences to zero. The next report has sequence
one and uses slot zero:

```text
slot_index = (sequence - 1) % VHID_SLOT_COUNT
```

The report data contains the complete wire report. If the descriptor uses
report IDs, `Data[0]` is the ID. If it does not, `Data[0]` is the first report
field. The transport never guesses or prepends an ID.

### Writer Algorithm

Only one service thread may publish to one device ring. Serialize callers
before this function.

```text
validate mapping magic, version, capacity, and slot count
validate report ID and exact length against the device descriptor table

producer = atomic_load(ProducerSequence)
consumer = atomic_load(ConsumerSequence)

if producer - consumer >= SlotCount:
    increment OverflowCount
    return queue-full without changing device state

next = producer + 1
slot = Slots[(next - 1) % SlotCount]

slot.Length = report length
zero slot.Data
copy complete report to slot.Data
full memory barrier
atomic_exchange(slot.CommittedSequence, next)
full memory barrier
atomic_exchange(ProducerSequence, next)
SetEvent(InputEvent)
```

On Windows, use aligned `InterlockedCompareExchange64` for atomic reads and
`InterlockedExchange64` for publication. Do not rely on a language `volatile`
keyword alone for cross-process ordering.

Queue-full is a real error. Valid policies are to apply backpressure or report
failure to the owner. Do not overwrite an unread keyboard or button transition.
Movement-only reports can be coalesced in an application-owned staging area
before entering the queue.

### Driver Reader Algorithm

Under the per-device input lock:

```text
next = ConsumerSequence + 1
producer = atomic_load(ProducerSequence)
if next > producer:
    no report is available

slot = Slots[(next - 1) % SlotCount]
committed1 = atomic_load(slot.CommittedSequence)
if committed1 != next:
    publication is not complete; retry later

length = slot.Length
copy slot.Data into driver-owned memory
full memory barrier
committed2 = atomic_load(slot.CommittedSequence)

if committed1 != committed2 or committed2 != next:
    discard the copy and retry

validate length and report ID again
pair this report with one pending HID read
atomic_exchange(ConsumerSequence, next)
```

Treat the shared mapping as untrusted input even when its ACL is restricted.
Never use its length as a copy size before checking both the transport capacity
and descriptor-defined size.

### Pairing Reports and Reads Without a Lost Wakeup

Both paths call one `PumpReports` function while holding `InputLock`:

```text
READ_REPORT callback:
    lock InputLock
    forward request to PendingReadQueue
    PumpReports()
    unlock

input worker after InputEvent:
    lock InputLock
    PumpReports()
    unlock

PumpReports:
    while a pending read and a committed report both exist:
        retrieve one request from PendingReadQueue
        copy one report to its output buffer
        advance ConsumerSequence only after a successful copy
        complete the request outside the lock when practical
```

Queueing the read before pumping prevents this race: the event wakes the worker,
the worker sees no read, then a read is queued after the only event was already
consumed. The read callback's own pump observes the already committed report.

If request completion must happen outside the lock, collect paired requests and
driver-owned report copies under the lock, release it, then complete them. Never
hold the lock while waiting on an event or thread.

### Input Event and ACL

Create an auto-reset, initially nonsignaled event for each device:

```text
Global\YourVendorVirtualHidInputEvent<N>
```

Create the file mapping as:

```text
Global\YourVendorVirtualHidInput<N>
```

The service needs mapping read/write and event signal rights. The UMDF host
needs mapping read/write because it updates `ConsumerSequence`, plus event wait
rights. Prefer a service SID in the ACL. Rust Console uses one security
descriptor for both object types and grants `LocalService` generic-all access:
generic read/write on an event does not include the `SYNCHRONIZE` right required
by the driver's wait. The driver still requests only mapping read/write and
event modify/synchronize access. Do not grant `Everyone` write access: write
access is permission to inject system keyboard and mouse input.

## Input Worker

The worker waits on two handles:

```text
StopEvent   - private manual-reset event owned by the device context
InputEvent  - shared auto-reset report notification
```

Its loop is:

```text
while TearingDown == 0:
    result = WaitForMultipleObjects(StopEvent, InputEvent, timeout)

    if TearingDown != 0 or StopEvent signaled:
        exit

    if InputEvent signaled:
        lock InputLock
        PumpReports
        unlock

    if timeout:
        optionally validate that mapping owner and PnP state still exist

    if wait failed:
        record error and initiate controlled device failure
```

Do not poll every millisecond. Event-driven delivery avoids continuous CPU use.
A finite timeout is still useful for health checks and shutdown recovery.

If the application must restart while the device remains present, add an IPC
generation number and an explicit reconnection protocol. Do not silently open a
new mapping with the same name while the old producer may still own it.

## Mouse Descriptor and Reports

### Descriptor

The following descriptor defines five buttons, relative X/Y, vertical wheel,
horizontal pan, and absolute X/Y. Relative and absolute input use separate
top-level Mouse application collections and report IDs `1` and `2`. Both reports
are exactly 8 bytes including their ID.

```c
static const UCHAR MouseReportDescriptor[] = {
    /* Relative mouse, report 1 */
    0x05,0x01, 0x09,0x02, 0xA1,0x01,
    0x09,0x01, 0xA1,0x00, 0x85,0x01,
    0x05,0x09, 0x19,0x01, 0x29,0x05,
    0x15,0x00, 0x25,0x01, 0x95,0x05,
    0x75,0x01, 0x81,0x02,
    0x95,0x03, 0x75,0x01, 0x81,0x03,
    0x05,0x01, 0x09,0x30, 0x09,0x31,
    0x16,0x00,0x80, 0x26,0xFF,0x7F,
    0x75,0x10, 0x95,0x02, 0x81,0x06,
    0x09,0x38, 0x15,0x81, 0x25,0x7F,
    0x75,0x08, 0x95,0x01, 0x81,0x06,
    0x05,0x0C, 0x0A,0x38,0x02,
    0x15,0x81, 0x25,0x7F,
    0x75,0x08, 0x95,0x01, 0x81,0x06,
    0xC0, 0xC0,

    /* Absolute mouse, report 2 */
    0x05,0x01, 0x09,0x02, 0xA1,0x01,
    0x09,0x01, 0xA1,0x00, 0x85,0x02,
    0x05,0x09, 0x19,0x01, 0x29,0x05,
    0x15,0x00, 0x25,0x01, 0x95,0x05,
    0x75,0x01, 0x81,0x02,
    0x95,0x03, 0x75,0x01, 0x81,0x03,
    0x05,0x01, 0x09,0x30, 0x09,0x31,
    0x15,0x00, 0x26,0xFF,0x7F,
    0x75,0x10, 0x95,0x02, 0x81,0x02,
    0x75,0x08, 0x95,0x02, 0x81,0x03,
    0xC0, 0xC0
};
```

Validate this descriptor with a HID parser and a live Windows device before
freezing it as a compatibility contract. Changing it after release can change
how Windows groups collections and may require device re-enumeration.

### Relative Report

| Byte | Meaning |
|---:|---|
| `0` | Report ID `1` |
| `1` | Held buttons: left, right, middle, back, forward in bits `0..4` |
| `2..3` | Relative X, signed 16-bit little-endian |
| `4..5` | Relative Y, signed 16-bit little-endian |
| `6` | Vertical wheel, signed 8-bit, `-127..127` |
| `7` | Horizontal AC Pan, signed 8-bit, `-127..127` |

Movement and wheel reports must repeat the complete currently held button mask.
Relative movement and wheel values apply to one report only. Split values that
exceed the field range into multiple ordered reports instead of clamping and
losing motion.

### Absolute Report

| Byte | Meaning |
|---:|---|
| `0` | Report ID `2` |
| `1` | Complete held-button mask |
| `2..3` | Absolute X, unsigned little-endian, `0..32767` |
| `4..5` | Absolute Y, unsigned little-endian, `0..32767` |
| `6..7` | Zero padding |

Map a pixel coordinate to the descriptor range with:

```text
hid_x = clamp(round(x * 32767 / max(width  - 1, 1)), 0, 32767)
hid_y = clamp(round(y * 32767 / max(height - 1, 1)), 0, 32767)
```

Absolute reports must preserve held buttons. Omitting the mask generates an
unintended button release when changing report collections.

### Mouse State Rules

- Store one authoritative held-button bit mask.
- A button press changes one bit and submits the complete new mask.
- A button release clears one bit and submits the complete new mask.
- Every relative, absolute, and wheel report includes the current mask.
- On orderly shutdown, enqueue an all-buttons-released report and wait until
  the driver consumes it before removing the device.
- On queue overflow, do not update authoritative state unless its report was
  accepted. Otherwise producer and Windows state diverge.

## Keyboard Descriptor and Reports

### Descriptor Without LED Output

This standard boot-keyboard descriptor has an 8-byte input report and no report
ID:

```c
static const UCHAR KeyboardReportDescriptor[] = {
    0x05,0x01,       /* Usage Page (Generic Desktop) */
    0x09,0x06,       /* Usage (Keyboard) */
    0xA1,0x01,       /* Collection (Application) */
    0x05,0x07,       /* Usage Page (Keyboard/Keypad) */
    0x19,0xE0,       /* Usage Minimum (Left Control) */
    0x29,0xE7,       /* Usage Maximum (Right GUI) */
    0x15,0x00,       /* Logical Minimum (0) */
    0x25,0x01,       /* Logical Maximum (1) */
    0x75,0x01,       /* Report Size (1) */
    0x95,0x08,       /* Report Count (8) */
    0x81,0x02,       /* Input (Data, Variable, Absolute) */
    0x81,0x01,       /* Input (Constant): reserved byte */
    0x19,0x00,       /* Usage Minimum (No event) */
    0x29,0x65,       /* Usage Maximum (Keyboard Application) */
    0x15,0x00,
    0x25,0x65,
    0x75,0x08,
    0x95,0x06,
    0x81,0x00,       /* Input (Data, Array, Absolute) */
    0xC0
};
```

Input report:

| Byte | Meaning |
|---:|---|
| `0` | Modifier bit mask |
| `1` | Reserved, zero |
| `2..7` | Up to six held HID keyboard usage codes |

Modifier bits from least to most significant are left Control, left Shift,
left Alt, left GUI, right Control, right Shift, right Alt, and right GUI.

### Optional Keyboard LEDs

To receive Num Lock, Caps Lock, Scroll Lock, Compose, and Kana state, insert
this output block after the reserved input byte and before the six-key array:

```c
0x95,0x05,       /* Report Count (5) */
0x75,0x01,       /* Report Size (1) */
0x05,0x08,       /* Usage Page (LEDs) */
0x19,0x01,       /* Usage Minimum (Num Lock) */
0x29,0x05,       /* Usage Maximum (Kana) */
0x91,0x02,       /* Output (Data, Variable, Absolute) */
0x95,0x01,
0x75,0x03,
0x91,0x01        /* Output padding */
```

Then implement both `IOCTL_HID_WRITE_REPORT` and
`IOCTL_UMDF_HID_SET_OUTPUT_REPORT`. Their input starts with a report-ID byte;
for a no-ID keyboard it is zero, followed by the one-byte LED payload. Publish
that payload through a reverse IPC queue to the owning service.

### Keyboard State Rules

- Keep one modifier mask and one de-duplicated set of held ordinary usages.
- Rebuild and enqueue the complete 8-byte state after every accepted change.
- Remove only the requested usage on key-up.
- Modifier usages `0xE0..0xE7` belong in the modifier byte, not the six-key
  array.
- Usage zero means no event and should not be stored as a held key.
- On orderly shutdown, enqueue eight zero bytes and wait for consumption.

The six-key array cannot represent a seventh ordinary held key. Choose one
explicit policy:

- Reject the seventh transition and leave authoritative state unchanged.
  Pros: the caller receives an exact failure and no false report is sent.
  Cons: the caller must decide how to recover.
- Put HID `ErrorRollOver` usage `0x01` in all six slots until the held set fits
  again. Pros: follows boot-keyboard convention. Cons: consumers vary in how
  they handle rollover and cannot see the actual keys during it.

Never silently discard one held usage. That makes later key-up processing
ambiguous and can create stuck keys.

## Output Transport

Keyboard LEDs travel from the driver to the service. Use a second bounded ring
with reversed producer and consumer ownership:

```text
driver: producer
service: consumer
```

Each output record contains:

- Monotonic sequence.
- Operation kind: output report or feature write.
- Report ID.
- Validated payload length.
- Payload bytes.

Signal a separate auto-reset output event after publication. Apply the same
commit-sequence and memory-barrier rules as the input ring. Output overflow
should increment a visible counter and preserve the newest lock state only if
that replacement policy is explicitly implemented and tested.

If LED state is not needed, omit the output descriptor fields, output IOCTL
handling, reverse ring, and output worker together.

## Report Descriptor Validation

Do not trust a configured `InputReportByteLength` without parsing the
descriptor. During device creation, walk every HID short item and track global
state:

```text
Report Size
Report Count
Report ID, zero when absent
Push and Pop global-state items
```

For each `Input` main item:

```text
input_bits[current_report_id] += ReportSize * ReportCount
```

For each report ID:

```text
wire_length = ceil(input_bits[id] / 8)
if the descriptor uses report IDs:
    wire_length += 1
```

Validate that:

- Report ID zero is not used when any nonzero report ID exists.
- Every submitted report ID has an input layout.
- Every input report fits the driver and IPC capacities.
- Mouse report IDs 1 and 2 both resolve to 8 bytes.
- The no-ID keyboard resolves to 8 bytes.
- Output and feature report lengths are independently computed.
- Collection nesting is balanced and long items are either supported or
  rejected.

Keep a table of valid input IDs and exact lengths in the immutable driver
context. Validate every shared report against that table. Do not scan for the
first `0x85` byte and assume it applies to every report.

Because the proposed mouse reports have equal lengths and the keyboard is a
separate device, each PnP device can advertise one fixed
`InputReportByteLength` of 8 to HIDClass.

## Teardown

### Orderly Removal

Application or service:

1. Stop accepting new input.
2. Enqueue all-released state.
3. Wait with a bounded timeout for `ConsumerSequence` to reach that report.
4. Request removal of the exact owned PnP parent.
5. Wait for the HID child and parent to disappear, including phantom nodes.
6. Close mapping and event handles.
7. Delete only that device's configuration key.

Driver cleanup callback:

```text
atomically set TearingDown = 1
signal StopEvent
wait for WorkerThread to exit
if it does not exit, do not unmap memory it can still access
close worker handle
unmap and close input/output mappings
close input/output/stop events
purge pending read requests with STATUS_DEVICE_REMOVED
```

Thread termination must be guaranteed before unmapping. A short timeout
followed by unconditional unmapping creates a use-after-unmap race.

### PnP Removal

Use the stored parent instance ID and verify ownership before removal:

1. Enumerate and remember current child instance IDs.
2. Call `SetupDiRemoveDevice` or the class installer with `DIF_REMOVE` for the
   exact parent.
3. Wait for each child and parent to disappear.
4. If normal removal fails, use `pnputil /remove-device <instance-id>` as an
   explicit fallback and report that fallback to the caller.
5. Confirm no live or phantom node remains before reusing the index or IPC name.

On the next service start, clean stale devices only when their private hardware
ID, instance token, and configuration ownership all match. Never use VID/PID
alone as proof of ownership.

### Producer Crash

The driver cannot infer key release merely because the producer stopped. Use
one of these explicit policies:

- A supervising service removes the virtual devices when the input client dies.
- A lease heartbeat expires and the service first submits all-released state,
  then removes the devices.
- Devices belong to a persistent service, and client disconnect always invokes
  release-all while the service stays alive.

Do not have the driver synthesize releases after an arbitrary short timeout.
Long key holds and drags are valid input.

## Security Requirements

Treat every boundary as hostile:

- Only the installer and owning service may modify machine configuration.
- Only the owning service may write input reports.
- The UMDF host may read input reports and update the consumer sequence.
- Only the driver may publish host output reports.
- Unprivileged clients reach the service through an authenticated, restricted
  IPC API, not by opening the shared mapping directly.
- Validate report size, report ID, descriptor limits, registry string lengths,
  index ranges, and arithmetic before every copy or allocation.
- Include random instance tokens in names or configuration to prevent stale
  same-index objects from attaching to a new device.
- Avoid a null DACL and avoid generic-all rights for `Everyone` on the device,
  mappings, or events.

Keyboard and mouse injection is a security-sensitive capability. The service
must apply the same caller authorization expected for direct interactive input.

## Failure Rules

Fail device creation when any required component fails:

- Driver package not installed or not trusted.
- Configuration missing or inconsistent.
- Mapping or event cannot be created or opened.
- Worker or WDF queue creation fails.
- INF binding fails.
- Parent does not start.
- HID child does not enumerate.
- Descriptor validation fails.

At runtime:

- Queue full is reported, not silently ignored.
- Invalid shared data is rejected and counted without copying out of bounds.
- A broken wait handle or worker failure marks the device unhealthy.
- Report submission after teardown begins fails immediately.
- Cleanup errors are returned alongside the primary error.
- Logs identify the device by private index and token, not only VID/PID.

## Validation

### Device-Free Tests

Test the descriptor parser against:

- No report IDs.
- Multiple equal-size report IDs.
- Different report sizes, which this fixed-read design rejects.
- Push and Pop global state.
- Malformed collection nesting.
- Oversized fields and integer overflow.

Test report encoding byte for byte:

- Negative and maximum relative mouse movement.
- Both wheel directions and horizontal pan.
- Every mouse button and held buttons during absolute movement.
- Absolute coordinates at all four corners.
- Left and right keyboard modifiers.
- Duplicate usages, release order, six keys, and the seventh-key policy.
- Release-all reports.

Test the report ring with forced thread interleavings:

- Reader before publication.
- Reader during an uncommitted slot write.
- Event before a HID read is queued.
- HID read before a report exists.
- Full queue and wrap through many ring cycles.
- Producer and consumer sequences near 64-bit wrap.
- Process termination during publication.
- Invalid length and report ID written directly into shared memory.

### Live Windows Tests

Use a disposable Windows test system:

1. Install, upgrade, and uninstall the signed package.
2. Create mouse and keyboard together and verify separate parent and HID child
   nodes.
3. Read back device, report, and string descriptors through the HID API.
4. Confirm the system mouse receives relative movement, both wheel axes, all
   buttons, and absolute corners.
5. Hold a button across relative and absolute reports and confirm no release.
6. Confirm keyboard chords, left/right modifiers, six-key state, rollover
   policy, and release-all.
7. Confirm Caps Lock and other LEDs if output reports are enabled.
8. Send down/up faster than the worker can run and prove both reports arrive.
9. Fill the ring and verify the selected backpressure behavior.
10. Kill the input client and service separately while controls are held.
11. Create, remove, and recreate both devices in both orders.
12. Reboot with stale configuration and verify ownership-safe recovery.
13. Run under a standard user and prove direct injection is denied.
14. Run Driver Verifier and Application Verifier over creation, traffic, and
    teardown.

Capture the actual request buffers received for `GET_STRING`, `GET_FEATURE`,
and `GET_INPUT_REPORT` on every supported Windows version. UMDF buffer
conventions for requested report IDs must be verified against the target WDK
and runtime rather than inferred.

## Implementation Checklist

- [ ] Choose project-owned service, hardware ID, registry, mapping, and event
      names.
- [ ] Choose legal VID/PID and stable per-device serial behavior.
- [ ] Build and sign a minimal UMDF2 HID package.
- [ ] Implement exact SetupAPI creation, ownership recording, and rollback.
- [ ] Implement strict descriptor parsing and fixed report-length validation.
- [ ] Implement descriptor, attribute, string, read, and optional output IOCTLs.
- [ ] Implement the bounded report ring with interlocked publication.
- [ ] Pair reports and pending reads without lost wakeups or duplicates.
- [ ] Encode the 8-byte mouse and keyboard reports exactly.
- [ ] Preserve complete held state in every report.
- [ ] Restrict all registry, device, mapping, event, and client IPC ACLs.
- [ ] Guarantee worker exit before unmapping on teardown.
- [ ] Implement release-all and producer-crash ownership policy.
- [ ] Pass device-free, live Windows, verifier, upgrade, and removal tests.
