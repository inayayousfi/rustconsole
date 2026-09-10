use rustconsole_client_core::{
    DiscoveredEndpoint, DiscoverySource, HostAvailability, LaunchRequest, PlayerEvent,
    PlayerProcess, authenticate_host_with, discover_manual as discover_manual_routes,
    probe_host_with, refresh_discovery_with_updates, resolve_host_addresses,
};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};
use zeroize::Zeroizing;

struct ClientRuntime {
    player: Mutex<Option<Arc<PlayerProcess>>>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeResponse {
    endpoint: String,
    host_identity: String,
    display_name: Option<String>,
    operating_system: &'static str,
    firewall_status: &'static str,
    availability: String,
    vb_cable_status: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryResponse {
    endpoints: Vec<DiscoveredEndpointResponse>,
    lan_error: Option<String>,
    tailscale_error: Option<String>,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveredEndpointResponse {
    endpoint: String,
    source: &'static str,
    source_name: Option<String>,
    host_identity: String,
    display_name: Option<String>,
    operating_system: &'static str,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryResultResponse {
    request_id: u64,
    endpoint: DiscoveredEndpointResponse,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryProgressResponse {
    request_id: u64,
    lan_completed: usize,
    lan_total: usize,
    tailscale_completed: usize,
    tailscale_total: usize,
}

const VB_CABLE_PAGE: &str = "https://www.vb-cable.com";
const VB_AUDIO_LICENSING_PAGE: &str = "https://vb-audio.com/Services/licensing.htm";

#[tauri::command]
fn probe(host: String, password: String) -> Result<ProbeResponse, String> {
    probe_address(&host, password).map(|(result, endpoint)| probe_response(result, endpoint))
}

#[tauri::command]
async fn discover(app: tauri::AppHandle, request_id: u64) -> Result<DiscoveryResponse, String> {
    tauri::async_runtime::spawn_blocking(move || {
        refresh_discovery_with_updates(
            |progress| {
                let _ = app.emit(
                    "discovery-progress",
                    DiscoveryProgressResponse {
                        request_id,
                        lan_completed: progress.lan_completed,
                        lan_total: progress.lan_total,
                        tailscale_completed: progress.tailscale_completed,
                        tailscale_total: progress.tailscale_total,
                    },
                );
            },
            |endpoint| {
                let _ = app.emit(
                    "discovery-result",
                    DiscoveryResultResponse {
                        request_id,
                        endpoint: discovered_response(endpoint),
                    },
                );
            },
        )
    })
    .await
    .map(discovery_response)
    .map_err(|error| format!("discovery task failed: {error}"))
}

#[tauri::command]
async fn discover_manual(host: String) -> Result<Vec<DiscoveredEndpointResponse>, String> {
    tauri::async_runtime::spawn_blocking(move || discover_manual_routes(host.trim()))
        .await
        .map_err(|error| format!("manual discovery task failed: {error}"))?
        .map(|endpoints| endpoints.into_iter().map(discovered_response).collect())
}

#[tauri::command]
fn open_vb_audio_page(page: String) -> Result<(), String> {
    let url = vb_audio_url(&page).ok_or("Unknown VB-Audio page")?;
    open_external_url(url)
}

fn vb_audio_url(page: &str) -> Option<&'static str> {
    match page {
        "product" => Some(VB_CABLE_PAGE),
        "licensing" => Some(VB_AUDIO_LICENSING_PAGE),
        _ => None,
    }
}

#[cfg(target_os = "windows")]
fn open_external_url(url: &str) -> Result<(), String> {
    Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open the default browser: {error}"))
}

#[cfg(target_os = "macos")]
fn open_external_url(url: &str) -> Result<(), String> {
    Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open the default browser: {error}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_external_url(url: &str) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not open the default browser: {error}"))
}

fn probe_address(
    host: &str,
    password: String,
) -> Result<
    (
        rustconsole_client_core::HostProbeResult,
        std::net::SocketAddr,
    ),
    String,
> {
    let discovered = discover_manual_routes(host.trim())?;
    if discovered.is_empty() {
        return Err("No Rust Console service found on this address and port".to_owned());
    }
    let password = Zeroizing::new(password.into_bytes());
    let mut errors = Vec::new();
    for route in discovered {
        let credential_error = Arc::new(Mutex::new(None));
        let lookup_error = Arc::clone(&credential_error);
        let result = probe_host_with(route.address, |host_identity| {
            if !password.is_empty() {
                return Some(password.to_vec());
            }
            let result = keyring::Entry::new(
                "org.rustconsole.viewer",
                &host_identity_key(host_identity.as_bytes()),
            )
            .and_then(|entry| entry.get_secret());
            match result {
                Ok(secret) => Some(secret),
                Err(error) => {
                    *lookup_error.lock().unwrap() = Some(error.to_string());
                    None
                }
            }
        });
        match result {
            Ok(result) => return Ok((result, route.address)),
            Err(error) => errors.push(credential_error.lock().unwrap().take().map_or_else(
                || error_chain(error.as_ref()),
                |lookup| format!("Remembered credential unavailable: {lookup}"),
            )),
        }
    }
    Err(errors
        .into_iter()
        .next()
        .unwrap_or_else(|| "No Rust Console service found on this address and port".to_owned()))
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

fn availability_name(result: &rustconsole_client_core::HostProbeResult) -> String {
    match result.availability {
        HostAvailability::Available => "available",
        HostAvailability::Busy => "busy",
        HostAvailability::DesktopSessionUnavailable => "desktop-session-unavailable",
    }
    .into()
}

fn probe_response(
    result: rustconsole_client_core::HostProbeResult,
    endpoint: std::net::SocketAddr,
) -> ProbeResponse {
    let availability = availability_name(&result);
    ProbeResponse {
        endpoint: endpoint.to_string(),
        host_identity: host_identity_key(result.host_identity.as_bytes()),
        display_name: result.display_name,
        operating_system: operating_system_name(result.operating_system),
        firewall_status: firewall_status_name(result.firewall_status),
        availability,
        vb_cable_status: vb_cable_status_name(result.vb_cable).into(),
    }
}

fn discovery_response(result: rustconsole_client_core::DiscoveryRefresh) -> DiscoveryResponse {
    DiscoveryResponse {
        endpoints: result
            .endpoints
            .into_iter()
            .map(discovered_response)
            .collect(),
        lan_error: result.lan_error,
        tailscale_error: result.tailscale_error,
    }
}

fn discovered_response(endpoint: DiscoveredEndpoint) -> DiscoveredEndpointResponse {
    DiscoveredEndpointResponse {
        endpoint: endpoint.address.to_string(),
        source: match endpoint.source {
            DiscoverySource::Manual => "manual",
            DiscoverySource::Lan => "lan",
            DiscoverySource::Tailscale => "tailscale",
        },
        source_name: endpoint.source_name,
        host_identity: host_identity_key(endpoint.host_identity.as_bytes()),
        display_name: endpoint.display_name,
        operating_system: operating_system_name(endpoint.operating_system),
    }
}

fn operating_system_name(
    operating_system: rustconsole_client_core::HostOperatingSystem,
) -> &'static str {
    match operating_system {
        rustconsole_client_core::HostOperatingSystem::Unknown => "unknown",
        rustconsole_client_core::HostOperatingSystem::Windows => "windows",
    }
}

fn firewall_status_name(status: rustconsole_client_core::HostFirewallStatus) -> &'static str {
    use rustconsole_client_core::HostFirewallStatus::*;
    match status {
        Unknown => "unknown",
        Missing => "missing",
        PrivateLocalSubnet => "private-local-subnet",
        AllProfilesLocalSubnet => "all-profiles-local-subnet",
        AllAddresses => "all-addresses",
        CheckFailed => "check-failed",
    }
}

fn vb_cable_status_name(status: rustconsole_client_core::VbCableAvailability) -> &'static str {
    match status {
        rustconsole_client_core::VbCableAvailability::Unspecified => "unspecified",
        rustconsole_client_core::VbCableAvailability::Ready => "ready",
        rustconsole_client_core::VbCableAvailability::Unavailable => "unavailable",
        rustconsole_client_core::VbCableAvailability::CheckFailed => "check-failed",
    }
}

fn host_identity_key(identity: &[u8; 32]) -> String {
    identity.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn authenticate_address(host: &str) -> Result<(), String> {
    let addresses = resolve_host_addresses(host.trim())?;
    let mut errors = Vec::new();
    for address in addresses {
        match authenticate_host_with(address, |host_identity| {
            keyring::Entry::new(
                "org.rustconsole.viewer",
                &host_identity_key(host_identity.as_bytes()),
            )
            .ok()?
            .get_secret()
            .ok()
        }) {
            Ok(_) => return Ok(()),
            Err(error) => errors.push(error_chain(error.as_ref())),
        }
    }
    Err(errors
        .into_iter()
        .next()
        .unwrap_or_else(|| "host has no address".to_owned()))
}

#[tauri::command]
fn start(
    app: tauri::AppHandle,
    state: tauri::State<'_, ClientRuntime>,
    host: String,
    password: String,
    remember: bool,
    maximum_bitrate_mbps: u64,
    latency_diagnostics: bool,
) -> Result<(), String> {
    let maximum_bitrate_bits_per_second = maximum_bitrate(maximum_bitrate_mbps)?;
    let address = resolve_host_addresses(host.trim())?
        .into_iter()
        .next()
        .ok_or("host has no address")?;
    let password = Zeroizing::new(password.into_bytes());
    let mut active = state.player.lock().unwrap();
    if active.is_some() {
        return Err("A player session is already active.".into());
    }
    let request = LaunchRequest {
        address,
        password: Zeroizing::new(password.to_vec()),
        remember_password: remember,
        maximum_bitrate_bits_per_second,
        latency_diagnostics,
    };
    let player = Arc::new(
        PlayerProcess::launch(&player_executable()?, &request)
            .map_err(|error| format!("Could not launch rustconsole-player: {error}"))?,
    );
    *active = Some(Arc::clone(&player));
    drop(active);

    std::thread::spawn(move || supervise_player(&app, player));
    Ok(())
}

fn maximum_bitrate(megabits_per_second: u64) -> Result<u64, String> {
    if !(5..=100).contains(&megabits_per_second) {
        return Err("Maximum bitrate must be between 5 and 100 Mbit/s.".into());
    }
    Ok(megabits_per_second * 1_000_000)
}

#[tauri::command]
fn disconnect(state: tauri::State<'_, ClientRuntime>) -> Result<(), String> {
    let player = state.player.lock().unwrap().clone();
    match player {
        Some(player) => player.stop().map_err(|error| error.to_string()),
        None => Ok(()),
    }
}

fn player_executable() -> Result<PathBuf, String> {
    let current = std::env::current_exe()
        .map_err(|error| format!("Could not locate rustconsole-client: {error}"))?;
    let directory = current
        .parent()
        .ok_or("rustconsole-client has no executable directory")?;
    let name = if cfg!(windows) {
        "rustconsole-player.exe"
    } else {
        "rustconsole-player"
    };
    let player = directory.join(name);
    if !player.is_file() {
        return Err(format!(
            "rustconsole-player is missing beside rustconsole-client at {}",
            player.display()
        ));
    }
    Ok(player)
}

fn supervise_player(app: &tauri::AppHandle, player: Arc<PlayerProcess>) {
    let error = loop {
        match player.next_event() {
            Ok(PlayerEvent::Authenticated { host_identity }) => {
                let identity = host_identity
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                let _ = app.emit("player-authenticated", identity);
            }
            Ok(PlayerEvent::Started) => {
                let _ = app.emit("player-started", ());
            }
            Ok(PlayerEvent::Ended) => {
                break None;
            }
            Ok(PlayerEvent::Error(error)) => {
                eprintln!("rustconsole-player: {error}");
                break Some(error);
            }
            Err(error) => {
                let message = match player.wait() {
                    Ok(status) => format!(
                        "player event channel closed before a complete event ({error}); player exited with {status}"
                    ),
                    Err(wait_error) => format!(
                        "player event channel closed before a complete event ({error}); its exit status could not be read: {wait_error}"
                    ),
                };
                eprintln!("rustconsole-player: {message}");
                break Some(message);
            }
        }
    };
    let _ = player.wait();
    let state = app.state::<ClientRuntime>();
    let mut active = state.player.lock().unwrap();
    if active
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &player))
    {
        *active = None;
    }
    drop(active);
    let _ = app.emit("player-ended", error);
}

