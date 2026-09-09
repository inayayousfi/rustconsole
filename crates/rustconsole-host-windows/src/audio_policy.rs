const CABLE_INTERFACE_NAME: &str = "VB-Audio Virtual Cable";
const CABLE_RENDER_JACK_SUB_TYPE: &str = "{DFF21CE1-F70F-11D0-B917-00A0C9223196}";
const CABLE_CAPTURE_JACK_SUB_TYPE: &str = "{DFF21FE3-F70F-11D0-B917-00A0C9223196}";
const RECORD_VERSION: &str = "1";
const MAX_RECORD_BYTES: usize = 4_096;
const MAX_ENDPOINT_ID_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EndpointFlow {
    Render,
    Capture,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EndpointCandidate {
    pub id: String,
    pub flow: EndpointFlow,
    pub interface_name: String,
    pub jack_sub_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CableEndpointIds {
    pub render: String,
    pub capture: String,
}

pub(crate) fn select_cable_endpoints(
    candidates: &[EndpointCandidate],
) -> Result<CableEndpointIds, &'static str> {
    let matching = |flow, jack_sub_type: &str| {
        candidates
            .iter()
            .filter(|candidate| {
                candidate.flow == flow
                    && candidate.interface_name == CABLE_INTERFACE_NAME
                    && candidate.jack_sub_type.eq_ignore_ascii_case(jack_sub_type)
            })
            .map(|candidate| candidate.id.as_str())
            .collect::<Vec<_>>()
    };
    let render = matching(EndpointFlow::Render, CABLE_RENDER_JACK_SUB_TYPE);
    let capture = matching(EndpointFlow::Capture, CABLE_CAPTURE_JACK_SUB_TYPE);
    if render.len() != 1 || capture.len() != 1 {
        return Err("VB-CABLE Standard endpoints are absent or ambiguous");
    }
    Ok(CableEndpointIds {
        render: render[0].to_owned(),
        capture: capture[0].to_owned(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RouteRecord {
    session_id: u32,
    target: String,
    originals: [String; 3],
}

impl RouteRecord {
    fn serialize(&self) -> Result<String, &'static str> {
        for id in std::iter::once(&self.target).chain(self.originals.iter()) {
            validate_endpoint_id(id)?;
        }
        Ok(format!(
            "version={RECORD_VERSION}\nsession_id={}\ntarget={}\nconsole={}\nmultimedia={}\ncommunications={}\n",
            self.session_id, self.target, self.originals[0], self.originals[1], self.originals[2],
        ))
    }

    fn parse(text: &str) -> Result<Self, &'static str> {
        if text.len() > MAX_RECORD_BYTES {
            return Err("audio route recovery record exceeds its size bound");
        }
        let mut lines = text.lines();
        if lines.next() != Some("version=1") {
            return Err("invalid audio route recovery version");
        }
        let session_id = value(&mut lines, "session_id=")?
            .parse()
            .map_err(|_| "invalid audio route recovery session")?;
        let target = value(&mut lines, "target=")?.to_owned();
        let originals = [
            value(&mut lines, "console=")?.to_owned(),
            value(&mut lines, "multimedia=")?.to_owned(),
            value(&mut lines, "communications=")?.to_owned(),
        ];
        if lines.next().is_some() {
            return Err("unexpected audio route recovery field");
        }
        for id in std::iter::once(&target).chain(originals.iter()) {
            validate_endpoint_id(id)?;
        }
        Ok(Self {
            session_id,
            target,
            originals,
        })
    }
}

fn value<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    prefix: &str,
) -> Result<&'a str, &'static str> {
    lines
        .next()
        .and_then(|line| line.strip_prefix(prefix))
        .ok_or("missing audio route recovery field")
}

fn validate_endpoint_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty()
        || id.len() > MAX_ENDPOINT_ID_BYTES
        || id.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
    {
        return Err("invalid audio endpoint identifier");
    }
    Ok(())
}

fn target_writes(
    current: &[String; 3],
    originals: &[String; 3],
    target: &str,
) -> Result<[bool; 3], &'static str> {
    if current
        .iter()
        .zip(originals)
        .any(|(current, original)| current != original && current != target)
    {
        return Err("an audio role changed while VB-CABLE routing was being applied");
    }
    Ok(std::array::from_fn(|index| current[index] != target))
}

