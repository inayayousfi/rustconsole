//! Platform-neutral player session orchestration.

pub mod process_protocol;
mod stream_receiver;
pub use stream_receiver::{AudioPlaybackEvent, AudioTransportSnapshot, StreamEnd};

use rustconsole_media::{AudioSamples, VideoFormat, VideoFrame};
use rustconsole_protocol::InputEvent;
use rustconsole_protocol::wire::{
    Av1CapabilityOffer, Av1HardwareCapability, Av1Mode, Av1ViewerSettings, ChromaSubsampling,
    Envelope, SelectedAv1Configuration, SessionAvailabilityProbe, VideoBitDepth, envelope,
};
pub use rustconsole_protocol::wire::{HostFirewallStatus, HostOperatingSystem};
use rustconsole_protocol::{
    Av1HardwareCapability as DomainCapability, Av1ViewerSettings as DomainSettings,
    ChromaSubsampling as DomainChroma, VideoBitDepth as DomainDepth, negotiate_av1_configuration,
};
pub use rustconsole_session::authentication::{HostIdentity, SessionIdentity};
pub use rustconsole_session::video_datagram::VideoFramePayload;
use rustconsole_session::video_datagram::{FrameAssemblyProgress, VideoAssemblyStats};
use std::net::SocketAddr;
use std::time::Duration;

pub const LAN_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(750);
pub const ROUTED_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
pub const MAX_CONCURRENT_DISCOVERY_PROBES: usize = 32;
pub const MAX_CONCURRENT_LAN_PROBES: usize = 24;
pub const MAX_CONCURRENT_ROUTED_PROBES: usize =
    MAX_CONCURRENT_DISCOVERY_PROBES - MAX_CONCURRENT_LAN_PROBES;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryProgress {
    pub lan_completed: usize,
    pub lan_total: usize,
    pub tailscale_completed: usize,
    pub tailscale_total: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredEndpoint {
    pub source: rustconsole_discovery::DiscoverySource,
    pub source_name: Option<String>,
    pub address: SocketAddr,
    pub host_identity: HostIdentity,
    pub display_name: Option<String>,
    pub operating_system: HostOperatingSystem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryUpdate {
    Progress(DiscoveryProgress),
    Endpoint(DiscoveredEndpoint),
}

struct CompletedDiscoveryProbe {
    source: rustconsole_discovery::DiscoverySource,
    endpoint: Option<DiscoveredEndpoint>,
}

pub fn discover_candidates(
    candidates: Vec<rustconsole_discovery::HostCandidate>,
) -> Result<Vec<DiscoveredEndpoint>, Box<dyn std::error::Error>> {
    discover_candidates_with_progress(candidates, |_| {})
}

pub fn discover_candidates_with_progress(
    candidates: Vec<rustconsole_discovery::HostCandidate>,
    mut on_progress: impl FnMut(DiscoveryProgress),
) -> Result<Vec<DiscoveredEndpoint>, Box<dyn std::error::Error>> {
    discover_candidates_with_updates(candidates, |update| {
        if let DiscoveryUpdate::Progress(progress) = update {
            on_progress(progress);
        }
    })
}

pub fn discover_candidates_with_updates(
    candidates: Vec<rustconsole_discovery::HostCandidate>,
    mut on_update: impl FnMut(DiscoveryUpdate),
) -> Result<Vec<DiscoveredEndpoint>, Box<dyn std::error::Error>> {
    let candidates = rustconsole_discovery::merge_candidates(candidates);
    let (lan, routed): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|candidate| candidate.source == rustconsole_discovery::DiscoverySource::Lan);
    let mut progress = DiscoveryProgress {
        lan_total: address_count(&lan),
        tailscale_total: routed
            .iter()
            .filter(|candidate| {
                candidate.source == rustconsole_discovery::DiscoverySource::Tailscale
            })
            .map(|candidate| candidate.addresses.len())
            .sum(),
        ..DiscoveryProgress::default()
    };
    on_update(DiscoveryUpdate::Progress(progress));
    let progress_events = address_count(&lan) + address_count(&routed);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let collect_updates = async {
            let mut discovered = Vec::new();
            for _ in 0..progress_events {
                let Some(completed) = progress_rx.recv().await else {
                    break;
                };
                record_discovery_probe(completed, &mut progress, &mut discovered, &mut on_update);
            }
            discovered
        };
        let (lan, routed, mut discovered) = tokio::join!(
            probe_candidates(lan, MAX_CONCURRENT_LAN_PROBES, progress_tx.clone()),
            probe_candidates(routed, MAX_CONCURRENT_ROUTED_PROBES, progress_tx),
            collect_updates,
        );
        lan?;
        routed?;
        discovered
            .sort_by_key(|endpoint| (discovery_source_priority(endpoint.source), endpoint.address));
        Ok(discovered)
    })
}

