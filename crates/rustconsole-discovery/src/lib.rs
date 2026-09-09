//! Discovery providers and candidate merging.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_DISCOVERY_PORT: u16 = 47_999;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoverySource {
    Manual,
    Lan,
    Tailscale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCandidate {
    pub source: DiscoverySource,
    pub display_name: Option<String>,
    pub addresses: Vec<SocketAddr>,
}

pub trait DiscoveryProvider {
    type Error;

    /// Performs one bounded refresh. Long-lived providers keep their own state.
    fn refresh(&mut self) -> Result<Vec<HostCandidate>, Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryError(String);

impl DiscoveryError {
    fn command(command: &Path, error: impl fmt::Display) -> Self {
        Self(format!("{} failed: {error}", command.display()))
    }

    fn output(command: &Path, stderr: &[u8]) -> Self {
        let detail = String::from_utf8_lossy(stderr).trim().to_owned();
        Self(if detail.is_empty() {
            format!("{} returned an error", command.display())
        } else {
            format!("{} returned an error: {detail}", command.display())
        })
    }

    fn json(source: &str, error: impl fmt::Display) -> Self {
        Self(format!("invalid {source} JSON: {error}"))
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DiscoveryError {}

pub struct TailscaleProvider {
    executable: PathBuf,
    port: u16,
}

impl TailscaleProvider {
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>, port: u16) -> Self {
        Self {
            executable: executable.into(),
            port,
        }
    }
}

impl Default for TailscaleProvider {
    fn default() -> Self {
        Self::new("tailscale", DEFAULT_DISCOVERY_PORT)
    }
}

impl DiscoveryProvider for TailscaleProvider {
    type Error = DiscoveryError;

