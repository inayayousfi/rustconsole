//! QUIC setup and OPAQUE authentication bound to the active TLS connection.

use crate::authentication::{
    DEFAULT_CREDENTIAL_IDENTIFIER, HostIdentity, MAX_CREDENTIAL_IDENTIFIER_SIZE,
    OpaqueServerRecord, SessionIdentity, TLS_EXPORTER_LABEL, TLS_EXPORTER_SIZE,
    start_client_authentication, start_server_authentication,
};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Connection, Endpoint, RecvStream, SendStream, ServerConfig};
use rustconsole_protocol::wire::{
    self, AuthenticationResult, Envelope, HostIdentityOffer, HostIdentityRequest,
    OpaqueCredentialFinalization, OpaqueCredentialRequest, OpaqueCredentialResponse, envelope,
};
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ALPN_PROTOCOL: &[u8] = b"rustconsole/1";
pub const AUTHENTICATION_STREAM_LIMIT: usize = wire::MAX_RELIABLE_MESSAGE_SIZE;
pub const MAX_TRACKED_FAILURE_ADDRESSES: usize = 1_024;
pub const MAX_DISCOVERY_DISPLAY_NAME_SIZE: usize = 63;

const INITIAL_FAILURE_DELAY: Duration = Duration::from_millis(250);
const MAX_FAILURE_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct AuthenticatedConnection {
    pub connection: Connection,
    pub session_identity: SessionIdentity,
    pub host_identity: HostIdentity,
    pub display_name: Option<String>,
    pub operating_system: wire::HostOperatingSystem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostMetadata {
    pub display_name: String,
    pub operating_system: wire::HostOperatingSystem,
}

impl HostMetadata {
    pub fn new(
        display_name: String,
        operating_system: wire::HostOperatingSystem,
    ) -> Result<Self, QuicAuthenticationError> {
        validate_display_name(&display_name)?;
        Ok(Self {
            display_name,
            operating_system,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredHost {
    pub host_identity: HostIdentity,
    pub display_name: Option<String>,
    pub operating_system: wire::HostOperatingSystem,
}

pub fn ephemeral_server_config() -> Result<ServerConfig, QuicAuthenticationError> {
    let generated = rcgen::generate_simple_self_signed(vec!["rustconsole.invalid".to_owned()])
        .map_err(QuicAuthenticationError::setup)?;
    let certificate = generated.cert.der().clone();
    let private_key =
        rustls::pki_types::PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der());
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(QuicAuthenticationError::setup)?
    .with_no_client_auth()
    .with_single_cert(vec![certificate], private_key.into())
    .map_err(QuicAuthenticationError::setup)?;
    tls.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
    let crypto = QuicServerConfig::try_from(tls).map_err(QuicAuthenticationError::setup)?;
    let mut config = ServerConfig::with_crypto(Arc::new(crypto));
    configure_shared_transport(&mut config.transport);
    Ok(config)
}

pub fn opaque_client_config() -> Result<ClientConfig, QuicAuthenticationError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(EphemeralCertificateVerifier(Arc::clone(&provider)));
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(QuicAuthenticationError::setup)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
    let crypto = QuicClientConfig::try_from(tls).map_err(QuicAuthenticationError::setup)?;
    let mut config = ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    configure_transport(&mut transport);
    config.transport_config(Arc::new(transport));
    Ok(config)
}

fn configure_shared_transport(transport: &mut Arc<quinn::TransportConfig>) {
    let transport = Arc::get_mut(transport).expect("new QUIC config has one transport owner");
    configure_transport(transport);
}

fn configure_transport(transport: &mut quinn::TransportConfig) {
    // Keep disposable media backlog close to two ordinary network packets.
    transport.datagram_send_buffer_size(2 * 1500);
    transport.max_concurrent_bidi_streams(8_u8.into());
    transport.max_concurrent_uni_streams(8_u8.into());
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(10))
            .expect("ten seconds is a valid QUIC idle timeout"),
    ));
}