fn address_count(candidates: &[rustconsole_discovery::HostCandidate]) -> usize {
    candidates
        .iter()
        .flat_map(|candidate| &candidate.addresses)
        .count()
}

async fn probe_candidates(
    candidates: Vec<rustconsole_discovery::HostCandidate>,
    limit: usize,
    progress: tokio::sync::mpsc::UnboundedSender<CompletedDiscoveryProbe>,
) -> Result<(), Box<dyn std::error::Error>> {
    let work = candidates
        .into_iter()
        .flat_map(|candidate| {
            candidate
                .addresses
                .into_iter()
                .map(move |address| (candidate.source, candidate.display_name.clone(), address))
        })
        .collect::<Vec<_>>();
    for batch in work.chunks(limit) {
        let mut ipv4 = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
        ipv4.set_default_client_config(rustconsole_session::quic::opaque_client_config()?);
        let ipv6 = if batch.iter().any(|(_, _, address)| address.is_ipv6()) {
            let mut endpoint = quinn::Endpoint::client("[::]:0".parse()?)?;
            endpoint.set_default_client_config(rustconsole_session::quic::opaque_client_config()?);
            Some(endpoint)
        } else {
            None
        };
        let mut probes = tokio::task::JoinSet::new();
        for (source, source_name, address) in batch {
            let endpoint = if address.is_ipv4() {
                ipv4.clone()
            } else {
                ipv6.as_ref().expect("IPv6 endpoint was requested").clone()
            };
            let progress = progress.clone();
            let source_name = source_name.clone();
            let source = *source;
            let address = *address;
            probes.spawn(async move {
                let discovered = async {
                    let wait = if source == rustconsole_discovery::DiscoverySource::Lan {
                        LAN_DISCOVERY_TIMEOUT
                    } else {
                        ROUTED_DISCOVERY_TIMEOUT
                    };
                    let host = rustconsole_session::quic::discover_host(&endpoint, address, wait)
                        .await
                        .ok()?;
                    Some(DiscoveredEndpoint {
                        source,
                        source_name,
                        address,
                        host_identity: host.host_identity,
                        display_name: host.display_name,
                        operating_system: host.operating_system,
                    })
                }
                .await;
                let _ = progress.send(CompletedDiscoveryProbe {
                    source,
                    endpoint: discovered,
                });
            });
        }
        while probes.join_next().await.is_some() {}
        ipv4.close(0_u32.into(), b"discovery batch complete");
        if let Some(ipv6) = ipv6 {
            ipv6.close(0_u32.into(), b"discovery batch complete");
        }
    }
    drop(progress);
    Ok(())
}

fn record_discovery_probe(
    completed: CompletedDiscoveryProbe,
    progress: &mut DiscoveryProgress,
    discovered: &mut Vec<DiscoveredEndpoint>,
    on_update: &mut impl FnMut(DiscoveryUpdate),
) {
    if let Some(endpoint) = completed.endpoint {
        on_update(DiscoveryUpdate::Endpoint(endpoint.clone()));
        discovered.push(endpoint);
    }
    match completed.source {
        rustconsole_discovery::DiscoverySource::Lan => progress.lan_completed += 1,
        rustconsole_discovery::DiscoverySource::Tailscale => progress.tailscale_completed += 1,
        rustconsole_discovery::DiscoverySource::Manual => {}
    }
    on_update(DiscoveryUpdate::Progress(*progress));
}

