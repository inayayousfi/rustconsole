//! Direct capture from VB-CABLE with conditional Windows default restoration.

use rustconsole_host_core::AudioCaptureEvent;
use rustconsole_media::{AudioFormat, AudioSamples, MediaTimestampMicros};
use std::time::{Duration, Instant};

const FORMAT: AudioFormat = AudioFormat {
    sample_rate: 48_000,
    channels: 2,
};
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const MAX_BUFFER_FRAMES: u32 = 4_800;

#[derive(Default)]
struct DeviceSelection {
    opened: Option<String>,
    next_check: Option<Instant>,
}

impl DeviceSelection {
    fn due(&self, now: Instant) -> bool {
        self.next_check.is_none_or(|deadline| now >= deadline)
    }

    fn changed(&mut self, device: Option<&str>, now: Instant) -> bool {
        self.next_check = Some(now + DEVICE_CHECK_INTERVAL);
        self.opened.as_deref() != device
    }

    fn lost(&mut self, now: Instant) {
        self.opened = None;
        self.next_check = Some(now + DEVICE_CHECK_INTERVAL);
    }
}

fn packet_event(
    samples: Vec<f32>,
    qpc_100ns: u64,
    timestamp_error: bool,
    discontinuity: bool,
    last_timestamp_micros: &mut Option<u64>,
) -> AudioCaptureEvent {
    if timestamp_error {
        return AudioCaptureEvent::InvalidTimestamp;
    }
    let captured_at_micros = qpc_100ns / 10;
    if last_timestamp_micros.is_some_and(|previous| captured_at_micros <= previous) {
        return AudioCaptureEvent::InvalidTimestamp;
    }
    *last_timestamp_micros = Some(captured_at_micros);
    AudioCaptureEvent::Samples {
        samples: AudioSamples {
            captured_at: MediaTimestampMicros(captured_at_micros),
            format: FORMAT,
            interleaved: samples,
        },
        discontinuity,
    }
}

#[cfg(windows)]
pub use native::{CableCapture, vb_cable_available};