pub async fn send_media_datagram(
    connection: &Connection,
    data: bytes::Bytes,
    wait: Duration,
) -> Result<bool, quinn::SendDatagramError> {
    match tokio::time::timeout(wait, connection.send_datagram_wait(data)).await {
        Ok(result) => result.map(|()| true),
        Err(_) => Ok(false),
    }
}

pub async fn connect_and_authenticate(
    endpoint: &Endpoint,
    address: SocketAddr,
    password: Vec<u8>,
) -> Result<AuthenticatedConnection, QuicAuthenticationError> {
    connect_and_authenticate_with(endpoint, address, move |_| Some(password)).await
}

pub async fn connect_and_authenticate_with(
    endpoint: &Endpoint,
    address: SocketAddr,
    password_for: impl FnOnce(HostIdentity) -> Option<Vec<u8>>,
) -> Result<AuthenticatedConnection, QuicAuthenticationError> {
    let connection = endpoint
        .connect(address, "rustconsole.invalid")
        .map_err(QuicAuthenticationError::transport)?
        .await
        .map_err(QuicAuthenticationError::transport)?;
    authenticate_client_with(connection, password_for).await
}

pub async fn discover_host(
    endpoint: &Endpoint,
    address: SocketAddr,
    wait: Duration,
) -> Result<DiscoveredHost, QuicAuthenticationError> {
    tokio::time::timeout(wait, async {
        let connection = endpoint
            .connect(address, "rustconsole.invalid")
            .map_err(QuicAuthenticationError::transport)?
            .await
            .map_err(QuicAuthenticationError::transport)?;
        let (mut send, mut receive) = connection
            .open_bi()
            .await
            .map_err(QuicAuthenticationError::transport)?;
        write_envelope(
            &mut send,
            Envelope {
                body: Some(envelope::Body::HostIdentityRequest(HostIdentityRequest {})),
            },
        )
        .await?;
        let offer = match read_envelope(&mut receive).await?.body {
            Some(envelope::Body::HostIdentityOffer(offer)) => offer,
            _ => return Err(QuicAuthenticationError::InvalidAuthenticationSequence),
        };
        let discovered = parse_host_identity_offer(offer)?;
        connection.close(0_u32.into(), b"discovery complete");
        Ok(discovered)
    })
    .await
    .map_err(|_| QuicAuthenticationError::Timeout)?
}

pub async fn authenticate_client(
    connection: Connection,
    password: Vec<u8>,
) -> Result<AuthenticatedConnection, QuicAuthenticationError> {
    authenticate_client_with(connection, move |_| Some(password)).await
}