fn main() -> ExitCode {
    let result = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => run_interface(),
        [command, host] if command == "probe-remembered" => {
            probe_address(host, String::new()).map(|(result, _)| {
                println!("availability={}", availability_name(&result));
            })
        }
        [command, host] if command == "authenticate-remembered" => {
            authenticate_address(host).map(|()| println!("authenticated=true"))
        }
        _ => Err(
            "expected no arguments, probe-remembered <host>, or authenticate-remembered <host>"
                .into(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustconsole-client: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_interface() -> Result<(), String> {
    tauri::Builder::default()
        .manage(ClientRuntime {
            player: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            probe,
            discover,
            discover_manual,
            start,
            disconnect,
            open_vb_audio_page
        ])
        .run(tauri::generate_context!())
        .map_err(|error| format!("client interface failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximum_bitrate_accepts_slider_boundaries() {
        assert_eq!(maximum_bitrate(5).unwrap(), 5_000_000);
        assert_eq!(maximum_bitrate(100).unwrap(), 100_000_000);
    }

    #[test]
    fn maximum_bitrate_rejects_values_outside_slider() {
        assert!(maximum_bitrate(4).is_err());
        assert!(maximum_bitrate(101).is_err());
    }

    #[test]
    fn browser_command_accepts_only_the_two_vb_audio_pages() {
        assert_eq!(vb_audio_url("product"), Some(VB_CABLE_PAGE));
        assert_eq!(vb_audio_url("licensing"), Some(VB_AUDIO_LICENSING_PAGE));
        assert_eq!(vb_audio_url("https://example.com"), None);
    }

    #[test]
    fn vb_cable_states_have_stable_interface_names() {
        use rustconsole_client_core::VbCableAvailability::*;

        assert_eq!(vb_cable_status_name(Unspecified), "unspecified");
        assert_eq!(vb_cable_status_name(Ready), "ready");
        assert_eq!(vb_cable_status_name(Unavailable), "unavailable");
        assert_eq!(vb_cable_status_name(CheckFailed), "check-failed");
    }
}