fn restoration_writes(current: &[String; 3], target: &str) -> [bool; 3] {
    std::array::from_fn(|index| current[index] == target)
}

fn route_recovered(current: &[String; 3], originals: &[String; 3], target: &str) -> bool {
    !current
        .iter()
        .zip(originals)
        .any(|(current, original)| current == target && original != target)
}

#[cfg(windows)]
mod native {
    use super::*;
    use crate::credentials::store_restricted_file;
    use std::ffi::c_void;
    use std::fs;
    use std::marker::PhantomData;
    use std::path::Path;
    use std::rc::Rc;
    use std::time::Duration;
    use windows::Win32::Media::Audio::{
        ERole, IMMDeviceEnumerator, eCommunications, eConsole, eMultimedia, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    };
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::core::{Error, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PCWSTR, Result};

    pub(crate) const ROUTE_RECORD_PATH: &str =
        r"C:\ProgramData\RustConsole\audio-route-recovery.txt";
    const ROLES: [ERole; 3] = [eConsole, eMultimedia, eCommunications];
    const POLICY_CONFIG_CLASS: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

    #[repr(transparent)]
    #[derive(Clone, PartialEq, Eq)]
    struct IPolicyConfig(IUnknown);

    unsafe impl Interface for IPolicyConfig {
        type Vtable = IPolicyConfig_Vtbl;
        const IID: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct IPolicyConfig_Vtbl {
        base__: IUnknown_Vtbl,
        GetMixFormat: usize,
        GetDeviceFormat: usize,
        ResetDeviceFormat: usize,
        SetDeviceFormat: usize,
        GetProcessingPeriod: usize,
        SetProcessingPeriod: usize,
        GetShareMode: usize,
        SetShareMode: usize,
        GetPropertyValue: usize,
        SetPropertyValue: usize,
        SetDefaultEndpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
        SetEndpointVisibility: usize,
    }

    impl IPolicyConfig {
        fn create() -> Result<Self> {
            unsafe { CoCreateInstance(&POLICY_CONFIG_CLASS, None, CLSCTX_ALL) }
        }

        fn set_default_endpoint(&self, id: &str, role: ERole) -> Result<()> {
            let wide = id.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            unsafe {
                (Interface::vtable(self).SetDefaultEndpoint)(
                    Interface::as_raw(self),
                    PCWSTR(wide.as_ptr()),
                    role,
                )
                .ok()
            }
        }
    }

    struct ComThread(PhantomData<Rc<()>>);

    impl ComThread {
        fn new() -> Result<Self> {
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
            Ok(Self(PhantomData))
        }
    }

    impl Drop for ComThread {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub(crate) struct AudioRoute {
        record: RouteRecord,
        policy: IPolicyConfig,
        devices: IMMDeviceEnumerator,
        restored: bool,
    }

    impl AudioRoute {
        pub(crate) fn activate(devices: &IMMDeviceEnumerator, target: String) -> Result<Self> {
            let originals = current_defaults(devices)?;
            let record = RouteRecord {
                session_id: current_session_id()?,
                target,
                originals,
            };
            let policy = IPolicyConfig::create()?;
            let serialized = record
                .serialize()
                .map_err(|message| Error::new(HRESULT(0x8007000d_u32 as i32), message))?;
            store_restricted_file(Path::new(ROUTE_RECORD_PATH), serialized.as_bytes())
                .map_err(Error::from)?;
            let mut route = Self {
                record,
                policy,
                devices: devices.clone(),
                restored: false,
            };
            if let Err(error) = route.apply() {
                let _ = route.restore();
                return Err(error);
            }
            Ok(route)
        }