const fn discovery_source_priority(source: rustconsole_discovery::DiscoverySource) -> u8 {
    match source {
        rustconsole_discovery::DiscoverySource::Manual => 0,
        rustconsole_discovery::DiscoverySource::Lan => 1,
        rustconsole_discovery::DiscoverySource::Tailscale => 2,
    }
}

fn client_endpoint(address: SocketAddr) -> Result<quinn::Endpoint, Box<dyn std::error::Error>> {
    let bind_address = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let mut endpoint = quinn::Endpoint::client(bind_address.parse()?)?;
    endpoint.set_default_client_config(rustconsole_session::quic::opaque_client_config()?);
    Ok(endpoint)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostAvailability {
    Available,
    Busy,
    DesktopSessionUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VbCableAvailability {
    Unspecified,
    Ready,
    Unavailable,
    CheckFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostProbeResult {
    pub host_identity: HostIdentity,
    pub display_name: Option<String>,
    pub operating_system: HostOperatingSystem,
    pub firewall_status: HostFirewallStatus,
    pub availability: HostAvailability,
    pub vb_cable: VbCableAvailability,
}

fn vb_cable_availability(status: i32) -> Result<VbCableAvailability, &'static str> {
    match rustconsole_protocol::wire::VbCableStatus::try_from(status) {
        Ok(rustconsole_protocol::wire::VbCableStatus::Unspecified) => {
            Ok(VbCableAvailability::Unspecified)
        }
        Ok(rustconsole_protocol::wire::VbCableStatus::Ready) => Ok(VbCableAvailability::Ready),
        Ok(rustconsole_protocol::wire::VbCableStatus::Unavailable) => {
            Ok(VbCableAvailability::Unavailable)
        }
        Ok(rustconsole_protocol::wire::VbCableStatus::CheckFailed) => {
            Ok(VbCableAvailability::CheckFailed)
        }
        Err(_) => Err("host returned an unknown VB-CABLE status"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamProgress {
    AudioTransport(AudioTransportSnapshot),
    KeyboardLeds {
        generation: u64,
        sequence: u64,
        mask: u8,
    },
    NegotiatingVideo,
    VideoNegotiated(rustconsole_protocol::NegotiatedAv1Configuration),
    WaitingForVideoPackets,
    ReceivingVideoPackets {
        frame: FrameAssemblyProgress,
        totals: VideoAssemblyStats,
    },
    FirstFrameAssembled {
        target_bitrate_bits_per_second: u64,
        estimated_capacity_bits_per_second: u64,
    },
}

pub struct StreamConsumers<Audio, Video> {
    pub audio: Audio,
    pub video: Video,
}

pub struct StreamHostParameters<PasswordFor, Authenticated, Progress, Stop, Input, Audio, Video> {
    pub address: SocketAddr,
    pub password_for: PasswordFor,
    pub on_authenticated: Authenticated,
    pub on_progress: Progress,
    pub video: (Vec<DomainCapability>, DomainSettings),
    pub should_stop: Stop,
    pub next_input: Input,
    pub consumers: StreamConsumers<Audio, Video>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamHostResult {
    pub identity: SessionIdentity,
    pub end: stream_receiver::StreamEnd,
}

pub fn authenticate_host(
    address: SocketAddr,
    password: Vec<u8>,
) -> Result<SessionIdentity, Box<dyn std::error::Error>> {
    authenticate_host_with(address, move |_| Some(password))
}

pub fn authenticate_host_with(
    address: SocketAddr,
    password_for: impl FnOnce(HostIdentity) -> Option<Vec<u8>>,
) -> Result<SessionIdentity, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let endpoint = client_endpoint(address)?;
        let authenticated = rustconsole_session::quic::connect_and_authenticate_with(
            &endpoint,
            address,
            password_for,
        )
        .await?;
        Ok(authenticated.session_identity)
    })
}

pub fn probe_host(
    address: SocketAddr,
    password: Vec<u8>,
) -> Result<HostProbeResult, Box<dyn std::error::Error>> {
    probe_host_with(address, move |_| Some(password))
}

pub fn probe_host_with(
    address: SocketAddr,
    password_for: impl FnOnce(HostIdentity) -> Option<Vec<u8>>,
) -> Result<HostProbeResult, Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let endpoint = client_endpoint(address)?;
        let authenticated = rustconsole_session::quic::connect_and_authenticate_with(
            &endpoint,
            address,
            password_for,
        )
        .await?;
        let (mut send, mut receive) = authenticated.connection.open_bi().await?;
        rustconsole_session::quic::write_envelope(
            &mut send,
            Envelope {
                body: Some(envelope::Body::SessionAvailabilityProbe(
                    SessionAvailabilityProbe {},
                )),
            },
        )
        .await?;
        send.finish()?;
        let (availability, vb_cable, firewall_status) = match rustconsole_session::quic::read_envelope(&mut receive)
            .await?
            .body
        {
            Some(envelope::Body::SessionAvailabilityResult(result)) => {
                let availability = match rustconsole_protocol::wire::SessionAvailability::try_from(
                    result.availability,
                )? {
                    rustconsole_protocol::wire::SessionAvailability::Available => {
                        HostAvailability::Available
                    }
                    rustconsole_protocol::wire::SessionAvailability::Busy => HostAvailability::Busy,
                    rustconsole_protocol::wire::SessionAvailability::DesktopSessionUnavailable => {
                        HostAvailability::DesktopSessionUnavailable
                    }
                    rustconsole_protocol::wire::SessionAvailability::Unspecified => {
                        return Err("host returned an unspecified session availability".into());
                    }
                };
                let vb_cable = vb_cable_availability(result.vb_cable_status)?;
                let firewall_status = HostFirewallStatus::try_from(result.firewall_status)
                    .unwrap_or(HostFirewallStatus::Unknown);
                (availability, vb_cable, firewall_status)
            }
            _ => return Err("host returned an invalid session availability response".into()),
        };
        authenticated
            .connection
            .close(0_u32.into(), b"probe complete");
        Ok(HostProbeResult {
            host_identity: authenticated.host_identity,
            display_name: authenticated.display_name,
            operating_system: authenticated.operating_system,
            firewall_status,
            availability,
            vb_cable,
        })
    })
}