pub async fn authenticate_client_with(
    connection: Connection,
    password_for: impl FnOnce(HostIdentity) -> Option<Vec<u8>>,
) -> Result<AuthenticatedConnection, QuicAuthenticationError> {
    let binding = connection_binding(&connection)?;
    let (mut send, mut receive) = connection
        .open_bi()
        .await
        .map_err(QuicAuthenticationError::transport)?;
    write_envelope(
        &mut send,
        Envelope {
            body: Some(envelope::Body::HostIdentityRequest(HostIdentityRequest {})),
        },
    )
    .await?;
    let offered_host = match read_envelope(&mut receive).await?.body {
        Some(envelope::Body::HostIdentityOffer(offer)) => parse_host_identity_offer(offer)?,
        _ => return Err(QuicAuthenticationError::InvalidAuthenticationSequence),
    };
    let password = password_for(offered_host.host_identity)
        .ok_or(QuicAuthenticationError::CredentialUnavailable)?;
    let start = start_client_authentication(password, DEFAULT_CREDENTIAL_IDENTIFIER.to_vec())
        .map_err(QuicAuthenticationError::protocol)?;
    write_envelope(
        &mut send,
        Envelope {
            body: Some(envelope::Body::OpaqueCredentialRequest(
                OpaqueCredentialRequest {
                    credential_identifier: start.credential_identifier,
                    message: start.message,
                },
            )),
        },
    )
    .await?;
    let response = match read_envelope(&mut receive).await?.body {
        Some(envelope::Body::OpaqueCredentialResponse(response)) => response,
        _ => return Err(QuicAuthenticationError::InvalidAuthenticationSequence),
    };
    let finish = start
        .state
        .finish(&response.message, binding)
        .map_err(QuicAuthenticationError::protocol)?;
    write_envelope(
        &mut send,
        Envelope {
            body: Some(envelope::Body::OpaqueCredentialFinalization(
                OpaqueCredentialFinalization {
                    message: finish.message,
                },
            )),
        },
    )
    .await?;
    let result = match read_envelope(&mut receive).await?.body {
        Some(envelope::Body::AuthenticationResult(result)) => result,
        _ => return Err(QuicAuthenticationError::InvalidAuthenticationSequence),
    };
    send.finish().map_err(QuicAuthenticationError::transport)?;
    let confirmed_host_identity =
        HostIdentity::from_bytes(&result.host_identity).map_err(QuicAuthenticationError::protocol);
    match (
        result.accepted,
        finish.session_identity,
        confirmed_host_identity,
    ) {
        (true, Some(session_identity), Ok(host_identity))
            if host_identity == offered_host.host_identity =>
        {
            Ok(AuthenticatedConnection {
                connection,
                session_identity,
                host_identity,
                display_name: offered_host.display_name,
                operating_system: offered_host.operating_system,
            })
        }
        _ => Err(QuicAuthenticationError::AuthenticationFailed),
    }
}

pub async fn authenticate_server(
    connection: Connection,
    record: &OpaqueServerRecord,
    limiter: &Mutex<AuthenticationRateLimiter>,
    metadata: &HostMetadata,
) -> Result<AuthenticatedConnection, QuicAuthenticationError> {
    let peer = connection.remote_address().ip();
    let initial_delay = limiter
        .lock()
        .map_err(|_| QuicAuthenticationError::RateLimiterUnavailable)?
        .remaining_delay(peer, Instant::now());
    if !initial_delay.is_zero() {
        tokio::time::sleep(initial_delay).await;
    }

    let binding = connection_binding(&connection)?;
    let (mut send, mut receive) = connection
        .accept_bi()
        .await
        .map_err(QuicAuthenticationError::transport)?;
    match read_envelope(&mut receive).await?.body {
        Some(envelope::Body::HostIdentityRequest(_)) => {}
        _ => return Err(QuicAuthenticationError::InvalidAuthenticationSequence),
    }
    write_envelope(
        &mut send,
        Envelope {
            body: Some(envelope::Body::HostIdentityOffer(HostIdentityOffer {
                host_identity: record.host_identity().as_bytes().to_vec(),
                display_name: metadata.display_name.clone(),
                operating_system: metadata.operating_system as i32,
            })),
        },
    )
    .await?;
    let request = match read_envelope(&mut receive).await?.body {
        Some(envelope::Body::OpaqueCredentialRequest(request))
            if !request.credential_identifier.is_empty()
                && request.credential_identifier.len() <= MAX_CREDENTIAL_IDENTIFIER_SIZE =>
        {
            request
        }
        _ => return Err(QuicAuthenticationError::InvalidAuthenticationSequence),
    };
    let start = start_server_authentication(
        record,
        &request.credential_identifier,
        &request.message,
        binding,
    )
    .map_err(QuicAuthenticationError::protocol)?;
    write_envelope(
        &mut send,
        Envelope {
            body: Some(envelope::Body::OpaqueCredentialResponse(
                OpaqueCredentialResponse {
                    message: start.message,
                },
            )),
        },
    )
    .await?;
    let finalization = match read_envelope(&mut receive).await?.body {
        Some(envelope::Body::OpaqueCredentialFinalization(finalization)) => finalization,
        _ => return Err(QuicAuthenticationError::InvalidAuthenticationSequence),
    };
    let identity = start.state.finish(&finalization.message).ok();
    let accepted = identity.is_some();
    let failure_delay = {
        let mut limiter = limiter
            .lock()
            .map_err(|_| QuicAuthenticationError::RateLimiterUnavailable)?;
        if accepted {
            limiter.record_success(peer);
            Duration::ZERO
        } else {
            limiter.record_failure(peer, Instant::now())
        }
    };
    if !failure_delay.is_zero() {
        tokio::time::sleep(failure_delay).await;
    }
    write_envelope(
        &mut send,
        Envelope {
            body: Some(envelope::Body::AuthenticationResult(AuthenticationResult {
                accepted,
                host_identity: if accepted {
                    record.host_identity().as_bytes().to_vec()
                } else {
                    Vec::new()
                },
            })),
        },
    )
    .await?;
    send.finish().map_err(QuicAuthenticationError::transport)?;
    match identity {
        Some(session_identity) => Ok(AuthenticatedConnection {
            connection,
            session_identity,
            host_identity: record.host_identity(),
            display_name: Some(metadata.display_name.clone()),
            operating_system: metadata.operating_system,
        }),
        None => Err(QuicAuthenticationError::AuthenticationFailed),
    }
}