        fn apply(&mut self) -> Result<()> {
            for (index, role) in ROLES.into_iter().enumerate() {
                let current = current_defaults(&self.devices)?;
                let writes =
                    target_writes(&current, &self.record.originals, &self.record.target)
                        .map_err(|message| Error::new(HRESULT(0x800704c7_u32 as i32), message))?;
                if writes[index] {
                    self.policy
                        .set_default_endpoint(&self.record.target, role)?;
                    wait_for_default(&self.devices, role, &self.record.target)?;
                }
            }
            if current_defaults(&self.devices)?
                != [
                    self.record.target.clone(),
                    self.record.target.clone(),
                    self.record.target.clone(),
                ]
            {
                return Err(Error::new(
                    HRESULT(0x800705b4_u32 as i32),
                    "Windows did not route every audio role to VB-CABLE",
                ));
            }
            Ok(())
        }

        pub(crate) fn restore(&mut self) -> Result<()> {
            if self.restored {
                return Ok(());
            }
            restore_record(&self.policy, &self.devices, &self.record)?;
            self.restored = true;
            remove_record_if_recovered(&self.devices, &self.record)
        }
    }

    impl Drop for AudioRoute {
        fn drop(&mut self) {
            if let Err(error) = self.restore() {
                eprintln!("audio route restoration: {error}");
            }
        }
    }

    pub(crate) fn recover_pending_route() -> Result<()> {
        let text = match fs::read_to_string(ROUTE_RECORD_PATH) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::from(error)),
        };
        let record = RouteRecord::parse(&text)
            .map_err(|message| Error::new(HRESULT(0x8007000d_u32 as i32), message))?;
        if unsafe { WTSGetActiveConsoleSessionId() } != record.session_id {
            return Ok(());
        }
        let _com = ComThread::new()?;
        let devices = unsafe {
            CoCreateInstance::<_, IMMDeviceEnumerator>(
                &windows::Win32::Media::Audio::MMDeviceEnumerator,
                None,
                CLSCTX_ALL,
            )?
        };
        let policy = IPolicyConfig::create()?;
        restore_record(&policy, &devices, &record)?;
        remove_record_if_recovered(&devices, &record)
    }

    fn restore_record(
        policy: &IPolicyConfig,
        devices: &IMMDeviceEnumerator,
        record: &RouteRecord,
    ) -> Result<()> {
        for (index, role) in ROLES.into_iter().enumerate() {
            let current = current_defaults(devices)?;
            if restoration_writes(&current, &record.target)[index] {
                policy.set_default_endpoint(&record.originals[index], role)?;
                wait_for_default(devices, role, &record.originals[index])?;
            }
        }
        Ok(())
    }

    fn remove_record_if_recovered(
        devices: &IMMDeviceEnumerator,
        record: &RouteRecord,
    ) -> Result<()> {
        if !route_recovered(
            &current_defaults(devices)?,
            &record.originals,
            &record.target,
        ) {
            return Err(Error::new(
                HRESULT(0x800705b4_u32 as i32),
                "one or more audio roles still point to VB-CABLE",
            ));
        }
        match fs::remove_file(ROUTE_RECORD_PATH) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::from(error)),
        }
    }

    fn current_defaults(devices: &IMMDeviceEnumerator) -> Result<[String; 3]> {
        let mut current = [String::new(), String::new(), String::new()];
        for (index, role) in ROLES.into_iter().enumerate() {
            let device = unsafe { devices.GetDefaultAudioEndpoint(eRender, role)? };
            current[index] = device_id(&device)?;
        }
        Ok(current)
    }

    fn wait_for_default(devices: &IMMDeviceEnumerator, role: ERole, expected: &str) -> Result<()> {
        for _ in 0..20 {
            let device = unsafe { devices.GetDefaultAudioEndpoint(eRender, role)? };
            if device_id(&device)? == expected {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(Error::new(
            HRESULT(0x800705b4_u32 as i32),
            "timed out waiting for the Windows audio default",
        ))
    }

    fn device_id(device: &windows::Win32::Media::Audio::IMMDevice) -> Result<String> {
        unsafe {
            let id = device.GetId()?;
            let result = id.to_string().map_err(Error::from);
            CoTaskMemFree(Some(id.0.cast()));
            result
        }
    }

    fn current_session_id() -> Result<u32> {
        let mut session_id = 0;
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id)? };
        Ok(session_id)
    }
}

