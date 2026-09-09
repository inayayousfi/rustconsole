//! Platform-neutral host selection, probing, and player supervision.

pub use rustconsole_player_core::process_protocol::{LaunchRequest, PlayerEvent};
use rustconsole_player_core::process_protocol::{
    read_event, write_launch, write_reconnect, write_stop,
};
pub use rustconsole_player_core::{HostAvailability, HostProbeResult, VbCableAvailability};
use std::io::{BufReader, BufWriter};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

pub const DEFAULT_HOST_PORT: u16 = 47_999;

pub use rustconsole_discovery::DiscoverySource;
pub use rustconsole_player_core::{
    DiscoveredEndpoint, DiscoveryProgress, HostFirewallStatus, HostOperatingSystem,
};

#[derive(Clone, Debug)]
pub struct DiscoveryRefresh {
    pub endpoints: Vec<DiscoveredEndpoint>,
    pub lan_error: Option<String>,
    pub tailscale_error: Option<String>,
}

pub fn refresh_discovery() -> DiscoveryRefresh {
    refresh_discovery_with_progress(|_| {})
}

pub fn refresh_discovery_with_progress(
    on_progress: impl FnMut(DiscoveryProgress),
) -> DiscoveryRefresh {
    refresh_discovery_with_updates(on_progress, |_| {})
}

pub fn refresh_discovery_with_updates(
    mut on_progress: impl FnMut(DiscoveryProgress),
    mut on_endpoint: impl FnMut(DiscoveredEndpoint),
) -> DiscoveryRefresh {
    use rustconsole_discovery::{DefaultRouteLanProvider, DiscoveryProvider, TailscaleProvider};

    let mut candidates = Vec::new();
    let mut lan = DefaultRouteLanProvider::default();
    let lan_error = match lan.refresh() {
        Ok(found) => {
            candidates.extend(found);
            None
        }
        Err(error) => Some(error.to_string()),
    };
    let mut tailscale = TailscaleProvider::default();
    let tailscale_error = match tailscale.refresh() {
        Ok(found) => {
            candidates.extend(found);
            None
        }
        Err(error) => Some(error.to_string()),
    };
    let endpoints = rustconsole_player_core::discover_candidates_with_updates(
        candidates,
        |update| match update {
            rustconsole_player_core::DiscoveryUpdate::Progress(progress) => on_progress(progress),
            rustconsole_player_core::DiscoveryUpdate::Endpoint(endpoint) => on_endpoint(endpoint),
        },
    )
    .unwrap_or_default();
    DiscoveryRefresh {
        endpoints,
        lan_error,
        tailscale_error,
    }
}

pub fn discover_manual(host: &str) -> Result<Vec<DiscoveredEndpoint>, String> {
    let addresses = resolve_host_addresses(host)?;
    rustconsole_player_core::discover_candidates(vec![rustconsole_discovery::HostCandidate {
        source: DiscoverySource::Manual,
        display_name: Some(host.to_owned()),
        addresses,
    }])
    .map_err(|error| error.to_string())
}

pub fn resolve_host(host: &str) -> Result<SocketAddr, String> {
    resolve_host_addresses(host)?
        .into_iter()
        .next()
        .ok_or_else(|| "host has no address".to_owned())
}

pub fn resolve_host_addresses(host: &str) -> Result<Vec<SocketAddr>, String> {
    let mut addresses = if let Ok(address) = host.parse::<SocketAddr>() {
        vec![address]
    } else if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, DEFAULT_HOST_PORT)]
    } else if host.rsplit_once(':').is_some() {
        host.to_socket_addrs()
            .map_err(|error| format!("invalid host address: {error}"))?
            .collect()
    } else {
        (host, DEFAULT_HOST_PORT)
            .to_socket_addrs()
            .map_err(|error| format!("invalid host address: {error}"))?
            .collect()
    };
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        return Err("host has no address".to_owned());
    }
    Ok(addresses)
}

pub fn probe_host(
    address: SocketAddr,
    password: Vec<u8>,
) -> Result<rustconsole_player_core::HostProbeResult, Box<dyn std::error::Error>> {
    rustconsole_player_core::probe_host(address, password)
}

pub fn probe_host_with(
    address: SocketAddr,
    password_for: impl FnOnce(rustconsole_player_core::HostIdentity) -> Option<Vec<u8>>,
) -> Result<rustconsole_player_core::HostProbeResult, Box<dyn std::error::Error>> {
    rustconsole_player_core::probe_host_with(address, password_for)
}

pub fn authenticate_host_with(
    address: SocketAddr,
    password_for: impl FnOnce(rustconsole_player_core::HostIdentity) -> Option<Vec<u8>>,
) -> Result<rustconsole_player_core::SessionIdentity, Box<dyn std::error::Error>> {
    rustconsole_player_core::authenticate_host_with(address, password_for)
}

pub struct PlayerProcess {
    child: Mutex<Child>,
    commands: Mutex<BufWriter<ChildStdin>>,
    events: Mutex<BufReader<ChildStdout>>,
}

impl PlayerProcess {
    pub fn launch(
        executable: &Path,
        request: &LaunchRequest,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut child = Command::new(executable)
            .arg("pipe-session")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("player command pipe is missing")?;
        let stdout = child.stdout.take().ok_or("player event pipe is missing")?;
        let mut process = Self {
            child: Mutex::new(child),
            commands: Mutex::new(BufWriter::new(stdin)),
            events: Mutex::new(BufReader::new(stdout)),
        };
        write_launch(process.commands.get_mut().unwrap(), request)?;
        Ok(process)
    }

    pub fn next_event(&self) -> Result<PlayerEvent, Box<dyn std::error::Error>> {
        Ok(read_event(&mut *self.events.lock().unwrap())?)
    }

    pub fn stop(&self) -> Result<(), Box<dyn std::error::Error>> {
        write_stop(&mut *self.commands.lock().unwrap())?;
        Ok(())
    }

    pub fn reconnect(&self) -> Result<(), Box<dyn std::error::Error>> {
        write_reconnect(&mut *self.commands.lock().unwrap())?;
        Ok(())
    }

    pub fn wait(&self) -> Result<std::process::ExitStatus, std::io::Error> {
        self.child.lock().unwrap().wait()
    }

    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.lock().unwrap().id()
    }
}

impl Drop for PlayerProcess {
    fn drop(&mut self) {
        let _ = write_stop(self.commands.get_mut().unwrap());
        let child = self.child.get_mut().unwrap();
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_resolution_supplies_the_production_port() {
        assert_eq!(
            resolve_host("127.0.0.1").unwrap(),
            "127.0.0.1:47999".parse().unwrap()
        );
        assert_eq!(
            resolve_host("127.0.0.1:48000").unwrap(),
            "127.0.0.1:48000".parse().unwrap()
        );
    }

    #[test]
    fn invalid_host_is_rejected() {
        assert!(resolve_host("not a host name / value").is_err());
    }

    #[test]
    fn host_resolution_returns_every_unique_address() {
        assert_eq!(
            resolve_host_addresses("127.0.0.1:48000").unwrap(),
            vec!["127.0.0.1:48000".parse().unwrap()]
        );
        assert_eq!(
            resolve_host_addresses("::1").unwrap(),
            vec!["[::1]:47999".parse().unwrap()]
        );
    }
}