fn validate_display_name(display_name: &str) -> Result<(), QuicAuthenticationError> {
    if display_name.is_empty()
        || display_name.len() > MAX_DISCOVERY_DISPLAY_NAME_SIZE
        || display_name.chars().any(char::is_control)
    {
        return Err(QuicAuthenticationError::InvalidDiscoveryMetadata);
    }
    Ok(())
}

fn parse_host_identity_offer(
    offer: HostIdentityOffer,
) -> Result<DiscoveredHost, QuicAuthenticationError> {
    let host_identity = HostIdentity::from_bytes(&offer.host_identity)
        .map_err(QuicAuthenticationError::protocol)?;
    let display_name = if offer.display_name.is_empty() {
        None
    } else {
        validate_display_name(&offer.display_name)?;
        Some(offer.display_name)
    };
    let operating_system = wire::HostOperatingSystem::try_from(offer.operating_system)
        .map_err(QuicAuthenticationError::protocol)?;
    Ok(DiscoveredHost {
        host_identity,
        display_name,
        operating_system,
    })
}

fn connection_binding(
    connection: &Connection,
) -> Result<[u8; TLS_EXPORTER_SIZE], QuicAuthenticationError> {
    let mut binding = [0; TLS_EXPORTER_SIZE];
    connection
        .export_keying_material(&mut binding, TLS_EXPORTER_LABEL, &[])
        .map_err(|_| {
            QuicAuthenticationError::Transport("TLS exporter is unavailable".to_owned())
        })?;
    Ok(binding)
}

pub async fn write_envelope(
    send: &mut SendStream,
    envelope: Envelope,
) -> Result<(), QuicAuthenticationError> {
    let frame =
        wire::encode_reliable_frame(&envelope).map_err(QuicAuthenticationError::protocol)?;
    send.write_all(&frame)
        .await
        .map_err(QuicAuthenticationError::transport)
}

pub async fn read_envelope(receive: &mut RecvStream) -> Result<Envelope, QuicAuthenticationError> {
    let mut prefix = [0; wire::RELIABLE_FRAME_PREFIX_SIZE];
    receive
        .read_exact(&mut prefix)
        .await
        .map_err(QuicAuthenticationError::transport)?;
    let size = u32::from_be_bytes(prefix) as usize;
    if size > AUTHENTICATION_STREAM_LIMIT {
        return Err(QuicAuthenticationError::MessageTooLarge(size));
    }
    let mut frame = Vec::with_capacity(prefix.len() + size);
    frame.extend_from_slice(&prefix);
    frame.resize(prefix.len() + size, 0);
    receive
        .read_exact(&mut frame[prefix.len()..])
        .await
        .map_err(QuicAuthenticationError::transport)?;
    wire::decode_reliable_frame(&frame).map_err(QuicAuthenticationError::protocol)
}

#[derive(Debug)]
struct EphemeralCertificateVerifier(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for EphemeralCertificateVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Clone, Copy, Debug)]
struct FailureState {
    failures: u32,
    blocked_until: Instant,
    last_seen: Instant,
}

