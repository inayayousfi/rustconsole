use rustconsole_protocol::wire::PhysicalLinkKind;
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathKind {
    Unknown,
    Ip,
    TailscaleDirect,
    TailscaleDerp,
    TailscalePeerRelay,
    TailscaleUnknown,
}

impl PathKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ip => "IP (not Tailscale)",
            Self::TailscaleDirect => "Tailscale direct",
            Self::TailscaleDerp => "Tailscale DERP relay",
            Self::TailscalePeerRelay => "Tailscale peer relay",
            Self::TailscaleUnknown => "Tailscale path unknown",
        }
    }

    pub const fn code(self) -> u64 {
        match self {
            Self::Unknown => 0,
            Self::Ip => 1,
            Self::TailscaleDirect => 2,
            Self::TailscaleDerp => 3,
            Self::TailscalePeerRelay => 4,
            Self::TailscaleUnknown => 5,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NetworkPath {
    pub kind: PathKind,
    pub local_link: PhysicalLinkKind,
}

#[derive(Deserialize)]
struct Route {
    dev: String,
}

fn route_device(address: IpAddr) -> Option<String> {
    let output = Command::new("ip")
        .args(["-j", "route", "get", &address.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice::<Vec<Route>>(&output.stdout)
        .ok()?
        .into_iter()
        .next()
        .map(|route| route.dev)
}

fn link_for_device(device: &str) -> PhysicalLinkKind {
    if device.starts_with("wl")
        || std::path::Path::new("/sys/class/net")
            .join(device)
            .join("wireless")
            .exists()
    {
        PhysicalLinkKind::Wifi
    } else if device.starts_with("en") || device.starts_with("eth") {
        PhysicalLinkKind::Ethernet
    } else {
        PhysicalLinkKind::Other
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Peer {
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<IpAddr>,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    cur_addr: String,
    #[serde(default)]
    relay: String,
    #[serde(default)]
    peer_relay: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    peer: std::collections::HashMap<String, Peer>,
}

fn tailscale_path(bytes: &[u8], target: IpAddr) -> (PathKind, Option<IpAddr>) {
    let Ok(status) = serde_json::from_slice::<Status>(bytes) else {
        return (PathKind::TailscaleUnknown, None);
    };
    let Some(peer) = status
        .peer
        .values()
        .find(|peer| peer.tailscale_ips.contains(&target))
    else {
        return (PathKind::TailscaleUnknown, None);
    };
    if !peer.active {
        return (PathKind::TailscaleUnknown, None);
    }
    if !peer.cur_addr.is_empty() {
        let endpoint = peer.cur_addr.parse::<SocketAddr>().ok();
        return (PathKind::TailscaleDirect, endpoint.map(|addr| addr.ip()));
    }
    if !peer.peer_relay.is_empty() {
        return (PathKind::TailscalePeerRelay, None);
    }
    if !peer.relay.is_empty() {
        return (PathKind::TailscaleDerp, None);
    }
    (PathKind::TailscaleUnknown, None)
}

pub fn inspect(target: IpAddr) -> NetworkPath {
    let Some(device) = route_device(target) else {
        return NetworkPath {
            kind: PathKind::Unknown,
            local_link: PhysicalLinkKind::Unknown,
        };
    };
    if device != "tailscale0" {
        return NetworkPath {
            kind: PathKind::Ip,
            local_link: link_for_device(&device),
        };
    }
    let status = Command::new("tailscale")
        .args(["status", "--json"])
        .output();
    let (kind, endpoint) = status
        .ok()
        .filter(|status| status.status.success())
        .map_or((PathKind::TailscaleUnknown, None), |status| {
            tailscale_path(&status.stdout, target)
        });
    // A relay's current endpoint is not exposed here. The default internet
    // route is useful context but is not proof of the relay's actual route.
    let underlay = endpoint.or_else(|| "1.1.1.1".parse().ok());
    let local_link = underlay
        .and_then(route_device)
        .map_or(PhysicalLinkKind::Unknown, |device| link_for_device(&device));
    NetworkPath { kind, local_link }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_endpoint_wins_over_the_relay_home_region() {
        let status = br#"{"Peer":{"peer":{"TailscaleIPs":["100.64.0.1"],"Active":true,"CurAddr":"198.51.100.2:41641","Relay":"par"}}}"#;
        assert_eq!(
            tailscale_path(status, "100.64.0.1".parse().unwrap()),
            (
                PathKind::TailscaleDirect,
                Some("198.51.100.2".parse().unwrap())
            )
        );
    }

    #[test]
    fn relay_needs_an_active_peer_without_a_direct_endpoint() {
        let status =
            br#"{"Peer":{"peer":{"TailscaleIPs":["100.64.0.1"],"Active":true,"Relay":"par"}}}"#;
        assert_eq!(
            tailscale_path(status, "100.64.0.1".parse().unwrap()),
            (PathKind::TailscaleDerp, None)
        );
    }

    #[test]
    fn peer_relay_is_not_misreported_as_derp() {
        let status = br#"{"Peer":{"peer":{"TailscaleIPs":["100.64.0.1"],"Active":true,"PeerRelay":"198.51.100.2:1234:vni:7","Relay":"par"}}}"#;
        assert_eq!(
            tailscale_path(status, "100.64.0.1".parse().unwrap()),
            (PathKind::TailscalePeerRelay, None)
        );
    }
}