pub fn stream_host<PasswordFor, Authenticated, Progress, Stop, Input, Audio, Video>(
    parameters: StreamHostParameters<
        PasswordFor,
        Authenticated,
        Progress,
        Stop,
        Input,
        Audio,
        Video,
    >,
) -> Result<StreamHostResult, Box<dyn std::error::Error>>
where
    PasswordFor: FnOnce(HostIdentity) -> Option<Vec<u8>>,
    Authenticated: FnOnce(HostIdentity) -> Result<(), Box<dyn std::error::Error>>,
    Progress: FnMut(StreamProgress),
    Stop: Fn() -> bool,
    Input: FnMut() -> Option<InputEvent> + Send + 'static,
    Audio: FnMut(AudioPlaybackEvent) -> Result<(), Box<dyn std::error::Error>>,
    Video: FnMut(
        VideoFramePayload,
        StreamTransportStatistics,
    ) -> Result<bool, Box<dyn std::error::Error>>,
{
    let StreamHostParameters {
        address,
        password_for,
        on_authenticated,
        mut on_progress,
        video,
        should_stop,
        next_input,
        consumers,
    } = parameters;
    let (decoder_capabilities, settings) = video;
    let StreamConsumers {
        audio: consume_audio,
        video: consume_video,
    } = consumers;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let endpoint = client_endpoint(address)?;
        let authenticated = rustconsole_session::quic::connect_and_authenticate_with(
            &endpoint,
            address,
            password_for,
        )
        .await?;
        let identity = authenticated.session_identity;
        on_authenticated(authenticated.host_identity)?;
        on_progress(StreamProgress::NegotiatingVideo);
        let connection = authenticated.connection;
        let (mut send, mut receive) = connection.open_bi().await?;
        rustconsole_session::quic::write_envelope(
            &mut send,
            Envelope {
                body: Some(envelope::Body::Av1CapabilityOffer(Av1CapabilityOffer {
                    audio_transport: Some(rustconsole_protocol::wire::AudioConfiguration::INITIAL),
                    encoder_capabilities: Vec::new(),
                    decoder_capabilities: decoder_capabilities
                        .iter()
                        .copied()
                        .map(wire_capability)
                        .collect(),
                    viewer_settings: Some(wire_settings(&settings)),
                })),
            },
        )
        .await?;
        let audio_transport;
        let host_capabilities = match rustconsole_session::quic::read_envelope(&mut receive)
            .await?
            .body
        {
            Some(envelope::Body::Av1CapabilityOffer(offer))
                if offer.decoder_capabilities.is_empty() && offer.viewer_settings.is_none() =>
            {
                audio_transport = offer
                    .audio_transport
                    .filter(|configuration| configuration.supported());
                offer
                    .encoder_capabilities
                    .iter()
                    .map(domain_capability)
                    .collect::<Result<Vec<_>, _>>()?
            }
            _ => return Err("host sent an invalid AV1 capability offer".into()),
        };
        let selected =
            negotiate_av1_configuration(&host_capabilities, &decoder_capabilities, &settings)?;
        let mut selected_wire = wire_selected(selected);
        selected_wire.audio_transport = audio_transport;
        rustconsole_session::quic::write_envelope(
            &mut send,
            Envelope {
                body: Some(envelope::Body::SelectedAv1Configuration(selected_wire)),
            },
        )
        .await?;
        match rustconsole_session::quic::read_envelope(&mut receive)
            .await?
            .body
        {
            Some(envelope::Body::SelectedAv1Configuration(peer)) if peer == selected_wire => {}
            _ => return Err("host selected a different AV1 configuration".into()),
        }
        on_progress(StreamProgress::VideoNegotiated(selected));
        on_progress(StreamProgress::WaitingForVideoPackets);

        let end = stream_receiver::receive_stream(stream_receiver::ReceiveStreamParameters {
            connection,
            control: (send, receive),
            fps: selected.frames_per_second,
            audio_enabled: audio_transport.is_some(),
            should_stop,
            next_input,
            progress: on_progress,
            consumers: StreamConsumers {
                audio: consume_audio,
                video: consume_video,
            },
        })
        .await?;
        Ok(StreamHostResult { identity, end })
    })
}