#[derive(Debug, Default)]
pub struct AuthenticationRateLimiter {
    failures: HashMap<IpAddr, FailureState>,
}

impl AuthenticationRateLimiter {
    #[must_use]
    pub fn remaining_delay(&mut self, address: IpAddr, now: Instant) -> Duration {
        self.failures
            .get_mut(&address)
            .map(|state| {
                state.last_seen = now;
                state.blocked_until.saturating_duration_since(now)
            })
            .unwrap_or_default()
    }

    pub fn record_failure(&mut self, address: IpAddr, now: Instant) -> Duration {
        if !self.failures.contains_key(&address)
            && self.failures.len() >= MAX_TRACKED_FAILURE_ADDRESSES
            && let Some(oldest) = self
                .failures
                .iter()
                .min_by_key(|(_, state)| state.last_seen)
                .map(|(address, _)| *address)
        {
            self.failures.remove(&oldest);
        }
        let state = self.failures.entry(address).or_insert(FailureState {
            failures: 0,
            blocked_until: now,
            last_seen: now,
        });
        state.failures = state.failures.saturating_add(1);
        state.last_seen = now;
        let shift = state.failures.saturating_sub(1).min(4);
        let delay = INITIAL_FAILURE_DELAY
            .checked_mul(1_u32 << shift)
            .unwrap_or(MAX_FAILURE_DELAY)
            .min(MAX_FAILURE_DELAY);
        state.blocked_until = now + delay;
        delay
    }

    pub fn record_success(&mut self, address: IpAddr) {
        self.failures.remove(&address);
    }

    #[must_use]
    pub fn tracked_addresses(&self) -> usize {
        self.failures.len()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuicAuthenticationError {
    Setup(String),
    Transport(String),
    Protocol(String),
    MessageTooLarge(usize),
    InvalidAuthenticationSequence,
    AuthenticationFailed,
    RateLimiterUnavailable,
    CredentialUnavailable,
    Timeout,
    InvalidDiscoveryMetadata,
}

impl QuicAuthenticationError {
    fn setup(error: impl fmt::Display) -> Self {
        Self::Setup(error.to_string())
    }

    fn transport(error: impl fmt::Display) -> Self {
        Self::Transport(error.to_string())
    }

    fn protocol(error: impl fmt::Display) -> Self {
        Self::Protocol(error.to_string())
    }
}

impl fmt::Display for QuicAuthenticationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setup(error) => write!(formatter, "QUIC setup failed: {error}"),
            Self::Transport(error) => write!(formatter, "QUIC transport failed: {error}"),
            Self::Protocol(error) => write!(formatter, "authentication protocol failed: {error}"),
            Self::MessageTooLarge(size) => write!(
                formatter,
                "authentication message is {size} bytes; maximum is {AUTHENTICATION_STREAM_LIMIT}"
            ),
            Self::InvalidAuthenticationSequence => {
                formatter.write_str("authentication message sequence is invalid")
            }
            Self::AuthenticationFailed => formatter.write_str("authentication failed"),
            Self::RateLimiterUnavailable => {
                formatter.write_str("authentication rate limiter is unavailable")
            }
            Self::CredentialUnavailable => {
                formatter.write_str("no password is available for this host identity")
            }
            Self::Timeout => formatter.write_str("Rust Console discovery timed out"),
            Self::InvalidDiscoveryMetadata => {
                formatter.write_str("host discovery metadata is invalid")
            }
        }
    }
}

impl std::error::Error for QuicAuthenticationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn failure_delay_grows_is_bounded_and_success_clears_it() {
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let now = Instant::now();
        let mut limiter = AuthenticationRateLimiter::default();