    fn refresh(&mut self) -> Result<Vec<HostCandidate>, Self::Error> {
        let output = Command::new(&self.executable)
            .args(["status", "--json"])
            .output()
            .map_err(|error| DiscoveryError::command(&self.executable, error))?;
        if !output.status.success() {
            return Err(DiscoveryError::output(&self.executable, &output.stderr));
        }
        tailscale_candidates(&output.stdout, self.port)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TailscaleStatus {
    #[serde(default)]
    backend_state: String,
    #[serde(default)]
    peer: HashMap<String, TailscalePeer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TailscalePeer {
    #[serde(default)]
    host_name: String,
    #[serde(default, rename = "DNSName")]
    dns_name: String,
    #[serde(default, rename = "TailscaleIPs")]
    tailscale_ips: Vec<String>,
    #[serde(default)]
    online: bool,
}

pub fn tailscale_candidates(bytes: &[u8], port: u16) -> Result<Vec<HostCandidate>, DiscoveryError> {
    let status: TailscaleStatus =
        serde_json::from_slice(bytes).map_err(|error| DiscoveryError::json("Tailscale", error))?;
    if status.backend_state != "Running" {
        return Ok(Vec::new());
    }
    let mut candidates = status
        .peer
        .into_values()
        .filter(|peer| peer.online)
        .filter_map(|peer| {
            let mut addresses = peer
                .tailscale_ips
                .iter()
                .filter_map(|value| value.parse::<IpAddr>().ok())
                .map(|address| SocketAddr::new(address, port))
                .collect::<Vec<_>>();
            addresses.sort_unstable();
            addresses.dedup();
            if addresses.is_empty() {
                return None;
            }
            let display_name = if !peer.host_name.is_empty() {
                Some(peer.host_name)
            } else {
                let dns_name = peer.dns_name.trim_end_matches('.');
                (!dns_name.is_empty()).then(|| dns_name.to_owned())
            };
            Some(HostCandidate {
                source: DiscoverySource::Tailscale,
                display_name,
                addresses,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    Ok(candidates)
}

pub struct DefaultRouteLanProvider {
    ip_executable: PathBuf,
    port: u16,
}

impl DefaultRouteLanProvider {
    #[must_use]
    pub fn new(ip_executable: impl Into<PathBuf>, port: u16) -> Self {
        Self {
            ip_executable: ip_executable.into(),
            port,
        }
    }

    fn run_json(&self, arguments: &[&str]) -> Result<Vec<u8>, DiscoveryError> {
        let output = Command::new(&self.ip_executable)
            .args(arguments)
            .output()
            .map_err(|error| DiscoveryError::command(&self.ip_executable, error))?;
        if !output.status.success() {
            return Err(DiscoveryError::output(&self.ip_executable, &output.stderr));
        }
        Ok(output.stdout)
    }
}

impl Default for DefaultRouteLanProvider {
    fn default() -> Self {
        Self::new("ip", DEFAULT_DISCOVERY_PORT)
    }
}

impl DiscoveryProvider for DefaultRouteLanProvider {
    type Error = DiscoveryError;

    fn refresh(&mut self) -> Result<Vec<HostCandidate>, Self::Error> {
        let routes = self.run_json(&["-json", "route", "show", "default"])?;
        let interface = default_route_interface(&routes)?
            .ok_or_else(|| DiscoveryError("no default IPv4 route was found".to_owned()))?;
        let addresses = self.run_json(&["-json", "address", "show", "dev", &interface])?;
        let local = interface_ipv4_address(&addresses)?.ok_or_else(|| {
            DiscoveryError("the default route has no global IPv4 address".to_owned())
        })?;
        Ok(default_route_slash_24(local, self.port))
    }
}

#[derive(Deserialize)]
struct RouteEntry {
    #[serde(default)]
    dev: String,
}

fn default_route_interface(bytes: &[u8]) -> Result<Option<String>, DiscoveryError> {
    let routes: Vec<RouteEntry> =
        serde_json::from_slice(bytes).map_err(|error| DiscoveryError::json("route", error))?;
    Ok(routes
        .into_iter()
        .map(|route| route.dev)
        .find(|interface| !interface.is_empty()))
}

#[derive(Deserialize)]
struct InterfaceEntry {
    #[serde(default)]
    addr_info: Vec<AddressEntry>,
}

#[derive(Deserialize)]
struct AddressEntry {
    #[serde(default)]
    family: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    local: String,
}

fn interface_ipv4_address(bytes: &[u8]) -> Result<Option<Ipv4Addr>, DiscoveryError> {
    let interfaces: Vec<InterfaceEntry> = serde_json::from_slice(bytes)
        .map_err(|error| DiscoveryError::json("interface address", error))?;
    Ok(interfaces
        .into_iter()
        .flat_map(|interface| interface.addr_info)
        .find(|address| address.family == "inet" && address.scope == "global")
        .and_then(|address| address.local.parse().ok()))
}

#[must_use]
pub fn default_route_slash_24(local: Ipv4Addr, port: u16) -> Vec<HostCandidate> {
    // The first LAN provider assumes the default route's /24 is sufficient. Keeping
    // subnet enumeration behind DiscoveryProvider lets a later provider cover every
    // active interface without changing probing or client presentation.
    let local = u32::from(local);
    let network = local & 0xffff_ff00;
    (1..=254)
        .map(|host| network | host)
        .filter(|address| *address != local)
        .map(|address| HostCandidate {
            source: DiscoverySource::Lan,
            display_name: None,
            addresses: vec![SocketAddr::new(Ipv4Addr::from(address).into(), port)],
        })
        .collect()
}

#[must_use]
pub fn merge_candidates(candidates: impl IntoIterator<Item = HostCandidate>) -> Vec<HostCandidate> {
    let mut merged = BTreeMap::<SocketAddr, HostCandidate>::new();
    for candidate in candidates {
        for address in &candidate.addresses {
            let single = HostCandidate {
                source: candidate.source,
                display_name: candidate.display_name.clone(),
                addresses: vec![*address],
            };
            match merged.get(address) {
                Some(existing)
                    if source_priority(existing.source) <= source_priority(single.source) => {}
                _ => {
                    merged.insert(*address, single);
                }
            }
        }
    }
    let mut merged = merged.into_values().collect::<Vec<_>>();
    merged.sort_by_key(|candidate| (source_priority(candidate.source), candidate.addresses[0]));
    merged
}

const fn source_priority(source: DiscoverySource) -> u8 {
    match source {
        DiscoverySource::Manual => 0,
        DiscoverySource::Lan => 1,
        DiscoverySource::Tailscale => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_keeps_online_peers_and_tolerates_extra_fields() {
        let candidates = tailscale_candidates(
            br#"{
                "BackendState":"Running",
                "FutureField":{"anything":true},
                "Peer":{
                    "one":{"HostName":"windows-host","DNSName":"windows.ts.net.","OS":"windows","TailscaleIPs":["100.64.0.2","fd7a:115c:a1e0::2"],"Online":true,"NewField":7},
                    "two":{"HostName":"offline","TailscaleIPs":["100.64.0.3"],"Online":false},
                    "three":{"HostName":"invalid","TailscaleIPs":["not-an-ip"],"Online":true}
                }
            }"#,
            47_999,
        )
        .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].display_name.as_deref(), Some("windows-host"));
        assert_eq!(candidates[0].addresses.len(), 2);
    }

    #[test]
    fn stopped_tailscale_returns_no_candidates() {
        assert!(
            tailscale_candidates(br#"{"BackendState":"Stopped","Peer":{}}"#, 47_999)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn default_route_scan_is_bounded_and_excludes_special_addresses() {
        let candidates = default_route_slash_24(Ipv4Addr::new(192, 168, 1, 228), 47_999);
        assert_eq!(candidates.len(), 253);
        assert_eq!(
            candidates[0].addresses[0],
            "192.168.1.1:47999".parse().unwrap()
        );
        assert!(!candidates.iter().any(|candidate| {
            matches!(candidate.addresses[0].ip(), IpAddr::V4(address) if address.octets()[3] == 0 || address.octets()[3] == 228 || address.octets()[3] == 255)
        }));
    }

    #[test]
    fn parsers_select_the_default_interface_and_global_ipv4() {
        assert_eq!(
            default_route_interface(br#"[{"dst":"default","dev":"wlan0","metric":600}]"#)
                .unwrap()
                .as_deref(),
            Some("wlan0")
        );
        assert_eq!(
            interface_ipv4_address(
                br#"[{"addr_info":[{"family":"inet6","scope":"global","local":"::1"},{"family":"inet","scope":"global","local":"192.168.1.228"}]}]"#
            )
            .unwrap(),
            Some(Ipv4Addr::new(192, 168, 1, 228))
        );
    }

    #[test]
    fn merge_deduplicates_addresses_and_prefers_lan() {
        let address = "192.168.1.10:47999".parse().unwrap();
        let merged = merge_candidates([
            HostCandidate {
                source: DiscoverySource::Tailscale,
                display_name: Some("tailnet-host".to_owned()),
                addresses: vec![address],
            },
            HostCandidate {
                source: DiscoverySource::Lan,
                display_name: None,
                addresses: vec![address],
            },
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, DiscoverySource::Lan);
    }
}