#[derive(Clone, Copy, Debug)]
pub struct StreamTransportStatistics {
    pub round_trip_time: Duration,
    pub assembly: VideoAssemblyStats,
}

fn wire_capability(capability: DomainCapability) -> Av1HardwareCapability {
    Av1HardwareCapability {
        chroma_subsampling: match capability.mode.chroma_subsampling {
            DomainChroma::Yuv420 => ChromaSubsampling::Yuv420 as i32,
            DomainChroma::Yuv422 => ChromaSubsampling::Yuv422 as i32,
            DomainChroma::Yuv444 => ChromaSubsampling::Yuv444 as i32,
        },
        bit_depth: match capability.mode.bit_depth {
            DomainDepth::Eight => VideoBitDepth::Eight as i32,
            DomainDepth::Ten => VideoBitDepth::Ten as i32,
        },
        maximum_width: capability.maximum_width,
        maximum_height: capability.maximum_height,
        maximum_frames_per_second: u32::from(capability.maximum_frames_per_second),
    }
}

fn domain_capability(
    capability: &Av1HardwareCapability,
) -> Result<DomainCapability, Box<dyn std::error::Error>> {
    Ok(DomainCapability {
        mode: rustconsole_protocol::Av1Mode {
            chroma_subsampling: match ChromaSubsampling::try_from(capability.chroma_subsampling)? {
                ChromaSubsampling::Yuv420 => DomainChroma::Yuv420,
                ChromaSubsampling::Yuv422 => DomainChroma::Yuv422,
                ChromaSubsampling::Yuv444 => DomainChroma::Yuv444,
                ChromaSubsampling::Unspecified => return Err("unspecified AV1 chroma mode".into()),
            },
            bit_depth: match VideoBitDepth::try_from(capability.bit_depth)? {
                VideoBitDepth::Eight => DomainDepth::Eight,
                VideoBitDepth::Ten => DomainDepth::Ten,
                VideoBitDepth::Unspecified => return Err("unspecified AV1 bit depth".into()),
            },
        },
        maximum_width: capability.maximum_width,
        maximum_height: capability.maximum_height,
        maximum_frames_per_second: u16::try_from(capability.maximum_frames_per_second)?,
    })
}

