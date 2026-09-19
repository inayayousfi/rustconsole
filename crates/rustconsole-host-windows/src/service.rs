#[cfg(windows)]
use std::ffi::OsString;
#[cfg(any(windows, test))]
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

pub const SERVICE_NAME: &str = "RustConsoleHost";
pub const SERVICE_DISPLAY_NAME: &str = "Rust Console Host";
pub const SERVICE_DESCRIPTION: &str =
    "Streams this Windows computer to authenticated Rust Console players.";
pub const SERVICE_ERROR_REPORT: &str = r"C:\ProgramData\RustConsole\service-error.txt";
pub const SESSION_ERROR_REPORT: &str = r"C:\ProgramData\RustConsole\session-error.txt";

#[cfg(any(windows, test))]
fn session_availability(
    stream_active: bool,
    desktop_session_available: bool,
) -> rustconsole_protocol::wire::SessionAvailability {
    if stream_active {
        rustconsole_protocol::wire::SessionAvailability::Busy
    } else if desktop_session_available {
        rustconsole_protocol::wire::SessionAvailability::Available
    } else {
        rustconsole_protocol::wire::SessionAvailability::DesktopSessionUnavailable
    }
}

#[cfg(any(windows, test))]
fn vb_cable_status<E>(available: Result<bool, E>) -> rustconsole_protocol::wire::VbCableStatus {
    match available {
        Ok(true) => rustconsole_protocol::wire::VbCableStatus::Ready,
        Ok(false) => rustconsole_protocol::wire::VbCableStatus::Unavailable,
        Err(_) => rustconsole_protocol::wire::VbCableStatus::CheckFailed,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceCommand {
    Displays,
    Run,
    Install,
    InstallElevated,
    SetPassword,
    SetPasswordStdin,
    MediaWorker {
        command_read: usize,
        event_write: usize,
        audio_write: usize,
    },
    MediaWorkerNamed {
        control_pipe: String,
        audio_pipe: String,
        connection_token: String,
    },
    SessionControls {
        pipe: String,
        connection_token: String,
    },
    Uninstall,
    UninstallElevated,
    FirewallStatus,
    FirewallEnable(crate::firewall::FirewallScope),
    FirewallDisable,
}

pub fn execute_service_command(command: ServiceCommand) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    {
        windows::execute(command)
    }

    #[cfg(not(windows))]
    {
        let _ = command;
        Err("the Rust Console host service is only available on Windows".into())
    }
}

#[cfg(windows)]
pub(crate) fn install_service(
    executable_path: std::path::PathBuf,
    launch_arguments: Vec<OsString>,
) -> Result<(), Box<dyn std::error::Error>> {
    windows::install_service(executable_path, launch_arguments)
}

#[cfg(any(windows, test))]
fn shutdown_channel() -> (SyncSender<()>, Receiver<()>) {
    sync_channel(1)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::worker_protocol::WorkerEvent;
    use quinn::{Connection, Endpoint, VarInt};
    use rustconsole_host_core::authentication::{
        AuthenticatedConnection, AuthenticationRateLimiter, HostMetadata, authenticate_server,
        ephemeral_server_config, read_envelope, write_envelope,
    };
    use rustconsole_host_core::{
        AdaptiveBitrateController, DesktopCapture, HostSessionControlAction,
        HostSessionControlSource, VideoDeliveryReport, VideoPathReport,
    };
    use rustconsole_input_windows::{HidReport, InputSession, ReportSink, VirtualInputOwner};
    use rustconsole_protocol::diagnostics::{MediaKind, PayloadDigest, STREAM_PREAMBLE};
    use rustconsole_protocol::input::STREAM_PREAMBLE as INPUT_STREAM_PREAMBLE;
    use rustconsole_protocol::wire::{
        self, Av1Capability, Av1CapabilityOffer, Av1Mode, EncodedVideoPacket, Envelope,
        HostSessionControl, HostSessionControlKind, KeyboardLeds as WireKeyboardLeds,
        SelectedAv1Configuration, SessionAvailabilityResult, VideoControlKind, envelope,
    };
    use rustconsole_protocol::{
        Av1Capability as DomainCapability, Av1Mode as DomainMode,
        Av1ViewerSettings as DomainSettings, ChromaSubsampling, VideoBitDepth,
        negotiate_av1_configuration,
    };
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{
        self, ServiceControlHandlerResult, ServiceStatusHandle,
    };
    use windows_service::service_dispatcher;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    const STATUS_WAIT_HINT: Duration = Duration::from_secs(5);
    const PRODUCTION_QUIC_IPV4_ADDRESS: &str = "0.0.0.0:47999";
    const PRODUCTION_QUIC_IPV6_ADDRESS: &str = "[::]:47999";
    const IPV6_STATUS_REPORT: &str = r"C:\ProgramData\RustConsole\ipv6-status.txt";

    #[derive(Clone, Copy)]
    enum ServiceMode {
        Idle,
    }

    static SERVICE_MODE: OnceLock<ServiceMode> = OnceLock::new();

    windows_service::define_windows_service!(service_main_ffi, service_main);

    pub fn execute(command: ServiceCommand) -> Result<(), Box<dyn std::error::Error>> {
        match command {
            ServiceCommand::Displays => {
                let inventory = crate::display::discover_active_console(&std::env::current_exe()?)?;
                for (index, display) in inventory.displays().iter().enumerate() {
                    let adapter = inventory
                        .adapters()
                        .iter()
                        .find(|adapter| adapter.id == display.adapter)
                        .ok_or("display adapter disappeared")?;
                    println!(
                        "display={} name={:?} id={:?} size={}x{} refresh={} primary={} adapter={:016x} gpu={:?}",
                        index + 1,
                        display.name,
                        display.id.as_str(),
                        display.width,
                        display.height,
                        display.refresh_rate,
                        display.primary,
                        adapter.id.0,
                        adapter.name
                    );
                }
                Ok(())
            }
            ServiceCommand::Run => {
                let _ = SERVICE_MODE.set(ServiceMode::Idle);
                service_dispatcher::start(SERVICE_NAME, service_main_ffi)?;
                Ok(())
            }
            ServiceCommand::Install => crate::installation::install(false),
            ServiceCommand::InstallElevated => crate::installation::install(true),
            ServiceCommand::SetPassword => crate::credentials::set_password_interactive(),
            ServiceCommand::SetPasswordStdin => crate::credentials::set_password_from_stdin(),
            ServiceCommand::MediaWorker {
                command_read,
                event_write,
                audio_write,
            } => crate::worker::run(command_read, event_write, audio_write),
            ServiceCommand::MediaWorkerNamed {
                control_pipe,
                audio_pipe,
                connection_token,
            } => crate::worker::run_named(&control_pipe, &audio_pipe, &connection_token),
            ServiceCommand::SessionControls {
                pipe,
                connection_token,
            } => crate::session_controls::run(&pipe, &connection_token),
            ServiceCommand::Uninstall => crate::installation::uninstall(false),
            ServiceCommand::UninstallElevated => crate::installation::uninstall(true),
            ServiceCommand::FirewallStatus => crate::firewall::print_status(),
            ServiceCommand::FirewallEnable(scope) => crate::firewall::enable(scope),
            ServiceCommand::FirewallDisable => crate::firewall::disable(),
        }
    }

    pub(super) fn install_service(
        executable_path: PathBuf,
        launch_arguments: Vec<OsString>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        let service = manager.create_service(
            &ServiceInfo {
                name: SERVICE_NAME.into(),
                display_name: SERVICE_DISPLAY_NAME.into(),
                service_type: ServiceType::OWN_PROCESS,
                start_type: ServiceStartType::AutoStart,
                error_control: ServiceErrorControl::Normal,
                executable_path,
                launch_arguments,
                dependencies: Vec::new(),
                account_name: None,
                account_password: None,
            },
            ServiceAccess::CHANGE_CONFIG,
        )?;
        service.set_description(SERVICE_DESCRIPTION)?;
        if let Err(error) = crate::firewall::print_status() {
            println!("Rust Console firewall status: check-failed ({error})");
            print!(
                "{}",
                crate::firewall::guidance(
                    rustconsole_protocol::wire::HostFirewallStatus::CheckFailed
                )
            );
        }
        Ok(())
    }

    fn service_main(_arguments: Vec<OsString>) {
        let mode = SERVICE_MODE.get().copied().unwrap_or(ServiceMode::Idle);
        if let Err(error) = run_service(mode) {
            let report_path = match mode {
                ServiceMode::Idle => Some(SERVICE_ERROR_REPORT),
            };
            if let Some(report_path) = report_path {
                let report_path = PathBuf::from(report_path);
                if let Some(parent) = report_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(report_path, format!("error: {error}\n"));
            }
            std::process::exit(1);
        }
    }

    fn run_service(mode: ServiceMode) -> Result<(), Box<dyn std::error::Error>> {
        let (shutdown_tx, shutdown_rx) = shutdown_channel();
        let status_handle =
            service_control_handler::register(SERVICE_NAME, move |control| match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = shutdown_tx.try_send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            })?;

        set_status(
            &status_handle,
            ServiceState::StartPending,
            ServiceControlAccept::empty(),
            1,
            STATUS_WAIT_HINT,
        )?;
        crate::audio_policy::recover_pending_route()
            .map_err(|error| format!("recovering the Windows audio route: {error}"))?;
        let _audio_recovery = AudioRecoveryGuard;
        set_status(
            &status_handle,
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            0,
            Duration::ZERO,
        )?;

        let shutdown_received = run_authenticated_quic_service(&shutdown_rx)?;
        if !shutdown_received && shutdown_rx.recv().is_err() {
            return Ok(());
        }

        set_status(
            &status_handle,
            ServiceState::StopPending,
            ServiceControlAccept::empty(),
            1,
            STATUS_WAIT_HINT,
        )?;
        set_status(
            &status_handle,
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            0,
            Duration::ZERO,
        )
        .map_err(Into::into)
    }

    fn run_authenticated_quic_service(
        shutdown_rx: &Receiver<()>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let record = Arc::new(
            crate::credentials::load_record()
                .map_err(|error| format!("loading the OPAQUE server record: {error}"))?,
        );
        let _ = crate::firewall::write_report();
        let host_metadata = Arc::new(
            discovery_metadata()
                .map_err(|error| format!("reading Windows host metadata: {error}"))?,
        );
        let limiter = Arc::new(Mutex::new(AuthenticationRateLimiter::default()));
        let active_stream = Arc::new(AtomicBool::new(false));
        let vb_cable_status = Arc::new(AtomicI32::new(detect_vb_cable_status() as i32));
        let executable = Arc::new(
            std::env::current_exe()
                .map_err(|error| format!("locating the host executable: {error}"))?,
        );
        let input_generation =
            u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros())?.max(1);
        let input_inf = executable
            .parent()
            .ok_or("host executable has no parent directory")?
            .join("input-driver")
            .join("rustconsole_input_driver.inf");
        let input_owner = Arc::new(Mutex::new(
            VirtualInputOwner::create(&input_inf, input_generation).map_err(|error| {
                format!("creating the persistent virtual input devices: {error}")
            })?,
        ));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("creating the host async runtime: {error}"))?;
        runtime.block_on(async move {
            let server_config = ephemeral_server_config()
                .map_err(|error| format!("creating the ephemeral QUIC certificate: {error}"))?;
            let ipv4 = Endpoint::server(
                server_config.clone(),
                PRODUCTION_QUIC_IPV4_ADDRESS.parse()?,
            )
            .map_err(|error| format!("opening the IPv4 QUIC endpoint: {error}"))?;
            let ipv6 = match Endpoint::server(
                server_config,
                PRODUCTION_QUIC_IPV6_ADDRESS.parse()?,
            ) {
                Ok(endpoint) => {
                    let _ = fs::remove_file(IPV6_STATUS_REPORT);
                    Some(endpoint)
                }
                Err(error) => {
                    fs::write(
                        IPV6_STATUS_REPORT,
                        format!("IPv6 QUIC listener unavailable: {error}\n"),
                    )?;
                    None
                }
            };
            let mut authentications = tokio::task::JoinSet::new();
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    ipv4.close(VarInt::from_u32(0), b"service stopping");
                    if let Some(ipv6) = &ipv6 {
                        ipv6.close(VarInt::from_u32(0), b"service stopping");
                    }
                    if tokio::time::timeout(Duration::from_secs(10), async {
                        while authentications.join_next().await.is_some() {}
                    })
                    .await
                    .is_err()
                    {
                        authentications.abort_all();
                        while authentications.join_next().await.is_some() {}
                    }
                    return Ok(true);
                }
                tokio::select! {
                    incoming = ipv4.accept() => {
                        let Some(incoming) = incoming else {
                            return Ok(true);
                        };
                        let record = Arc::clone(&record);
                        let host_metadata = Arc::clone(&host_metadata);
                        let limiter = Arc::clone(&limiter);
                        let active_stream = Arc::clone(&active_stream);
                        let vb_cable_status = Arc::clone(&vb_cable_status);
                        let executable = Arc::clone(&executable);
                        let input_owner = Arc::clone(&input_owner);
                        authentications.spawn(async move {
                            let connection = match incoming.await {
                                Ok(connection) => connection,
                                Err(_) => return,
                            };
                            match authenticate_server(connection.clone(), &record, &limiter, &host_metadata).await {
                                Ok(authenticated) => {
                                    if let Err(error) = serve_authenticated_video(
                                         authenticated,
                                         active_stream,
                                         vb_cable_status,
                                         executable,
                                         input_owner,
                                    ).await {
                                        report_session_error(&connection, error.as_ref());
                                    }
                                }
                                Err(_) => connection.close(
                                    VarInt::from_u32(0x100),
                                    b"authentication failed",
                                ),
                            }
                        });
                    }
                    incoming = optional_accept(ipv6.as_ref()) => {
                        let Some(incoming) = incoming else {
                            continue;
                        };
                        let record = Arc::clone(&record);
                        let host_metadata = Arc::clone(&host_metadata);
                        let limiter = Arc::clone(&limiter);
                        let active_stream = Arc::clone(&active_stream);
                        let vb_cable_status = Arc::clone(&vb_cable_status);
                        let executable = Arc::clone(&executable);
                        let input_owner = Arc::clone(&input_owner);
                        authentications.spawn(async move {
                            let connection = match incoming.await {
                                Ok(connection) => connection,
                                Err(_) => return,
                            };
                            match authenticate_server(connection.clone(), &record, &limiter, &host_metadata).await {
                                Ok(authenticated) => {
                                    if let Err(error) = serve_authenticated_video(
                                         authenticated,
                                         active_stream,
                                         vb_cable_status,
                                         executable,
                                         input_owner,
                                    ).await {
                                        report_session_error(&connection, error.as_ref());
                                    }
                                }
                                Err(_) => connection.close(
                                    VarInt::from_u32(0x100),
                                    b"authentication failed",
                                ),
                            }
                        });
                    }
                    completed = authentications.join_next(), if !authentications.is_empty() => {
                        let _ = completed;
                    }
                    () = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        })
    }

    async fn optional_accept(endpoint: Option<&Endpoint>) -> Option<quinn::Incoming> {
        match endpoint {
            Some(endpoint) => endpoint.accept().await,
            None => std::future::pending().await,
        }
    }

    fn report_session_error(connection: &Connection, error: &dyn std::error::Error) {
        let report = PathBuf::from(SESSION_ERROR_REPORT);
        if let Some(parent) = report.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut report) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(report)
        {
            let timestamp_millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis());
            let _ = writeln!(report, "time_millis={timestamp_millis} error={error}");
        }
        connection.close(VarInt::from_u32(0x102), b"session failed");
    }

    fn discovery_metadata() -> Result<HostMetadata, Box<dyn std::error::Error>> {
        use ::windows::Win32::System::SystemInformation::{
            ComputerNameDnsHostname, GetComputerNameExW,
        };
        use ::windows::core::PWSTR;

        let mut buffer = [0u16; 64];
        let mut length = buffer.len() as u32;
        // SAFETY: the writable buffer contains `length` UTF-16 code units.
        unsafe {
            GetComputerNameExW(
                ComputerNameDnsHostname,
                Some(PWSTR(buffer.as_mut_ptr())),
                &mut length,
            )?;
        }
        let length = usize::try_from(length)?;
        let display_name = String::from_utf16(&buffer[..length])?;
        Ok(HostMetadata::new(
            display_name,
            wire::HostOperatingSystem::Windows,
        )?)
    }

    async fn serve_authenticated_video(
        authenticated: AuthenticatedConnection,
        active_stream: Arc<AtomicBool>,
        vb_cable_status: Arc<AtomicI32>,
        executable: Arc<PathBuf>,
        input_owner: Arc<Mutex<VirtualInputOwner>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let connection = authenticated.connection;
        macro_rules! session_try {
            ($stage:literal, $result:expr) => {
                $result.map_err(|error| format!(concat!($stage, ": {}"), error))?
            };
        }
        let (mut send, mut receive) =
            tokio::time::timeout(Duration::from_secs(10), connection.accept_bi())
                .await
                .map_err(|_| "timed out waiting for AV1 capability negotiation")?
                .map_err(|error| format!("accepting AV1 negotiation stream: {error}"))?;
        let request = session_try!(
            "reading player AV1 capability offer",
            read_envelope(&mut receive).await
        );
        if matches!(
            &request.body,
            Some(envelope::Body::DisplayCatalogRequest(_))
        ) {
            let discovery_executable = Arc::clone(&executable);
            let catalog = tokio::task::spawn_blocking(move || {
                crate::display::discover_active_console(&discovery_executable)
                    .map(|inventory| wire::DisplayCatalog::from(&inventory))
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|_| "display discovery task panicked")??;
            write_envelope(
                &mut send,
                Envelope {
                    body: Some(envelope::Body::DisplayCatalog(catalog)),
                },
            )
            .await?;
            send.finish()?;
            if send.stopped().await?.is_some() {
                return Err("display discovery response was cancelled".into());
            }
            return Ok(());
        }
        if matches!(
            &request.body,
            Some(envelope::Body::SessionAvailabilityProbe(_))
        ) {
            let vb_cable_status = refresh_vb_cable_status(&vb_cable_status);
            let availability = session_availability(
                active_stream.load(Ordering::Acquire),
                crate::worker::active_console_session_available(),
            );
            write_envelope(
                &mut send,
                Envelope {
                    body: Some(envelope::Body::SessionAvailabilityResult(
                        SessionAvailabilityResult {
                            availability: availability as i32,
                            vb_cable_status: vb_cable_status as i32,
                            firewall_status: crate::firewall::status() as i32,
                        },
                    )),
                },
            )
            .await?;
            send.finish()?;
            if let Some(code) = send.stopped().await? {
                return Err(
                    format!("viewer stopped the availability response with code {code}").into(),
                );
            }
            return Ok(());
        }
        let (
            audio_configuration,
            dedicated_input_stream,
            full_diagnostics,
            host_pointer_release,
            video_datagram_version,
        ) = match &request.body {
            Some(envelope::Body::Av1CapabilityOffer(offer)) => (
                offer
                    .audio_transport
                    .filter(|configuration| configuration.supported()),
                offer.dedicated_input_stream,
                offer.full_diagnostics,
                offer.host_pointer_release,
                if offer.video_datagram_version
                    >= rustconsole_host_core::video_transport::VIDEO_DATAGRAM_VERSION
                {
                    rustconsole_host_core::video_transport::VIDEO_DATAGRAM_VERSION
                } else {
                    rustconsole_host_core::video_transport::LEGACY_VIDEO_DATAGRAM_VERSION
                },
            ),
            _ => (
                None,
                false,
                false,
                false,
                rustconsole_host_core::video_transport::LEGACY_VIDEO_DATAGRAM_VERSION,
            ),
        };
        let display_id = match &request.body {
            Some(envelope::Body::Av1CapabilityOffer(offer)) => offer
                .display_id
                .clone()
                .map(rustconsole_protocol::display::DisplayId::new)
                .transpose()?,
            _ => None,
        };
        let (decoder_capabilities, settings) = session_try!(
            "parsing player AV1 capability offer",
            parse_viewer_offer(request)
        );
        if active_stream
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            connection.close(VarInt::from_u32(0x101), b"another viewer is streaming");
            return Ok(());
        }
        let _active_guard = ActiveStreamGuard(Arc::clone(&active_stream));
        let mut session_controls = if host_pointer_release {
            let controls_executable = executable.as_ref().to_owned();
            tokio::task::spawn_blocking(move || {
                match crate::session_controls::WindowsSessionControls::launch(&controls_executable)
                {
                    Ok(controls) => Ok(Some(controls)),
                    Err(error)
                        if crate::session_controls::user_token_unavailable(error.as_ref()) =>
                    {
                        Ok(None)
                    }
                    Err(error) => Err(error.to_string()),
                }
            })
            .await
            .map_err(|_| "session controls launch task panicked")??
        } else {
            None
        };
        let host_pointer_release = session_controls.is_some();
        let worker_executable = executable.as_ref().to_owned();
        let worker_display_id = display_id.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let (_shutdown_tx, shutdown_rx) = shutdown_channel();
            crate::worker::MediaWorker::launch_prepared_video(
                &worker_executable,
                &shutdown_rx,
                worker_display_id,
            )
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "media worker preparation was interrupted".to_owned())
        })
        .await
        .map_err(|_| "media worker preparation task panicked")?;
        let (worker, video_configuration) = session_try!("preparing media worker", prepared);
        if settings.width != video_configuration.width
            || settings.height != video_configuration.height
        {
            return Err(
                "requested dimensions do not match the selected display; scaling is not configured"
                    .into(),
            );
        }
        let encoder_capability = DomainCapability {
            mode: DomainMode {
                chroma_subsampling: ChromaSubsampling::Yuv420,
                bit_depth: match video_configuration.format {
                    crate::worker_protocol::WorkerVideoFormat::Nv12 => VideoBitDepth::Eight,
                    crate::worker_protocol::WorkerVideoFormat::P010 => VideoBitDepth::Ten,
                },
            },
            maximum_width: video_configuration.width,
            maximum_height: video_configuration.height,
            maximum_frames_per_second: session_try!(
                "converting host refresh rate",
                u16::try_from(video_configuration.refresh_rate)
            ),
        };
        let selected = session_try!(
            "selecting AV1 configuration",
            negotiate_av1_configuration(&[encoder_capability], &decoder_capabilities, &settings)
        );
        session_try!(
            "sending host AV1 capability offer",
            write_envelope(
                &mut send,
                Envelope {
                    body: Some(envelope::Body::Av1CapabilityOffer(Av1CapabilityOffer {
                        display_id: display_id.map(|id| id.as_str().to_owned()),
                        dedicated_input_stream,
                        video_datagram_version,
                        host_pointer_release,
                        full_diagnostics,
                        audio_transport: audio_configuration,
                        encoder_capabilities: vec![wire_capability(encoder_capability)],
                        decoder_capabilities: Vec::new(),
                        viewer_settings: None,
                    })),
                },
            )
            .await
        );
        let mut selected_wire = wire_selected(selected);
        selected_wire.audio_transport = audio_configuration;
        selected_wire.full_diagnostics = full_diagnostics;
        selected_wire.host_pointer_release = host_pointer_release;
        selected_wire.dedicated_input_stream = dedicated_input_stream;
        selected_wire.video_datagram_version = video_datagram_version;
        let player_selection = session_try!(
            "reading player AV1 selection",
            read_envelope(&mut receive).await
        );
        match player_selection.body {
            Some(envelope::Body::SelectedAv1Configuration(mut peer)) => {
                if peer.video_datagram_version == 0 {
                    peer.video_datagram_version =
                        rustconsole_host_core::video_transport::LEGACY_VIDEO_DATAGRAM_VERSION;
                }
                if peer != selected_wire {
                    return Err("viewer selected a different AV1 configuration".into());
                }
            }
            _ => return Err("viewer selected a different AV1 configuration".into()),
        }
        session_try!(
            "sending host AV1 selection",
            write_envelope(
                &mut send,
                Envelope {
                    body: Some(envelope::Body::SelectedAv1Configuration(selected_wire)),
                },
            )
            .await
        );

        let mut input_receive = if dedicated_input_stream {
            let mut input = tokio::time::timeout(Duration::from_secs(10), connection.accept_uni())
                .await
                .map_err(|_| "timed out waiting for dedicated input stream")?
                .map_err(|error| format!("accepting dedicated input stream: {error}"))?;
            let mut preamble = [0; INPUT_STREAM_PREAMBLE.len()];
            session_try!(
                "reading dedicated input stream preamble",
                input.read_exact(&mut preamble).await
            );
            if preamble != INPUT_STREAM_PREAMBLE {
                return Err("invalid dedicated input stream preamble".into());
            }
            Some(input)
        } else {
            None
        };

        let diagnostic_tx = if full_diagnostics {
            let (diagnostic_tx, mut diagnostic_rx) =
                tokio::sync::mpsc::channel::<PayloadDigest>(1024);
            let diagnostic_connection = connection.clone();
            tokio::spawn(async move {
                let result = async {
                    let mut stream = diagnostic_connection.open_uni().await?;
                    stream.write_all(&STREAM_PREAMBLE).await?;
                    while let Some(record) = diagnostic_rx.recv().await {
                        stream.write_all(&record.encode()).await?;
                    }
                    stream.finish()?;
                    Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                }
                .await;
                if let Err(error) = result {
                    eprintln!("diagnostic stream: {error}");
                    diagnostic_connection
                        .close(VarInt::from_u32(0x104), b"diagnostic stream failed");
                }
            });
            Some(DiagnosticSender {
                tx: diagnostic_tx,
                dropped: Arc::new(AtomicU64::new(0)),
            })
        } else {
            None
        };

        let (control_tx, control_rx) = std::sync::mpsc::sync_channel(16);
        let latest_input = Arc::new(Mutex::new((0_u64, 0_u64)));
        let (audio_state_tx, mut audio_state_rx) =
            tokio::sync::watch::channel(wire::AudioStreamState::new(
                0,
                if audio_configuration.is_some() {
                    wire::AudioStatus::Waiting
                } else {
                    wire::AudioStatus::NotNegotiated
                },
                0,
                String::new(),
            ));
        let mut audio_state_open = audio_configuration.is_some();
        let media_connection = connection.clone();
        let runtime = tokio::runtime::Handle::current();
        let media_latest_input = Arc::clone(&latest_input);
        let mut media = tokio::task::spawn_blocking(move || {
            run_worker_video_stream(
                media_connection,
                runtime,
                worker,
                selected,
                video_datagram_version,
                control_rx,
                audio_configuration.is_some(),
                audio_state_tx,
                media_latest_input,
                diagnostic_tx,
            )
        });
        let mut control_error = None::<String>;
        let mut media_finished = false;
        let mut input_session = InputSession::default();
        let input_clock = full_diagnostics
            .then(crate::clock::HostClock::new)
            .transpose()?;
        let pointer_datagrams_received = 0_u64;
        let pointer_updates_applied = 0_u64;
        let pointer_updates_ignored = 0_u64;
        let mut mouse_reports_published = 0_u64;
        let mut keyboard_reports_published = 0_u64;
        let mut reliable_transitions_received = 0_u64;
        let mut reliable_transitions_applied = 0_u64;
        let reliable_transitions_rejected = 0_u64;
        let mut reliable_transitions_missing = 0_u64;
        let mut reliable_transitions_duplicate_or_late = 0_u64;
        let mut release_all_transitions = 0_u64;
        let pointer_missing_datagrams = 0_u64;
        let pointer_stale_generations = 0_u64;
        let pointer_duplicate_or_late = 0_u64;
        let pointer_mode_rejections = 0_u64;
        let pointer_relative_baselines = 0_u64;
        let (response_tx, mut responses) = tokio::sync::mpsc::channel::<Envelope>(64);
        let mut response_writer = tokio::task::JoinSet::new();
        response_writer.spawn(async move {
            while let Some(response) = responses.recv().await {
                tokio::time::timeout(Duration::from_secs(3), write_envelope(&mut send, response))
                    .await
                    .map_err(|_| "session response write timed out".to_owned())?
                    .map_err(|error| error.to_string())?;
            }
            send.finish().map_err(|error| error.to_string())
        });
        let (incoming_tx, mut incoming) = tokio::sync::mpsc::channel(8);
        let mut readers = tokio::task::JoinSet::new();
        for (stream, from_input_stream) in std::iter::once((receive, false))
            .chain(input_receive.take().map(|stream| (stream, true)))
        {
            readers.spawn(read_session_messages(
                stream,
                from_input_stream,
                incoming_tx.clone(),
            ));
        }
        drop(incoming_tx);
        let mut input_tick = tokio::time::interval(Duration::from_millis(5));
        input_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        'control: loop {
            let (envelope, from_input_stream) = loop {
                tokio::select! {
                    result = response_writer.join_next() => {
                        control_error = Some(match result {
                            Some(Ok(Err(error))) => error,
                            _ => "session response writer stopped unexpectedly".to_owned(),
                        });
                        break 'control;
                    }
                    state = audio_state_rx.changed(), if audio_state_open => {
                        if state.is_err() { audio_state_open = false; continue; }
                        let state = audio_state_rx.borrow_and_update().clone();
                        if let Err(error) = response_tx.try_send(Envelope { body: Some(envelope::Body::AudioStreamState(state)) }) {
                            control_error = Some(error.to_string()); break 'control;
                        }
                    }
                    result = &mut media => {
                        media_finished = true;
                        match result {
                            Ok(Ok(WorkerVideoExit::ReconfigurationRequired(cause))) => {
                                if let Err(error) = response_tx.try_send(Envelope {
                                    body: Some(envelope::Body::VideoStreamState(
                                        wire::VideoStreamState::reconfiguration_required(cause),
                                    )),
                                }) {
                                    control_error = Some(error.to_string());
                                }
                            }
                            Ok(Ok(WorkerVideoExit::Stopped)) => {
                                control_error = Some("media worker stopped".to_owned());
                            }
                            Ok(Err(error)) => control_error = Some(format!("media worker failed: {error}")),
                            Err(error) => control_error = Some(format!("media worker task failed: {error}")),
                        }
                        break 'control;
                    }
                    error = connection.closed() => {
                        if !matches!(error, quinn::ConnectionError::LocallyClosed) {
                            control_error = Some(format!("viewer connection closed: {error}"));
                        }
                        break 'control;
                    }
                    _ = input_tick.tick() => {
                        if let Some(controls) = session_controls.as_mut() {
                            match controls.try_next_action() {
                                Ok(Some(HostSessionControlAction::ReleasePointerCapture)) => {
                                    if let Err(error) = response_tx.try_send(Envelope {
                                        body: Some(envelope::Body::HostSessionControl(HostSessionControl {
                                            kind: HostSessionControlKind::ReleasePointerCapture as i32,
                                        })),
                                    }) {
                                        control_error = Some(error.to_string());
                                        break 'control;
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    control_error = Some(error);
                                    break 'control;
                                }
                            }
                        }
                        let leds = match input_owner.lock() {
                            Ok(mut owner) => {
                                let generation = owner.generation();
                                match owner.drain_keyboard_leds() {
                                    Ok(states) => states.into_iter().map(|(sequence, state)| WireKeyboardLeds {
                                        generation,
                                        sequence,
                                        mask: u32::from(state.mask()),
                                    }).collect::<Vec<_>>(),
                                    Err(error) => { control_error = Some(format!("keyboard LED input failed: {error:?}")); break 'control; }
                                }
                            }
                            Err(_) => { control_error = Some("virtual input owner lock poisoned".to_owned()); break 'control; }
                        };
                        for state in leds {
                            if let Err(error) = response_tx.try_send(Envelope { body: Some(envelope::Body::KeyboardLeds(state)) }) {
                                control_error = Some(error.to_string()); break 'control;
                            }
                        }
                    }
                    message = incoming.recv() => match message {
                        Some((Ok(envelope), from_input_stream)) => break (envelope, from_input_stream),
                        Some((Err(error), _)) => { control_error = Some(error.to_string()); break 'control; }
                        None => { control_error = Some("session readers stopped".to_owned()); break 'control; }
                    },
                }
            };
            if from_input_stream != matches!(&envelope.body, Some(envelope::Body::InputPack(_)))
                && dedicated_input_stream
            {
                control_error = Some("message arrived on the wrong session stream".to_owned());
                break 'control;
            }
            let command = match envelope.body {
                Some(envelope::Body::InputPack(pack)) => {
                    if pack.transitions.is_empty()
                        || pack.transitions.len() > rustconsole_protocol::input::MAX_EVENTS_PER_PACK
                    {
                        control_error = Some("input pack has an invalid event count".to_owned());
                        break;
                    }
                    let host_received_at_micros = input_clock
                        .as_ref()
                        .map(crate::clock::HostClock::now)
                        .transpose()?
                        .unwrap_or(0);
                    let mut last_ack = None;
                    for transition in pack.transitions {
                        if full_diagnostics {
                            reliable_transitions_received =
                                reliable_transitions_received.saturating_add(1);
                            let expected = input_session.reliable_sequence().saturating_add(1);
                            if transition.generation == input_session.generation() {
                                if transition.sequence > expected {
                                    reliable_transitions_missing = reliable_transitions_missing
                                        .saturating_add(transition.sequence - expected);
                                } else if transition.sequence < expected {
                                    reliable_transitions_duplicate_or_late =
                                        reliable_transitions_duplicate_or_late.saturating_add(1);
                                }
                            }
                            if matches!(
                                transition.action,
                                Some(wire::input_transition::Action::ReleaseAll(_))
                            ) {
                                release_all_transitions = release_all_transitions.saturating_add(1);
                            }
                        }
                        let ack = match input_owner.lock() {
                            Ok(mut owner) if full_diagnostics => {
                                let mut sink = DiagnosticReportSink::new(owner.sink_mut());
                                let result = input_session.reliable(transition, &mut sink);
                                mouse_reports_published =
                                    mouse_reports_published.saturating_add(sink.mouse_reports);
                                keyboard_reports_published = keyboard_reports_published
                                    .saturating_add(sink.keyboard_reports);
                                match result {
                                    Ok(ack) => ack,
                                    Err(error) => {
                                        control_error =
                                            Some(format!("input transition failed: {error:?}"));
                                        break 'control;
                                    }
                                }
                            }
                            Ok(mut owner) => {
                                match input_session.reliable(transition, owner.sink_mut()) {
                                    Ok(ack) => ack,
                                    Err(error) => {
                                        control_error =
                                            Some(format!("input transition failed: {error:?}"));
                                        break 'control;
                                    }
                                }
                            }
                            Err(_) => {
                                control_error =
                                    Some("virtual input owner lock poisoned".to_owned());
                                break 'control;
                            }
                        };
                        if full_diagnostics {
                            reliable_transitions_applied =
                                reliable_transitions_applied.saturating_add(1);
                        }
                        last_ack = Some(ack);
                    }
                    let mut ack =
                        last_ack.expect("nonempty input pack produced an acknowledgement");
                    ack.host_received_at_micros = host_received_at_micros;
                    ack.host_submitted_at_micros = input_clock
                        .as_ref()
                        .map(crate::clock::HostClock::now)
                        .transpose()?
                        .unwrap_or(0);
                    if full_diagnostics {
                        ack.pointer_datagrams_received = pointer_datagrams_received;
                        ack.pointer_updates_applied = pointer_updates_applied;
                        ack.pointer_updates_ignored = pointer_updates_ignored;
                        ack.mouse_reports_published = mouse_reports_published;
                        ack.keyboard_reports_published = keyboard_reports_published;
                        ack.reliable_transitions_received = reliable_transitions_received;
                        ack.reliable_transitions_applied = reliable_transitions_applied;
                        ack.reliable_transitions_rejected = reliable_transitions_rejected;
                        ack.reliable_transitions_missing = reliable_transitions_missing;
                        ack.reliable_transitions_duplicate_or_late =
                            reliable_transitions_duplicate_or_late;
                        ack.release_all_transitions = release_all_transitions;
                        ack.pointer_missing_datagrams = pointer_missing_datagrams;
                        ack.pointer_stale_generations = pointer_stale_generations;
                        ack.pointer_duplicate_or_late = pointer_duplicate_or_late;
                        ack.pointer_mode_rejections = pointer_mode_rejections;
                        ack.pointer_relative_baselines = pointer_relative_baselines;
                    }
                    *latest_input
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        (ack.through_sequence, ack.host_submitted_at_micros);
                    if let Err(error) = response_tx.try_send(Envelope {
                        body: Some(envelope::Body::InputAck(ack)),
                    }) {
                        control_error = Some(error.to_string());
                        break;
                    }
                    continue;
                }
                Some(envelope::Body::ClockPing(ping)) => {
                    let host_received_at_micros = input_clock
                        .as_ref()
                        .map(crate::clock::HostClock::now)
                        .transpose()?
                        .unwrap_or(0);
                    let pong = wire::ClockPong {
                        sequence: ping.sequence,
                        player_sent_at_micros: ping.player_sent_at_micros,
                        host_received_at_micros,
                        host_sent_at_micros: input_clock
                            .as_ref()
                            .map(crate::clock::HostClock::now)
                            .transpose()?
                            .unwrap_or(0),
                    };
                    if let Err(error) = response_tx.try_send(Envelope {
                        body: Some(envelope::Body::ClockPong(pong)),
                    }) {
                        control_error = Some(error.to_string());
                        break;
                    }
                    continue;
                }
                Some(envelope::Body::VideoControl(control))
                    if VideoControlKind::try_from(control.kind) == Ok(VideoControlKind::Stop) =>
                {
                    break;
                }
                Some(envelope::Body::VideoControl(control))
                    if VideoControlKind::try_from(control.kind)
                        == Ok(VideoControlKind::RequestKeyframe) =>
                {
                    WorkerVideoControl::RequestKeyframe
                }
                Some(envelope::Body::VideoReceiverReport(report)) => {
                    WorkerVideoControl::ReceiverReport(report)
                }
                _ => {
                    control_error = Some("invalid video control message".into());
                    break;
                }
            };
            if control_tx.try_send(command).is_err() {
                control_error = Some("media control queue is full or closed".into());
                break;
            }
        }
        if let Ok(mut owner) = input_owner.lock() {
            if let Err(error) = input_session.close(owner.sink_mut()) {
                control_error.get_or_insert_with(|| format!("input release failed: {error:?}"));
            }
        } else {
            control_error
                .get_or_insert_with(|| "virtual input owner lock poisoned during release".into());
        }
        let _ = control_tx.try_send(WorkerVideoControl::Stop);
        drop(control_tx);
        drop(readers);
        drop(response_tx);
        while let Some(result) = response_writer.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    control_error.get_or_insert(error);
                }
                Err(error) => {
                    control_error.get_or_insert_with(|| error.to_string());
                }
            }
        }
        if !media_finished {
            match media.await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    control_error.get_or_insert_with(|| format!("media worker failed: {error}"));
                }
                Err(_) => {
                    control_error.get_or_insert_with(|| "media worker task panicked".to_owned());
                }
            }
        }
        if let Some(error) = control_error {
            connection.close(VarInt::from_u32(0x102), error.as_bytes());
            return Err(error.into());
        }
        if matches!(
            connection.close_reason(),
            Some(quinn::ConnectionError::LocallyClosed)
        ) {
            return Ok(());
        }
        Ok(())
    }

    async fn read_session_messages(
        mut stream: quinn::RecvStream,
        from_input_stream: bool,
        incoming: tokio::sync::mpsc::Sender<(Result<Envelope, String>, bool)>,
    ) {
        loop {
            // Each stream keeps its partial frame until it completes or the session ends.
            let message = read_envelope(&mut stream)
                .await
                .map_err(|error| error.to_string());
            let failed = message.is_err();
            if incoming.send((message, from_input_stream)).await.is_err() || failed {
                break;
            }
        }
    }

    #[cfg(test)]
    mod stream_tests {
        use super::*;

        #[tokio::test]
        async fn control_messages_do_not_cancel_partial_input_frames() {
            let server = Endpoint::server(
                ephemeral_server_config().unwrap(),
                "127.0.0.1:0".parse().unwrap(),
            )
            .unwrap();
            let mut client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            client.set_default_client_config(
                rustconsole_session::quic::opaque_client_config().unwrap(),
            );
            let connecting = client
                .connect(server.local_addr().unwrap(), "rustconsole.invalid")
                .unwrap();
            let (client_connection, server_connection) =
                tokio::join!(async { connecting.await.unwrap() }, async {
                    server.accept().await.unwrap().await.unwrap()
                },);
            let (incoming_tx, mut incoming) = tokio::sync::mpsc::channel(8);
            let mut readers = tokio::task::JoinSet::new();
            let mut input = client_connection.open_uni().await.unwrap();
            let down = Envelope {
                body: Some(envelope::Body::InputPack(wire::InputPack {
                    transitions: vec![wire::InputTransition {
                        generation: 1,
                        sequence: 1,
                        player_sent_at_micros: 0,
                        action: Some(wire::input_transition::Action::Key(wire::KeyTransition {
                            hid_usage: 4,
                            pressed: true,
                        })),
                    }],
                })),
            };
            let bytes = wire::encode_reliable_frame(&down).unwrap();
            input.write_all(&bytes[..2]).await.unwrap();
            readers.spawn(read_session_messages(
                server_connection.accept_uni().await.unwrap(),
                true,
                incoming_tx.clone(),
            ));
            let mut control = client_connection.open_uni().await.unwrap();
            let ping = Envelope {
                body: Some(envelope::Body::ClockPing(wire::ClockPing {
                    sequence: 1,
                    player_sent_at_micros: 0,
                })),
            };
            write_envelope(&mut control, ping.clone()).await.unwrap();
            readers.spawn(read_session_messages(
                server_connection.accept_uni().await.unwrap(),
                false,
                incoming_tx,
            ));
            let (message, is_input) = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(!is_input);
            assert_eq!(message.unwrap(), ping);
            input.write_all(&bytes[2..]).await.unwrap();
            let mut up = down.clone();
            if let Some(envelope::Body::InputPack(pack)) = up.body.as_mut() {
                pack.transitions[0].sequence = 2;
                pack.transitions[0].action =
                    Some(wire::input_transition::Action::Key(wire::KeyTransition {
                        hid_usage: 4,
                        pressed: false,
                    }));
            }
            write_envelope(&mut input, up.clone()).await.unwrap();
            for expected in [down, up] {
                let (message, is_input) =
                    tokio::time::timeout(Duration::from_secs(2), incoming.recv())
                        .await
                        .unwrap()
                        .unwrap();
                assert!(is_input);
                assert_eq!(message.unwrap(), expected);
            }
            readers.abort_all();
        }
    }

    fn detect_vb_cable_status() -> wire::VbCableStatus {
        vb_cable_status(crate::audio::vb_cable_available())
    }

    fn refresh_vb_cable_status(status: &AtomicI32) -> wire::VbCableStatus {
        let current = detect_vb_cable_status();
        status.store(current as i32, Ordering::Release);
        current
    }

    struct ActiveStreamGuard(Arc<AtomicBool>);

    impl Drop for ActiveStreamGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }

    struct AudioRecoveryGuard;

    impl Drop for AudioRecoveryGuard {
        fn drop(&mut self) {
            if let Err(error) = crate::audio_policy::recover_pending_route() {
                eprintln!("audio route recovery: {error}");
            }
        }
    }

    enum WorkerVideoControl {
        RequestKeyframe,
        ReceiverReport(wire::VideoReceiverReport),
        Stop,
    }

    enum WorkerVideoExit {
        Stopped,
        ReconfigurationRequired(wire::VideoReconfigurationCause),
    }

    struct DiagnosticReportSink<'a, S> {
        inner: &'a mut S,
        mouse_reports: u64,
        keyboard_reports: u64,
    }

    impl<'a, S> DiagnosticReportSink<'a, S> {
        fn new(inner: &'a mut S) -> Self {
            Self {
                inner,
                mouse_reports: 0,
                keyboard_reports: 0,
            }
        }
    }

    impl<S: ReportSink> ReportSink for DiagnosticReportSink<'_, S> {
        type Error = S::Error;

        fn submit(&mut self, report: HidReport) -> Result<(), Self::Error> {
            let mouse = matches!(report, HidReport::Mouse(_));
            self.inner.submit(report)?;
            if mouse {
                self.mouse_reports = self.mouse_reports.saturating_add(1);
            } else {
                self.keyboard_reports = self.keyboard_reports.saturating_add(1);
            }
            Ok(())
        }
    }

    #[derive(Clone)]
    struct DiagnosticSender {
        tx: tokio::sync::mpsc::Sender<PayloadDigest>,
        dropped: Arc<AtomicU64>,
    }

    fn prepare_payload_digest(
        diagnostics: Option<&DiagnosticSender>,
        clock: &crate::clock::HostClock,
        kind: MediaKind,
        generation: u64,
        sequence: u64,
        payload: &[u8],
        producer_sha256: Option<[u8; 32]>,
        producer_hash_duration_micros: u64,
        encode_started_at_micros: u64,
        encoded_at_micros: u64,
        worker_queued_at_micros: u64,
        service_received_at_micros: u64,
        packetized_at_micros: u64,
        captured_at_micros: u64,
        mirror_decode_micros: u64,
        capture_acquisition_micros: u64,
        cross_adapter_copy_micros: u64,
        color_conversion_micros: u64,
        encoder_call_micros: u64,
        audio_capture_buffer_frames: u64,
        audio_capture_discontinuities: u64,
        audio_invalid_capture_timestamps: u64,
        audio_device_reopens: u64,
        audio_encoder_resets: u64,
        audio_capture_queue_depth: u64,
        audio_capture_queue_capacity: u64,
        audio_capture_queue_drops: u64,
        quality: Option<crate::worker_protocol::WorkerVideoQuality>,
    ) -> Result<Option<PayloadDigest>, Box<dyn std::error::Error + Send + Sync>> {
        let Some(diagnostics) = diagnostics else {
            return Ok(None);
        };
        let started = Instant::now();
        let boundary_sha256 = Sha256::digest(payload).into();
        let boundary_matched = producer_sha256 == Some(boundary_sha256);
        let record = PayloadDigest {
            kind,
            generation,
            sequence,
            payload_size: payload.len() as u64,
            hashed_at_micros: clock.now()?,
            producer_hash_duration_micros,
            boundary_hash_duration_micros: started.elapsed().as_micros() as u64,
            producer_dropped_records: diagnostics.dropped.load(Ordering::Relaxed),
            boundary_matched,
            sha256: producer_sha256.unwrap_or(boundary_sha256),
            encode_started_at_micros,
            encoded_at_micros,
            worker_queued_at_micros,
            service_received_at_micros,
            packetized_at_micros,
            captured_at_micros,
            mirror_decode_micros,
            quality_present: quality.is_some(),
            quality_presentation_timestamp: quality
                .map_or(0, |quality| quality.presentation_timestamp),
            source_readback_micros: quality.map_or(0, |quality| quality.source_readback_micros),
            decoded_readback_micros: quality.map_or(0, |quality| quality.decoded_readback_micros),
            scoring_micros: quality.map_or(0, |quality| quality.scoring_micros),
            readback_bytes: quality.map_or(0, |quality| quality.readback_bytes),
            luma_psnr_millidecibels: quality.map_or(0, |quality| quality.luma_psnr_millidecibels),
            luma_mean_absolute_error_ppm: quality
                .map_or(0, |quality| quality.luma_mean_absolute_error_ppm),
            packetization_completed_at_micros: 0,
            first_send_attempt_at_micros: 0,
            last_send_completed_at_micros: 0,
            capture_acquisition_micros,
            cross_adapter_copy_micros,
            color_conversion_micros,
            encoder_call_micros,
            audio_capture_buffer_frames,
            audio_capture_discontinuities,
            audio_invalid_capture_timestamps,
            audio_device_reopens,
            audio_encoder_resets,
            audio_capture_queue_depth,
            audio_capture_queue_capacity,
            audio_capture_queue_drops,
        };
        Ok(Some(record))
    }

    fn submit_payload_digest(
        diagnostics: Option<&DiagnosticSender>,
        mut record: PayloadDigest,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(diagnostics) = diagnostics else {
            return Ok(());
        };
        record.producer_dropped_records = diagnostics.dropped.load(Ordering::Relaxed);
        match diagnostics.tx.try_send(record) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                diagnostics.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                Err("diagnostic stream writer stopped".into())
            }
        }
    }

    fn run_worker_video_stream(
        connection: Connection,
        runtime: tokio::runtime::Handle,
        worker: crate::worker::MediaWorker,
        selected: rustconsole_protocol::NegotiatedAv1Configuration,
        video_datagram_version: u32,
        controls: std::sync::mpsc::Receiver<WorkerVideoControl>,
        enable_audio: bool,
        audio_state: tokio::sync::watch::Sender<wire::AudioStreamState>,
        latest_input: Arc<Mutex<(u64, u64)>>,
        diagnostic_tx: Option<DiagnosticSender>,
    ) -> Result<WorkerVideoExit, Box<dyn std::error::Error + Send + Sync>> {
        let _audio_recovery = AudioRecoveryGuard;
        let frames_per_second = selected.frames_per_second;
        let bitrate_bits_per_second = selected.maximum_bitrate_bits_per_second;
        let mut controller = AdaptiveBitrateController::new(bitrate_bits_per_second);
        let clock = crate::clock::HostClock::new()?;
        let (_shutdown_tx, shutdown_rx) = shutdown_channel();
        let Some(mut stream) = worker
            .start_video_stream(
                &shutdown_rx,
                frames_per_second,
                controller.target_bits_per_second(),
                enable_audio,
                diagnostic_tx.is_some(),
            )
            .map_err(|error| error.to_string())?
        else {
            return Ok(WorkerVideoExit::Stopped);
        };
        let mut video_pending = std::collections::VecDeque::<bytes::Bytes>::new();
        let mut video_pending_diagnostic = None::<PayloadDigest>;
        let mut video_started = Instant::now();
        let mut audio_pending = std::collections::VecDeque::<bytes::Bytes>::new();
        let mut audio_pending_diagnostic = None::<PayloadDigest>;
        let mut audio_queued_at = 0_u64;
        let mut local_audio_drops = 0_u64;
        let mut worker_audio_drops = 0_u64;
        let mut recovery = rustconsole_host_core::video_recovery::VideoRecovery::default();
        let frame_period = Duration::from_nanos(1_000_000_000 / u64::from(frames_per_second));
        let mut observed_worker_drops = stream.dropped_events();
        loop {
            loop {
                match controls.try_recv() {
                    Ok(WorkerVideoControl::RequestKeyframe) => {
                        recovery.require_keyframe();
                    }
                    Ok(WorkerVideoControl::ReceiverReport(report)) => {
                        let path = connection.stats().path;
                        let change = controller.observe(
                            VideoPathReport {
                                round_trip_time: path.rtt,
                                congestion_window_bytes: path.cwnd,
                                lost_packets: path.lost_packets,
                            },
                            VideoDeliveryReport {
                                completed_payload_bytes: report.completed_payload_bytes,
                                measurement_interval_micros: report.measurement_interval_micros,
                                lost_chunks: report.lost_chunks,
                                late_chunks: report.late_chunks,
                                assembly_overflows: report.assembly_overflows,
                                incomplete_frames: report.incomplete_frames,
                            },
                        );
                        if let Some(change) = change {
                            stream.set_bitrate(change.target_bits_per_second)?;
                        }
                    }
                    Ok(WorkerVideoControl::Stop)
                    | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Ok(WorkerVideoExit::Stopped);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                }
            }
            if connection.close_reason().is_some() {
                return Ok(WorkerVideoExit::Stopped);
            }
            let worker_drops = stream.dropped_events();
            if worker_drops > observed_worker_drops {
                observed_worker_drops = worker_drops;
                recovery.require_keyframe();
                if let Some(change) = controller.observe_sender_congestion() {
                    stream.set_bitrate(change.target_bits_per_second)?;
                }
            }
            if recovery.request_due(Instant::now()) {
                stream.request_keyframe()?;
            }
            if enable_audio
                && audio_pending.is_empty()
                && let Some(event) = stream.next_audio_event()
            {
                match event {
                    Ok(crate::worker_protocol::AudioWorkerEvent::Packet {
                        queued_at_micros,
                        payload_sha256,
                        hash_duration_micros,
                        encode_started_at_micros,
                        encoded_at_micros,
                        capture_buffer_frames,
                        capture_discontinuities,
                        invalid_capture_timestamps,
                        device_reopens,
                        encoder_resets,
                        capture_queue_depth,
                        capture_queue_capacity,
                        capture_queue_drops,
                        packet,
                    }) => {
                        let service_received_at_micros = clock.now()?;
                        let mut prepared_diagnostic = prepare_payload_digest(
                            diagnostic_tx.as_ref(),
                            &clock,
                            MediaKind::Audio,
                            packet.generation,
                            packet.sequence,
                            &packet.payload,
                            payload_sha256,
                            hash_duration_micros,
                            encode_started_at_micros,
                            encoded_at_micros,
                            queued_at_micros,
                            service_received_at_micros,
                            clock.now()?,
                            packet.captured_at_micros,
                            0,
                            0,
                            0,
                            0,
                            0,
                            capture_buffer_frames,
                            capture_discontinuities,
                            invalid_capture_timestamps,
                            device_reopens,
                            encoder_resets,
                            capture_queue_depth,
                            capture_queue_capacity,
                            capture_queue_drops,
                            None,
                        )?;
                        audio_state.send_if_modified(|state| {
                            if packet.generation > state.generation {
                                state.generation = packet.generation;
                                state.status = wire::AudioStatus::Active as i32;
                                state.detail.clear();
                                true
                            } else {
                                false
                            }
                        });
                        if rustconsole_host_core::audio_transport::expired(
                            queued_at_micros,
                            clock.now()?,
                        ) {
                            local_audio_drops += 1;
                            if let Some(record) = prepared_diagnostic.take() {
                                submit_payload_digest(diagnostic_tx.as_ref(), record)?;
                            }
                        } else if let Some(maximum) = connection.max_datagram_size() {
                            match rustconsole_host_core::audio_transport::packetize(
                                &packet, maximum,
                            ) {
                                Ok(pieces) => {
                                    audio_pending =
                                        pieces.into_iter().map(bytes::Bytes::from).collect();
                                    audio_queued_at = queued_at_micros;
                                    if let Some(record) = prepared_diagnostic.as_mut() {
                                        record.packetization_completed_at_micros = clock.now()?;
                                    }
                                    audio_pending_diagnostic = prepared_diagnostic;
                                }
                                Err(_) => {
                                    local_audio_drops += 1;
                                    if let Some(record) = prepared_diagnostic.take() {
                                        submit_payload_digest(diagnostic_tx.as_ref(), record)?;
                                    }
                                }
                            }
                        } else {
                            local_audio_drops += 1;
                            if let Some(record) = prepared_diagnostic.take() {
                                submit_payload_digest(diagnostic_tx.as_ref(), record)?;
                            }
                        }
                    }
                    Ok(crate::worker_protocol::AudioWorkerEvent::State(mut state)) => {
                        if state.generation < audio_state.borrow().generation {
                            continue;
                        }
                        worker_audio_drops = state.dropped_packets;
                        state.dropped_packets += local_audio_drops + stream.audio_dropped();
                        audio_state.send_replace(state);
                    }
                    Err(error) => {
                        let generation = audio_state.borrow().generation;
                        audio_state.send_replace(wire::AudioStreamState::new(
                            generation,
                            wire::AudioStatus::Failed,
                            local_audio_drops + worker_audio_drops + stream.audio_dropped(),
                            error.to_string(),
                        ));
                    }
                }
            }
            if !audio_pending.is_empty()
                && rustconsole_host_core::audio_transport::expired(audio_queued_at, clock.now()?)
            {
                audio_pending.clear();
                local_audio_drops += 1;
                if let Some(record) = audio_pending_diagnostic.take() {
                    submit_payload_digest(diagnostic_tx.as_ref(), record)?;
                }
            }
            if let Some(datagram) = audio_pending.front() {
                if let Some(record) = audio_pending_diagnostic.as_mut()
                    && record.first_send_attempt_at_micros == 0
                {
                    record.first_send_attempt_at_micros = clock.now()?;
                }
                match runtime.block_on(rustconsole_host_core::authentication::send_media_datagram(
                    &connection,
                    datagram.clone(),
                    Duration::from_millis(1),
                )) {
                    Ok(true) => {
                        audio_pending.pop_front();
                        if audio_pending.is_empty()
                            && let Some(mut record) = audio_pending_diagnostic.take()
                        {
                            record.last_send_completed_at_micros = clock.now()?;
                            submit_payload_digest(diagnostic_tx.as_ref(), record)?;
                        }
                    }
                    Ok(false) => {}
                    Err(
                        quinn::SendDatagramError::TooLarge
                        | quinn::SendDatagramError::UnsupportedByPeer
                        | quinn::SendDatagramError::Disabled,
                    ) => {
                        audio_pending.clear();
                        local_audio_drops += 1;
                        if let Some(record) = audio_pending_diagnostic.take() {
                            submit_payload_digest(diagnostic_tx.as_ref(), record)?;
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            audio_state.send_if_modified(|state| {
                let drops = local_audio_drops + worker_audio_drops + stream.audio_dropped();
                if state.dropped_packets == drops {
                    false
                } else {
                    state.dropped_packets = drops;
                    true
                }
            });
            let send_deadline = rustconsole_host_core::video_transport::assembly_deadline(
                frame_period,
                connection.rtt(),
            );
            if !video_pending.is_empty() && video_started.elapsed() >= send_deadline {
                video_pending.clear();
                if let Some(change) = controller.observe_sender_congestion() {
                    stream.set_bitrate(change.target_bits_per_second)?;
                }
                if let Some(record) = video_pending_diagnostic.take() {
                    submit_payload_digest(diagnostic_tx.as_ref(), record)?;
                }
                recovery.require_keyframe();
            }
            if video_pending.is_empty()
                && let Some(event) = stream
                    .next_event_timeout(Duration::ZERO)
                    .map_err(|error| error.to_string())?
            {
                match event {
                    WorkerEvent::EncodedVideoFrame {
                        sequence,
                        last_present_time,
                        keyframe,
                        payload_sha256,
                        hash_duration_micros,
                        encode_started_at_micros,
                        encoded_at_micros,
                        worker_queued_at_micros,
                        mirror_decode_micros,
                        capture_acquisition_micros,
                        cross_adapter_copy_micros,
                        color_conversion_micros,
                        encoder_call_micros,
                        quality,
                        payload,
                        ..
                    } => {
                        let service_received_at_micros = clock.now()?;
                        if !recovery.accept(sequence, keyframe) {
                            continue;
                        }
                        let Some(maximum_datagram_size) = connection.max_datagram_size() else {
                            return Err("QUIC peer does not support datagrams".into());
                        };
                        let captured_at_micros = clock.ticks_to_micros(last_present_time)?;
                        let (input_sequence, input_submitted_at_micros) = *latest_input
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        let input_sequence = if input_submitted_at_micros != 0
                            && captured_at_micros >= input_submitted_at_micros
                        {
                            input_sequence
                        } else {
                            0
                        };
                        let packetized_at_micros = clock.now()?;
                        let mut prepared_diagnostic = prepare_payload_digest(
                            diagnostic_tx.as_ref(),
                            &clock,
                            MediaKind::Video,
                            1,
                            sequence,
                            &payload,
                            payload_sha256,
                            hash_duration_micros,
                            encode_started_at_micros,
                            encoded_at_micros,
                            worker_queued_at_micros,
                            service_received_at_micros,
                            packetized_at_micros,
                            captured_at_micros,
                            mirror_decode_micros,
                            capture_acquisition_micros,
                            cross_adapter_copy_micros,
                            color_conversion_micros,
                            encoder_call_micros,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            0,
                            quality,
                        )?;
                        let frame = rustconsole_host_core::video_transport::VideoFramePayload {
                            sequence,
                            captured_at_micros,
                            encoded_at_micros,
                            packetized_at_micros,
                            input_sequence,
                            keyframe,
                            target_bitrate_bits_per_second: controller.target_bits_per_second(),
                            estimated_capacity_bits_per_second: controller
                                .estimated_capacity_bits_per_second(),
                            soft_ceiling_bits_per_second: controller.soft_ceiling_bits_per_second(),
                            payload,
                        };
                        let datagrams =
                            rustconsole_host_core::video_transport::packetize_video_frame_for_version(
                                &frame,
                                maximum_datagram_size,
                                video_datagram_version,
                            )?;
                        if let Some(record) = prepared_diagnostic.as_mut() {
                            record.packetization_completed_at_micros = clock.now()?;
                        }
                        video_pending = datagrams.into_iter().map(bytes::Bytes::from).collect();
                        video_pending_diagnostic = prepared_diagnostic;
                        video_started = Instant::now();
                    }
                    WorkerEvent::Failure(error) => return Err(error.into()),
                    WorkerEvent::VideoReconfigurationRequired(cause) => {
                        return Ok(WorkerVideoExit::ReconfigurationRequired(cause));
                    }
                    _ => return Err("media worker returned an unexpected stream event".into()),
                }
            }
            if let Some(datagram) = video_pending.front() {
                if let Some(record) = video_pending_diagnostic.as_mut()
                    && record.first_send_attempt_at_micros == 0
                {
                    record.first_send_attempt_at_micros = clock.now()?;
                }
                match runtime.block_on(rustconsole_host_core::authentication::send_media_datagram(
                    &connection,
                    datagram.clone(),
                    Duration::from_millis(1),
                )) {
                    Ok(true) => {
                        video_pending.pop_front();
                        if video_pending.is_empty() {
                            if let Some(mut record) = video_pending_diagnostic.take() {
                                record.last_send_completed_at_micros = clock.now()?;
                                submit_payload_digest(diagnostic_tx.as_ref(), record)?;
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(quinn::SendDatagramError::TooLarge) => {
                        video_pending.clear();
                        recovery.require_keyframe();
                        if let Some(record) = video_pending_diagnostic.take() {
                            submit_payload_digest(diagnostic_tx.as_ref(), record)?;
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            } else if audio_pending.is_empty() {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn parse_viewer_offer(
        envelope: Envelope,
    ) -> Result<(Vec<DomainCapability>, DomainSettings), Box<dyn std::error::Error>> {
        let offer = match envelope.body {
            Some(envelope::Body::Av1CapabilityOffer(offer))
                if offer.encoder_capabilities.is_empty() =>
            {
                offer
            }
            _ => return Err("viewer sent an invalid AV1 capability offer".into()),
        };
        let settings = offer.viewer_settings.ok_or("viewer omitted AV1 settings")?;
        if settings.width == 0
            || settings.height == 0
            || settings.frames_per_second == 0
            || settings.maximum_bitrate_bits_per_second == 0
            || settings.mode_preferences.is_empty()
        {
            return Err("viewer settings do not match the negotiated video mode".into());
        }
        let capabilities = offer
            .decoder_capabilities
            .into_iter()
            .map(|capability| {
                if capability.chroma_subsampling != wire::ChromaSubsampling::Yuv420 as i32 {
                    return Err("viewer advertised an unsupported AV1 mode".into());
                }
                let bit_depth = match wire::VideoBitDepth::try_from(capability.bit_depth) {
                    Ok(wire::VideoBitDepth::Eight) => VideoBitDepth::Eight,
                    Ok(wire::VideoBitDepth::Ten) => VideoBitDepth::Ten,
                    _ => return Err("viewer advertised an unsupported AV1 bit depth".into()),
                };
                Ok(DomainCapability {
                    mode: DomainMode {
                        chroma_subsampling: ChromaSubsampling::Yuv420,
                        bit_depth,
                    },
                    maximum_width: capability.maximum_width,
                    maximum_height: capability.maximum_height,
                    maximum_frames_per_second: u16::try_from(capability.maximum_frames_per_second)?,
                })
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
        Ok((
            capabilities,
            DomainSettings {
                width: settings.width,
                height: settings.height,
                frames_per_second: u16::try_from(settings.frames_per_second)?,
                mode_preferences: settings
                    .mode_preferences
                    .into_iter()
                    .map(|mode| {
                        let bit_depth = match wire::VideoBitDepth::try_from(mode.bit_depth) {
                            Ok(wire::VideoBitDepth::Eight) => Ok(VideoBitDepth::Eight),
                            Ok(wire::VideoBitDepth::Ten) => Ok(VideoBitDepth::Ten),
                            _ => Err("viewer requested an unsupported AV1 bit depth"),
                        }?;
                        if mode.chroma_subsampling != wire::ChromaSubsampling::Yuv420 as i32 {
                            return Err("viewer requested unsupported chroma subsampling");
                        }
                        Ok(DomainMode {
                            chroma_subsampling: ChromaSubsampling::Yuv420,
                            bit_depth,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                maximum_bitrate_bits_per_second: settings.maximum_bitrate_bits_per_second,
            },
        ))
    }

    fn wire_capability(capability: DomainCapability) -> Av1Capability {
        Av1Capability {
            chroma_subsampling: wire::ChromaSubsampling::Yuv420 as i32,
            bit_depth: match capability.mode.bit_depth {
                VideoBitDepth::Eight => wire::VideoBitDepth::Eight as i32,
                VideoBitDepth::Ten => wire::VideoBitDepth::Ten as i32,
            },
            maximum_width: capability.maximum_width,
            maximum_height: capability.maximum_height,
            maximum_frames_per_second: u32::from(capability.maximum_frames_per_second),
        }
    }

    fn wire_selected(
        selected: rustconsole_protocol::NegotiatedAv1Configuration,
    ) -> SelectedAv1Configuration {
        SelectedAv1Configuration {
            dedicated_input_stream: false,
            video_datagram_version:
                rustconsole_host_core::video_transport::LEGACY_VIDEO_DATAGRAM_VERSION,
            host_pointer_release: false,
            full_diagnostics: false,
            audio_transport: None,
            width: selected.width,
            height: selected.height,
            frames_per_second: u32::from(selected.frames_per_second),
            mode: Some(Av1Mode {
                chroma_subsampling: wire::ChromaSubsampling::Yuv420 as i32,
                bit_depth: match selected.mode.bit_depth {
                    VideoBitDepth::Eight => wire::VideoBitDepth::Eight as i32,
                    VideoBitDepth::Ten => wire::VideoBitDepth::Ten as i32,
                },
            }),
            maximum_bitrate_bits_per_second: selected.maximum_bitrate_bits_per_second,
        }
    }

    fn read_control(stream: &mut TcpStream) -> Result<Envelope, Box<dyn std::error::Error>> {
        let mut prefix = [0_u8; wire::RELIABLE_FRAME_PREFIX_SIZE];
        stream.read_exact(&mut prefix)?;
        let size = u32::from_be_bytes(prefix) as usize;
        if size > wire::MAX_RELIABLE_MESSAGE_SIZE {
            return Err("viewer control frame exceeds 64 KiB".into());
        }
        let mut frame = Vec::with_capacity(prefix.len() + size);
        frame.extend_from_slice(&prefix);
        frame.resize(prefix.len() + size, 0);
        stream.read_exact(&mut frame[prefix.len()..])?;
        Ok(wire::decode_reliable_frame(&frame)?)
    }

    fn write_control(
        stream: &mut TcpStream,
        envelope: Envelope,
    ) -> Result<(), Box<dyn std::error::Error>> {
        stream.write_all(&wire::encode_reliable_frame(&envelope)?)?;
        stream.flush()?;
        Ok(())
    }

    fn set_status(
        status_handle: &ServiceStatusHandle,
        current_state: ServiceState,
        controls_accepted: ServiceControlAccept,
        checkpoint: u32,
        wait_hint: Duration,
    ) -> windows_service::Result<()> {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state,
            controls_accepted,
            exit_code: ServiceExitCode::NO_ERROR,
            checkpoint,
            wait_hint,
            process_id: None,
        })
    }
}

#[cfg(any(windows, test))]
fn bgra_bmp(width: u32, height: u32, pixels: &[u8]) -> std::io::Result<Vec<u8>> {
    const HEADER_SIZE: usize = 54;
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "BMP row overflow"))?;
    let pixel_bytes = row_bytes
        .checked_mul(usize::try_from(height).unwrap_or(usize::MAX))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "BMP size overflow"))?;
    if pixels.len() != pixel_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "BGRA pixel length does not match BMP dimensions",
        ));
    }
    let file_size = HEADER_SIZE
        .checked_add(pixel_bytes)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "BMP is too large"))?;
    let width = i32::try_from(width).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "BMP width is too large")
    })?;
    let height = i32::try_from(height)
        .ok()
        .and_then(i32::checked_neg)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "BMP height is too large")
        })?;

    let mut output = Vec::with_capacity(usize::try_from(file_size).unwrap_or(HEADER_SIZE));
    output.extend_from_slice(b"BM");
    output.extend_from_slice(&file_size.to_le_bytes());
    output.extend_from_slice(&[0; 4]);
    output.extend_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
    output.extend_from_slice(&40_u32.to_le_bytes());
    output.extend_from_slice(&width.to_le_bytes());
    output.extend_from_slice(&height.to_le_bytes());
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&32_u16.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&u32::try_from(pixel_bytes).unwrap_or(0).to_le_bytes());
    output.extend_from_slice(&[0; 16]);
    output.extend_from_slice(pixels);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::TrySendError;

    #[test]
    fn duplicate_shutdown_signal_never_blocks_the_control_handler() {
        let (sender, receiver) = shutdown_channel();

        sender.try_send(()).unwrap();
        assert_eq!(sender.try_send(()), Err(TrySendError::Full(())));
        receiver.recv().unwrap();
    }

    #[test]
    fn availability_prioritizes_busy_then_desktop_state() {
        use rustconsole_protocol::wire::SessionAvailability;

        assert_eq!(session_availability(true, true), SessionAvailability::Busy);
        assert_eq!(session_availability(true, false), SessionAvailability::Busy);
        assert_eq!(
            session_availability(false, true),
            SessionAvailability::Available
        );
        assert_eq!(
            session_availability(false, false),
            SessionAvailability::DesktopSessionUnavailable
        );
    }

    #[test]
    fn vb_cable_detection_preserves_ready_unavailable_and_failure() {
        use rustconsole_protocol::wire::VbCableStatus;

        assert_eq!(vb_cable_status(Ok::<_, ()>(true)), VbCableStatus::Ready);
        assert_eq!(
            vb_cable_status(Ok::<_, ()>(false)),
            VbCableStatus::Unavailable
        );
        assert_eq!(
            vb_cable_status(Err::<bool, _>(())),
            VbCableStatus::CheckFailed
        );
    }

    #[test]
    fn bmp_uses_top_down_bgra_rows() {
        let pixels = [1, 2, 3, 4, 5, 6, 7, 8];
        let image = bgra_bmp(2, 1, &pixels).unwrap();

        assert_eq!(&image[..2], b"BM");
        assert_eq!(u32::from_le_bytes(image[2..6].try_into().unwrap()), 62);
        assert_eq!(i32::from_le_bytes(image[18..22].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(image[22..26].try_into().unwrap()), -1);
        assert_eq!(u16::from_le_bytes(image[28..30].try_into().unwrap()), 32);
        assert_eq!(&image[54..], pixels);
    }

    #[test]
    fn bmp_rejects_wrong_pixel_length() {
        assert!(bgra_bmp(2, 1, &[0; 4]).is_err());
    }
}