#[cfg(windows)]
mod native {
    use super::*;
    use crate::audio_policy::{
        AudioRoute, CableEndpointIds, EndpointCandidate, EndpointFlow, select_cable_endpoints,
    };
    use rustconsole_host_core::SystemAudioCapture;
    use std::marker::PhantomData;
    use std::rc::Rc;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_DeviceInterface_FriendlyName;
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    };
    use windows::core::{Error, HRESULT, Interface, PCWSTR, Result};

    const ENDPOINT_NOT_FOUND: HRESULT = HRESULT::from_win32(ERROR_NOT_FOUND.0);

    struct ComThread(PhantomData<Rc<()>>);

    impl Drop for ComThread {
        fn drop(&mut self) {
            // SAFETY: constructed only after successful initialization on this thread;
            // Rc's marker prevents moving the owner to another thread.
            unsafe { CoUninitialize() };
        }
    }

    struct Stream {
        capture: IAudioCaptureClient,
        client: IAudioClient,
        buffer_frames: u32,
        last_timestamp_micros: Option<u64>,
    }

    impl Stream {
        fn open(device: &IMMDevice) -> Result<Self> {
            // SAFETY: COM is initialized on this thread, and all format/output storage
            // remains valid throughout the synchronous calls.
            unsafe {
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                let format = WAVEFORMATEX {
                    wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
                    nChannels: 2,
                    nSamplesPerSec: 48_000,
                    nAvgBytesPerSec: 48_000 * 8,
                    nBlockAlign: 8,
                    wBitsPerSample: 32,
                    cbSize: 0,
                };
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    200_000,
                    0,
                    &format,
                    None,
                )?;
                let buffer_frames = client.GetBufferSize()?;
                if buffer_frames == 0 || buffer_frames > MAX_BUFFER_FRAMES {
                    return Err(Error::new(
                        HRESULT(0x80070057_u32 as i32),
                        "audio engine buffer exceeds the 100 ms capture bound",
                    ));
                }
                let capture = client.GetService()?;
                client.Start()?;
                Ok(Self {
                    capture,
                    client,
                    buffer_frames,
                    last_timestamp_micros: None,
                })
            }
        }

        fn read(&mut self) -> Result<AudioCaptureEvent> {
            // SAFETY: acquisition and release occur on the same COM thread. The
            // engine buffer is copied only while acquired, with the negotiated format.
            unsafe {
                if self.capture.GetNextPacketSize()? == 0 {
                    return Ok(AudioCaptureEvent::Idle);
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0;
                let mut flags = 0;
                let mut timestamp = 0;
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut timestamp),
                )?;
                if frames == 0 {
                    return Ok(AudioCaptureEvent::Idle);
                }
                let result = (|| {
                    if frames > self.buffer_frames {
                        return Err(Error::new(
                            HRESULT(0x80070057_u32 as i32),
                            "capture packet exceeds the engine buffer",
                        ));
                    }
                    let invalid_timestamp =
                        flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0;
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    let count = frames as usize * 2;
                    let samples = if invalid_timestamp {
                        Vec::new()
                    } else if silent {
                        vec![0.0; count]
                    } else {
                        if data.is_null() {
                            return Err(Error::new(
                                HRESULT(0x80004003_u32 as i32),
                                "capture returned null non-silent samples",
                            ));
                        }
                        (0..count)
                            .map(|index| data.cast::<f32>().add(index).read_unaligned())
                            .collect()
                    };
                    Ok(packet_event(
                        samples,
                        timestamp,
                        invalid_timestamp,
                        flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
                        &mut self.last_timestamp_micros,
                    ))
                })();
                self.capture.ReleaseBuffer(frames)?;
                result
            }
        }
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: the owning COM thread still exists. Releasing the interfaces
            // also ends capture if an invalidated device can no longer be stopped.
            if let Err(error) = unsafe { self.client.Stop() } {
                eprintln!("audio capture stop: {error}");
            }
        }
    }

    pub struct CableCapture {
        stream: Option<Stream>,
        route: Option<AudioRoute>,
        devices: IMMDeviceEnumerator,
        selection: DeviceSelection,
        discontinuity: bool,
        pub device_reopens: u64,
        pub last_unavailable_reason: Option<String>,
        // Declared last so every COM interface is released before CoUninitialize.
        _com: ComThread,
    }

    impl CableCapture {
        pub fn new() -> Result<Self> {
            // SAFETY: this owner balances initialization and cannot leave this thread.
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
                let com = ComThread(PhantomData);
                let devices = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                Ok(Self {
                    stream: None,
                    route: None,
                    devices,
                    selection: DeviceSelection::default(),
                    discontinuity: true,
                    device_reopens: 0,
                    last_unavailable_reason: None,
                    _com: com,
                })
            }
        }

        pub fn device_id(&self) -> Option<&str> {
            self.selection.opened.as_deref()
        }

        fn poll(&mut self) -> Result<AudioCaptureEvent> {
            let now = Instant::now();
            if self.selection.due(now) {
                let endpoints = match cable_endpoint_ids(&self.devices) {
                    Ok(endpoints) => Some(endpoints),
                    Err(error) if error.code() == ENDPOINT_NOT_FOUND => {
                        self.last_unavailable_reason = Some(error.to_string());
                        None
                    }
                    Err(error) => return Err(error),
                };
                let capture_id = endpoints
                    .as_ref()
                    .map(|endpoints| endpoints.capture.as_str());
                if self.selection.changed(capture_id, now) {
                    self.stream = None;
                    self.route = None;
                    self.selection.opened = None;
                    self.discontinuity = true;
                    if let Some(endpoints) = endpoints {
                        let device = get_device(&self.devices, &endpoints.capture)?;
                        let route = AudioRoute::activate(&self.devices, endpoints.render)?;
                        self.stream = Some(Stream::open(&device)?);
                        self.route = Some(route);
                        self.selection.opened = Some(endpoints.capture);
                        self.device_reopens += 1;
                        self.last_unavailable_reason = None;
                    }
                }
                if self.stream.is_none() {
                    self.last_unavailable_reason
                        .get_or_insert_with(|| "VB-CABLE Standard is unavailable".into());
                }
            }
            let Some(stream) = self.stream.as_mut() else {
                return Ok(AudioCaptureEvent::Unavailable);
            };
            let mut event = stream.read()?;
            if let AudioCaptureEvent::Samples { discontinuity, .. } = &mut event {
                *discontinuity |= std::mem::take(&mut self.discontinuity);
            } else if matches!(event, AudioCaptureEvent::InvalidTimestamp) {
                self.discontinuity = true;
            }
            Ok(event)
        }
    }

    impl SystemAudioCapture for CableCapture {
        type Error = Error;

        fn next_samples(&mut self) -> Result<AudioCaptureEvent> {
            match self.poll() {
                Err(error)
                    if matches!(
                        error.code(),
                        AUDCLNT_E_DEVICE_INVALIDATED
                            | AUDCLNT_E_RESOURCES_INVALIDATED
                            | AUDCLNT_E_SERVICE_NOT_RUNNING
                            | AUDCLNT_E_DEVICE_IN_USE
                            | ENDPOINT_NOT_FOUND
                    ) =>
                {
                    self.stream = None;
                    self.selection.lost(Instant::now());
                    self.discontinuity = true;
                    self.last_unavailable_reason = Some(error.to_string());
                    Ok(AudioCaptureEvent::Unavailable)
                }
                result => result,
            }
        }

        fn reset(&mut self) -> Result<()> {
            // SAFETY: stop on the owning COM thread before releasing its resources.
            // Stop is idempotent; Drop remains the fallback for early returns.
            let stopped = self
                .stream
                .as_ref()
                .map(|stream| unsafe { stream.client.Stop() })
                .transpose();
            self.stream = None;
            self.selection = DeviceSelection::default();
            self.discontinuity = true;
            stopped?;
            if let Some(mut route) = self.route.take() {
                route.restore()?;
            }
            Ok(())
        }
    }

    pub fn vb_cable_available() -> Result<bool> {
        // SAFETY: this function owns every COM interface it creates and releases
        // them before balancing COM initialization on the calling thread.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let _com = ComThread(PhantomData);
            let devices = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            match cable_endpoint_ids(&devices) {
                Ok(_) => Ok(true),
                Err(error) if error.code() == ENDPOINT_NOT_FOUND => Ok(false),
                Err(error) => Err(error),
            }
        }
    }

    fn cable_endpoint_ids(devices: &IMMDeviceEnumerator) -> Result<CableEndpointIds> {
        let collection = unsafe { devices.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE)? };
        let count = unsafe { collection.GetCount()? };
        let mut candidates = Vec::with_capacity(count as usize);
        for index in 0..count {
            let device = unsafe { collection.Item(index)? };
            let endpoint: IMMEndpoint = device.cast()?;
            let data_flow = unsafe { endpoint.GetDataFlow()? };
            let flow = if data_flow == eRender {
                EndpointFlow::Render
            } else if data_flow == eCapture {
                EndpointFlow::Capture
            } else {
                continue;
            };
            let properties = match unsafe { device.OpenPropertyStore(STGM_READ) } {
                Ok(properties) => properties,
                Err(_) => continue,
            };
            candidates.push(EndpointCandidate {
                id: device_id(&device)?,
                flow,
                interface_name: property_string(&properties, &PKEY_DeviceInterface_FriendlyName)
                    .unwrap_or_default(),
                jack_sub_type: property_string(&properties, &PKEY_AudioEndpoint_JackSubType)
                    .unwrap_or_default(),
            });
        }
        select_cable_endpoints(&candidates)
            .map_err(|message| Error::new(ENDPOINT_NOT_FOUND, message))
    }

    fn get_device(devices: &IMMDeviceEnumerator, id: &str) -> Result<IMMDevice> {
        let wide = id.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        unsafe { devices.GetDevice(PCWSTR(wide.as_ptr())) }
    }

    fn device_id(device: &IMMDevice) -> Result<String> {
        unsafe {
            let id = device.GetId()?;
            let result = id.to_string().map_err(Error::from);
            CoTaskMemFree(Some(id.0.cast()));
            result
        }
    }

    fn property_string(
        properties: &windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore,
        key: &windows::Win32::Foundation::PROPERTYKEY,
    ) -> Result<String> {
        unsafe {
            let value = properties.GetValue(key)?;
            let text = PropVariantToStringAlloc(&value)?;
            let result = text.to_string().map_err(Error::from);
            CoTaskMemFree(Some(text.0.cast()));
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_device_retries_without_busy_loop() {
        let now = Instant::now();
        let mut state = DeviceSelection::default();
        assert!(state.due(now));
        assert!(!state.changed(None, now));
        assert!(!state.due(now + Duration::from_millis(249)));
        assert!(state.due(now + DEVICE_CHECK_INTERVAL));
        assert!(state.changed(Some("speakers"), now + DEVICE_CHECK_INTERVAL));
    }

    #[test]
    fn default_replacement_loss_and_return_require_reopen() {
        let now = Instant::now();
        let mut state = DeviceSelection {
            opened: Some("speakers".into()),
            next_check: None,
        };
        assert!(!state.changed(Some("speakers"), now));
        assert!(state.changed(Some("headphones"), now));
        state.opened = Some("headphones".into());
        assert!(state.changed(None, now));
        state.lost(now);
        assert!(!state.due(now));
        assert!(state.changed(Some("headphones"), now + DEVICE_CHECK_INTERVAL));
    }

    #[test]
    fn failed_open_remains_retryable() {
        let now = Instant::now();
        let mut state = DeviceSelection::default();
        assert!(state.changed(Some("speakers"), now));
        state.lost(now);
        assert!(state.due(now + DEVICE_CHECK_INTERVAL));
        assert!(state.changed(Some("speakers"), now + DEVICE_CHECK_INTERVAL));
    }

    #[test]
    fn stereo_samples_preserve_order_and_convert_windows_time_units() {
        let event = packet_event(vec![0.25, -0.5, 0.75, -1.0], 12_349, false, true, &mut None);
        let AudioCaptureEvent::Samples {
            samples,
            discontinuity,
        } = event
        else {
            panic!()
        };
        assert_eq!(samples.format, FORMAT);
        assert_eq!(samples.captured_at, MediaTimestampMicros(1_234));
        assert_eq!(samples.interleaved, [0.25, -0.5, 0.75, -1.0]);
        assert!(discontinuity);
        assert_eq!(MAX_BUFFER_FRAMES, FORMAT.sample_rate / 10);
    }

    #[test]
    fn silent_packet_is_not_a_missing_packet() {
        let event = packet_event(vec![0.0; 960], 10_000, false, false, &mut None);
        assert!(matches!(event, AudioCaptureEvent::Samples { .. }));
    }

    #[test]
    fn invalid_or_non_increasing_timestamp_discards_packet_until_time_advances() {
        let mut last = None;
        assert_eq!(
            packet_event(vec![1.0; 4], 0, true, false, &mut last),
            AudioCaptureEvent::InvalidTimestamp
        );
        assert_eq!(last, None);
        assert!(matches!(
            packet_event(vec![1.0; 4], 10_000, false, false, &mut last),
            AudioCaptureEvent::Samples { .. }
        ));
        assert_eq!(last, Some(1_000));
        assert_eq!(
            packet_event(vec![1.0; 4], 10_000, false, false, &mut last),
            AudioCaptureEvent::InvalidTimestamp
        );
        assert_eq!(last, Some(1_000));
        assert!(matches!(
            packet_event(vec![1.0; 4], 20_000, false, false, &mut last),
            AudioCaptureEvent::Samples { .. }
        ));
    }
}