fn wire_settings(settings: &DomainSettings) -> Av1ViewerSettings {
    Av1ViewerSettings {
        width: settings.width,
        height: settings.height,
        frames_per_second: u32::from(settings.frames_per_second),
        mode_preferences: settings
            .mode_preferences
            .iter()
            .map(|mode| Av1Mode {
                chroma_subsampling: wire_capability(DomainCapability {
                    mode: *mode,
                    maximum_width: 1,
                    maximum_height: 1,
                    maximum_frames_per_second: 1,
                })
                .chroma_subsampling,
                bit_depth: wire_capability(DomainCapability {
                    mode: *mode,
                    maximum_width: 1,
                    maximum_height: 1,
                    maximum_frames_per_second: 1,
                })
                .bit_depth,
            })
            .collect(),
        maximum_bitrate_bits_per_second: settings.maximum_bitrate_bits_per_second,
    }
}

fn wire_selected(
    selected: rustconsole_protocol::NegotiatedAv1Configuration,
) -> SelectedAv1Configuration {
    let capability = wire_capability(DomainCapability {
        mode: selected.mode,
        maximum_width: selected.width,
        maximum_height: selected.height,
        maximum_frames_per_second: selected.frames_per_second,
    });
    SelectedAv1Configuration {
        audio_transport: None,
        width: selected.width,
        height: selected.height,
        frames_per_second: u32::from(selected.frames_per_second),
        mode: Some(Av1Mode {
            chroma_subsampling: capability.chroma_subsampling,
            bit_depth: capability.bit_depth,
        }),
        maximum_bitrate_bits_per_second: selected.maximum_bitrate_bits_per_second,
    }
}

pub trait VideoRenderer<Frame> {
    type Error;

    fn present(&mut self, frame: VideoFrame<Frame>) -> Result<(), Self::Error>;

    fn reconfigure(&mut self, format: VideoFormat) -> Result<(), Self::Error>;
}

pub trait AudioOutput {
    type Error;

    fn play(&mut self, samples: AudioSamples) -> Result<(), Self::Error>;

    fn reset(&mut self) -> Result<(), Self::Error>;
}

pub trait LocalInputSource {
    type Error;

    fn next_event(&mut self) -> Result<Option<InputEvent>, Self::Error>;

    fn focused(&self) -> bool;
}

pub trait SecretStore {
    type Error;

    fn load(&self, host_identity: &str) -> Result<Option<Vec<u8>>, Self::Error>;

    fn store(&mut self, host_identity: &str, secret: &[u8]) -> Result<(), Self::Error>;