#[cfg(windows)]
pub(crate) use native::{AudioRoute, recover_pending_route};

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        id: &str,
        flow: EndpointFlow,
        interface: &str,
        jack_sub_type: &str,
    ) -> EndpointCandidate {
        EndpointCandidate {
            id: id.into(),
            flow,
            interface_name: interface.into(),
            jack_sub_type: jack_sub_type.into(),
        }
    }

    #[test]
    fn selects_one_standard_cable_pair() {
        let candidates = [
            candidate(
                "render",
                EndpointFlow::Render,
                CABLE_INTERFACE_NAME,
                CABLE_RENDER_JACK_SUB_TYPE,
            ),
            candidate(
                "capture",
                EndpointFlow::Capture,
                CABLE_INTERFACE_NAME,
                CABLE_CAPTURE_JACK_SUB_TYPE,
            ),
            candidate(
                "sixteen-channel",
                EndpointFlow::Render,
                CABLE_INTERFACE_NAME,
                CABLE_CAPTURE_JACK_SUB_TYPE,
            ),
            candidate(
                "other",
                EndpointFlow::Render,
                "Other",
                CABLE_RENDER_JACK_SUB_TYPE,
            ),
        ];
        assert_eq!(
            select_cable_endpoints(&candidates).unwrap(),
            CableEndpointIds {
                render: "render".into(),
                capture: "capture".into(),
            }
        );
    }

    #[test]
    fn missing_or_ambiguous_cable_is_rejected() {
        let duplicate = [
            candidate(
                "render-a",
                EndpointFlow::Render,
                CABLE_INTERFACE_NAME,
                CABLE_RENDER_JACK_SUB_TYPE,
            ),
            candidate(
                "render-b",
                EndpointFlow::Render,
                CABLE_INTERFACE_NAME,
                CABLE_RENDER_JACK_SUB_TYPE,
            ),
            candidate(
                "capture",
                EndpointFlow::Capture,
                CABLE_INTERFACE_NAME,
                CABLE_CAPTURE_JACK_SUB_TYPE,
            ),
        ];
        assert!(select_cable_endpoints(&[]).is_err());
        assert!(select_cable_endpoints(&duplicate).is_err());
    }

    #[test]
    fn route_record_round_trips_with_a_fixed_field_order() {
        let record = RouteRecord {
            session_id: 1,
            target: "cable".into(),
            originals: ["console".into(), "media".into(), "calls".into()],
        };
        assert_eq!(
            RouteRecord::parse(&record.serialize().unwrap()).unwrap(),
            record
        );
    }

    #[test]
    fn route_record_rejects_unknown_fields_and_unbounded_ids() {
        let extra = "version=1\nsession_id=1\ntarget=t\nconsole=a\nmultimedia=b\ncommunications=c\nextra=d\n";
        assert!(RouteRecord::parse(extra).is_err());
        let oversized = RouteRecord {
            session_id: 1,
            target: "x".repeat(MAX_ENDPOINT_ID_BYTES + 1),
            originals: ["a".into(), "b".into(), "c".into()],
        };
        assert!(oversized.serialize().is_err());
    }

    #[test]
    fn coupled_roles_are_not_written_twice() {
        let originals = ["speaker".into(), "speaker".into(), "speaker".into()];
        let current = ["cable".into(), "cable".into(), "speaker".into()];
        assert_eq!(
            target_writes(&current, &originals, "cable").unwrap(),
            [false, false, true]
        );
    }

    #[test]
    fn unrelated_changes_abort_routing_and_are_not_restored() {
        let originals = ["speaker".into(), "speaker".into(), "speaker".into()];
        let current = ["cable".into(), "headset".into(), "speaker".into()];
        assert!(target_writes(&current, &originals, "cable").is_err());
        assert_eq!(restoration_writes(&current, "cable"), [true, false, false]);
    }

    #[test]
    fn cable_is_recovered_when_it_was_already_the_original_default() {
        let cable = ["cable".into(), "cable".into(), "cable".into()];
        assert!(route_recovered(&cable, &cable, "cable"));

        let speakers = ["speaker".into(), "speaker".into(), "speaker".into()];
        assert!(!route_recovered(&cable, &speakers, "cable"));
    }
}