        assert_eq!(
            limiter.record_failure(address, now),
            Duration::from_millis(250)
        );
        assert_eq!(
            limiter.record_failure(address, now),
            Duration::from_millis(500)
        );
        for _ in 0..10 {
            assert!(limiter.record_failure(address, now) <= MAX_FAILURE_DELAY);
        }
        limiter.record_success(address);
        assert_eq!(limiter.remaining_delay(address, now), Duration::ZERO);
    }

    #[test]
    fn failure_tracker_has_a_hard_address_bound() {
        let now = Instant::now();
        let mut limiter = AuthenticationRateLimiter::default();
        for index in 0..MAX_TRACKED_FAILURE_ADDRESSES + 20 {
            let address = SocketAddrV4::new(
                Ipv4Addr::new(10, (index / 256) as u8, (index % 256) as u8, 1),
                1,
            )
            .ip()
            .to_owned();
            limiter.record_failure(
                IpAddr::V4(address),
                now + Duration::from_millis(index as u64),
            );
        }

        assert_eq!(limiter.tracked_addresses(), MAX_TRACKED_FAILURE_ADDRESSES);
    }

    #[tokio::test]
    async fn loopback_quic_authentication_uses_the_connection_exporter() {
        let record = Arc::new(OpaqueServerRecord::enroll(b"test password".to_vec()).unwrap());
        let limiter = Arc::new(Mutex::new(AuthenticationRateLimiter::default()));
        let metadata = Arc::new(
            HostMetadata::new("test-host".to_owned(), wire::HostOperatingSystem::Windows).unwrap(),
        );
        let server = Endpoint::server(
            ephemeral_server_config().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let address = server.local_addr().unwrap();
        let server_task = tokio::spawn({
            let server = server.clone();
            let record = Arc::clone(&record);
            let limiter = Arc::clone(&limiter);
            let metadata = Arc::clone(&metadata);
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                authenticate_server(connection, &record, &limiter, &metadata).await
            }
        });
        let mut client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(opaque_client_config().unwrap());
        let client = connect_and_authenticate(&client, address, b"test password".to_vec())
            .await
            .unwrap();
        let authenticated_server = server_task.await.unwrap().unwrap();

        assert_eq!(
            client.session_identity,
            authenticated_server.session_identity
        );
    }

    #[tokio::test]
    async fn discovery_reads_metadata_without_recording_an_authentication_failure() {
        let record = Arc::new(OpaqueServerRecord::enroll(b"test password".to_vec()).unwrap());
        let limiter = Arc::new(Mutex::new(AuthenticationRateLimiter::default()));
        let metadata = Arc::new(
            HostMetadata::new("test-host".to_owned(), wire::HostOperatingSystem::Windows).unwrap(),
        );
        let server = Endpoint::server(
            ephemeral_server_config().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let address = server.local_addr().unwrap();
        let server_task = tokio::spawn({
            let server = server.clone();
            let record = Arc::clone(&record);
            let limiter = Arc::clone(&limiter);
            let metadata = Arc::clone(&metadata);
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                authenticate_server(connection, &record, &limiter, &metadata).await
            }
        });
        let mut client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(opaque_client_config().unwrap());

        let discovered = discover_host(&client, address, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(discovered.host_identity, record.host_identity());
        assert_eq!(discovered.display_name.as_deref(), Some("test-host"));
        assert_eq!(
            discovered.operating_system,
            wire::HostOperatingSystem::Windows
        );
        assert!(server_task.await.unwrap().is_err());
        assert_eq!(limiter.lock().unwrap().tracked_addresses(), 0);
    }

    #[test]
    fn discovery_metadata_is_bounded_and_rejects_control_characters() {
        assert!(
            HostMetadata::new(
                "a".repeat(MAX_DISCOVERY_DISPLAY_NAME_SIZE),
                wire::HostOperatingSystem::Windows,
            )
            .is_ok()
        );
        assert!(
            HostMetadata::new(
                "a".repeat(MAX_DISCOVERY_DISPLAY_NAME_SIZE + 1),
                wire::HostOperatingSystem::Windows,
            )
            .is_err()
        );
        assert!(
            HostMetadata::new("bad\nname".to_owned(), wire::HostOperatingSystem::Windows,).is_err()
        );
    }
}