    fn remove(&mut self, host_identity: &str) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_lanes_share_the_approved_global_limit() {
        assert_eq!(MAX_CONCURRENT_LAN_PROBES, 24);
        assert_eq!(MAX_CONCURRENT_ROUTED_PROBES, 8);
        assert_eq!(
            MAX_CONCURRENT_LAN_PROBES + MAX_CONCURRENT_ROUTED_PROBES,
            MAX_CONCURRENT_DISCOVERY_PROBES
        );
    }

    #[test]
    fn empty_discovery_reports_a_complete_zero_total() {
        let mut updates = Vec::new();
        assert!(
            discover_candidates_with_progress(Vec::new(), |value| updates.push(value))
                .unwrap()
                .is_empty()
        );
        assert_eq!(updates, vec![DiscoveryProgress::default()]);
    }

    #[test]
    fn discovery_progress_is_monotone_through_a_timeout() {
        let mut updates = Vec::new();
        let candidates = vec![rustconsole_discovery::HostCandidate {
            source: rustconsole_discovery::DiscoverySource::Lan,
            display_name: None,
            addresses: vec!["127.0.0.1:9".parse().unwrap()],
        }];
        assert!(
            discover_candidates_with_updates(candidates, |value| updates.push(value))
                .unwrap()
                .is_empty()
        );
        assert_eq!(updates.len(), 2);
        assert_eq!(
            updates,
            vec![
                DiscoveryUpdate::Progress(DiscoveryProgress {
                    lan_total: 1,
                    ..DiscoveryProgress::default()
                }),
                DiscoveryUpdate::Progress(DiscoveryProgress {
                    lan_completed: 1,
                    lan_total: 1,
                    ..DiscoveryProgress::default()
                }),
            ]
        );
    }

    #[test]
    fn successful_probe_reports_endpoint_before_completion() {
        let endpoint = DiscoveredEndpoint {
            source: rustconsole_discovery::DiscoverySource::Lan,
            source_name: None,
            address: "192.0.2.10:47999".parse().unwrap(),
            host_identity: HostIdentity::from_bytes(&[7; 32]).unwrap(),
            display_name: Some("host".to_owned()),
            operating_system: HostOperatingSystem::Windows,
        };
        let mut progress = DiscoveryProgress {
            lan_total: 1,
            ..DiscoveryProgress::default()
        };
        let mut discovered = Vec::new();
        let mut updates = Vec::new();

        record_discovery_probe(
            CompletedDiscoveryProbe {
                source: rustconsole_discovery::DiscoverySource::Lan,
                endpoint: Some(endpoint.clone()),
            },
            &mut progress,
            &mut discovered,
            &mut |update| updates.push(update),
        );

        assert_eq!(discovered, vec![endpoint.clone()]);
        assert_eq!(
            updates,
            vec![
                DiscoveryUpdate::Endpoint(endpoint),
                DiscoveryUpdate::Progress(DiscoveryProgress {
                    lan_completed: 1,
                    lan_total: 1,
                    ..DiscoveryProgress::default()
                }),
            ]
        );
    }
    use rustconsole_media::{AudioFormat, MediaTimestampMicros};
    use rustconsole_session::{SessionLifecycle, SessionPhase};
    use std::collections::{BTreeMap, VecDeque};
    use std::convert::Infallible;

    #[derive(Default)]
    struct MockRenderer {
        presented_sequences: Vec<u64>,
        configured_format: Option<VideoFormat>,
    }

    impl VideoRenderer<Vec<u8>> for MockRenderer {
        type Error = Infallible;

        fn present(&mut self, frame: VideoFrame<Vec<u8>>) -> Result<(), Self::Error> {
            self.presented_sequences.push(frame.sequence);
            Ok(())
        }

        fn reconfigure(&mut self, format: VideoFormat) -> Result<(), Self::Error> {
            self.configured_format = Some(format);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockAudioOutput {
        played_samples: usize,
    }

    impl AudioOutput for MockAudioOutput {
        type Error = Infallible;

        fn play(&mut self, samples: AudioSamples) -> Result<(), Self::Error> {
            self.played_samples += samples.interleaved.len();
            Ok(())
        }

        fn reset(&mut self) -> Result<(), Self::Error> {
            self.played_samples = 0;
            Ok(())
        }
    }

    struct MockInputSource {
        focused: bool,
        events: VecDeque<InputEvent>,
    }

    impl LocalInputSource for MockInputSource {
        type Error = Infallible;

        fn next_event(&mut self) -> Result<Option<InputEvent>, Self::Error> {
            Ok(self.events.pop_front())
        }

        fn focused(&self) -> bool {
            self.focused
        }
    }

    #[derive(Default)]
    struct MockSecretStore {
        secrets: BTreeMap<String, Vec<u8>>,
    }

    impl SecretStore for MockSecretStore {
        type Error = Infallible;

        fn load(&self, host_identity: &str) -> Result<Option<Vec<u8>>, Self::Error> {
            Ok(self.secrets.get(host_identity).cloned())
        }

        fn store(&mut self, host_identity: &str, secret: &[u8]) -> Result<(), Self::Error> {
            self.secrets
                .insert(host_identity.to_owned(), secret.to_vec());
            Ok(())
        }

        fn remove(&mut self, host_identity: &str) -> Result<(), Self::Error> {
            self.secrets.remove(host_identity);
            Ok(())
        }
    }

    #[test]
    fn vb_cable_wire_states_remain_distinct() {
        use rustconsole_protocol::wire::VbCableStatus;

        assert_eq!(
            vb_cable_availability(VbCableStatus::Unspecified as i32),
            Ok(VbCableAvailability::Unspecified)
        );
        assert_eq!(
            vb_cable_availability(VbCableStatus::Ready as i32),
            Ok(VbCableAvailability::Ready)
        );
        assert_eq!(
            vb_cable_availability(VbCableStatus::Unavailable as i32),
            Ok(VbCableAvailability::Unavailable)
        );
        assert_eq!(
            vb_cable_availability(VbCableStatus::CheckFailed as i32),
            Ok(VbCableAvailability::CheckFailed)
        );
        assert!(vb_cable_availability(99).is_err());
    }

    #[test]
    fn mock_viewer_components_follow_streaming_lifecycle() {
        let format = VideoFormat {
            width: 2560,
            height: 1440,
            frames_per_second: 120,
        };
        let mut lifecycle = SessionLifecycle::connected();
        let mut renderer = MockRenderer::default();
        let mut audio = MockAudioOutput::default();
        let mut input = MockInputSource {
            focused: true,
            events: VecDeque::from([InputEvent::PointerMotion {
                delta_x: 4,
                delta_y: -2,
            }]),
        };
        let mut secrets = MockSecretStore::default();

        secrets.store("host-1", b"test password").unwrap();
        lifecycle.authenticate().unwrap();
        lifecycle.negotiate().unwrap();
        renderer.reconfigure(format).unwrap();
        lifecycle.start_streaming().unwrap();
        renderer
            .present(VideoFrame {
                sequence: 9,
                captured_at: MediaTimestampMicros(1_000),
                format,
                frame: vec![1, 2, 3],
            })
            .unwrap();
        audio
            .play(AudioSamples {
                captured_at: MediaTimestampMicros(1_000),
                format: AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                },
                interleaved: vec![0.0; 8],
            })
            .unwrap();

        assert_eq!(lifecycle.phase(), SessionPhase::Streaming);
        assert_eq!(renderer.configured_format, Some(format));
        assert_eq!(renderer.presented_sequences, [9]);
        assert_eq!(audio.played_samples, 8);
        assert!(input.focused());
        assert_eq!(
            input.next_event().unwrap(),
            Some(InputEvent::PointerMotion {
                delta_x: 4,
                delta_y: -2,
            })
        );
        assert_eq!(
            secrets.load("host-1").unwrap(),
            Some(b"test password".to_vec())
        );
    }
}
