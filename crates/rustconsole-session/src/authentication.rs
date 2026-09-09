//! OPAQUE password enrollment and connection-bound authentication.

use opaque_ke::argon2::Argon2;
use opaque_ke::ciphersuite::CipherSuite;
use opaque_ke::generic_array::typenum::Unsigned;
use opaque_ke::rand::rngs::OsRng;
use opaque_ke::rand::{CryptoRng, RngCore};
use opaque_ke::{
    ClientLogin, ClientLoginFinishParameters, ClientRegistration,
    ClientRegistrationFinishParameters, CredentialFinalization, CredentialFinalizationLen,
    CredentialRequest, CredentialRequestLen, CredentialResponse, CredentialResponseLen,
    Identifiers, ServerLogin, ServerLoginParameters, ServerRegistration, ServerSetup,
};
use sha2::{Digest, Sha256, Sha512};
use std::fmt;
use zeroize::Zeroizing;

pub const DEFAULT_CREDENTIAL_IDENTIFIER: &[u8] = b"rustconsole-default";
pub const TLS_EXPORTER_LABEL: &[u8] = b"EXPORTER-RustConsole-OPAQUE-v1";
pub const TLS_EXPORTER_SIZE: usize = 64;
pub const MAX_CREDENTIAL_IDENTIFIER_SIZE: usize = 255;
pub const HOST_IDENTITY_SIZE: usize = 32;

const RECORD_MAGIC: &[u8; 8] = b"RCOPAQUE";
const RECORD_VERSION: u16 = 2;
const SERVER_IDENTIFIER: &[u8] = b"RustConsole host";
const CLIENT_IDENTIFIER: &[u8] = b"RustConsole viewer";
const SESSION_IDENTITY_LABEL: &[u8] = b"RustConsole authenticated session v1";

pub struct RustConsoleCipherSuite;

impl CipherSuite for RustConsoleCipherSuite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::TripleDh<opaque_ke::Ristretto255, Sha512>;
    type Ksf = Argon2<'static>;
}

type Setup = ServerSetup<RustConsoleCipherSuite>;
type PasswordFile = ServerRegistration<RustConsoleCipherSuite>;

#[derive(Clone)]
pub struct OpaqueServerRecord {
    setup: Setup,
    password_file: PasswordFile,
    credential_identifier: Vec<u8>,
    host_identity: HostIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostIdentity([u8; HOST_IDENTITY_SIZE]);

impl HostIdentity {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AuthenticationError> {
        bytes
            .try_into()
            .map(Self)
            .map_err(|_| AuthenticationError::InvalidHostIdentity)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; HOST_IDENTITY_SIZE] {
        &self.0
    }
}

impl OpaqueServerRecord {
    pub fn enroll(password: Vec<u8>) -> Result<Self, AuthenticationError> {
        let mut rng = OsRng;
        Self::enroll_with_rng(None, password, &mut rng)
    }

    pub fn replace_password(&self, password: Vec<u8>) -> Result<Self, AuthenticationError> {
        let mut rng = OsRng;
        Self::enroll_with_rng(Some(self), password, &mut rng)
    }

    fn enroll_with_rng<R: CryptoRng + RngCore>(
        previous: Option<&Self>,
        password: Vec<u8>,
        rng: &mut R,
    ) -> Result<Self, AuthenticationError> {
        if password.is_empty() {
            return Err(AuthenticationError::EmptyPassword);
        }
        let password = Zeroizing::new(password);
        let setup = previous
            .map(|record| record.setup.clone())
            .unwrap_or_else(|| Setup::new(rng));
        let credential_identifier = previous
            .map(|record| record.credential_identifier.clone())
            .unwrap_or_else(|| DEFAULT_CREDENTIAL_IDENTIFIER.to_vec());
        let host_identity = previous.map_or_else(
            || {
                let mut identity = [0; HOST_IDENTITY_SIZE];
                rng.fill_bytes(&mut identity);
                HostIdentity(identity)
            },
            |record| record.host_identity,
        );
        let client_start = ClientRegistration::<RustConsoleCipherSuite>::start(rng, &password)
            .map_err(AuthenticationError::opaque)?;
        let server_start = ServerRegistration::<RustConsoleCipherSuite>::start(
            &setup,
            client_start.message,
            &credential_identifier,
        )
        .map_err(AuthenticationError::opaque)?;
        let ksf = Argon2::default();
        let client_finish = client_start
            .state
            .finish(
                rng,
                &password,
                server_start.message,
                ClientRegistrationFinishParameters::new(identifiers(), Some(&ksf)),
            )
            .map_err(AuthenticationError::opaque)?;
        let password_file = ServerRegistration::finish(client_finish.message);

        Ok(Self {
            setup,
            password_file,
            credential_identifier,
            host_identity,
        })
    }

    #[must_use]
    pub const fn host_identity(&self) -> HostIdentity {
        self.host_identity
    }

    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let setup = self.setup.serialize();
        let password_file = self.password_file.serialize();
        let mut output = Vec::with_capacity(
            RECORD_MAGIC.len()
                + size_of::<u16>() * 4
                + HOST_IDENTITY_SIZE
                + self.credential_identifier.len()
                + setup.len()
                + password_file.len(),
        );
        output.extend_from_slice(RECORD_MAGIC);
        output.extend_from_slice(&RECORD_VERSION.to_be_bytes());
        output.extend_from_slice(&(self.credential_identifier.len() as u16).to_be_bytes());
        output.extend_from_slice(&(setup.len() as u16).to_be_bytes());
        output.extend_from_slice(&(password_file.len() as u16).to_be_bytes());
        output.extend_from_slice(self.host_identity.as_bytes());
        output.extend_from_slice(&self.credential_identifier);
        output.extend_from_slice(&setup);
        output.extend_from_slice(&password_file);
        output
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self, AuthenticationError> {
        const HEADER_SIZE: usize = 8 + size_of::<u16>() * 4 + HOST_IDENTITY_SIZE;
        if bytes.len() < HEADER_SIZE || &bytes[..RECORD_MAGIC.len()] != RECORD_MAGIC {
            return Err(AuthenticationError::InvalidRecord);
        }
        let version = u16::from_be_bytes([bytes[8], bytes[9]]);
        if version != RECORD_VERSION {
            return Err(AuthenticationError::UnsupportedRecordVersion(version));
        }
        let identifier_len = usize::from(u16::from_be_bytes([bytes[10], bytes[11]]));
        let setup_len = usize::from(u16::from_be_bytes([bytes[12], bytes[13]]));
        let password_file_len = usize::from(u16::from_be_bytes([bytes[14], bytes[15]]));
        if identifier_len == 0 || identifier_len > MAX_CREDENTIAL_IDENTIFIER_SIZE {
            return Err(AuthenticationError::InvalidRecord);
        }
        let expected = HEADER_SIZE
            .checked_add(identifier_len)
            .and_then(|size| size.checked_add(setup_len))
            .and_then(|size| size.checked_add(password_file_len))
            .ok_or(AuthenticationError::InvalidRecord)?;
        if bytes.len() != expected {
            return Err(AuthenticationError::InvalidRecord);
        }
        let host_identity = HostIdentity::from_bytes(&bytes[16..HEADER_SIZE])?;
        let identifier_end = HEADER_SIZE + identifier_len;
        let setup_end = identifier_end + setup_len;
        let credential_identifier = bytes[HEADER_SIZE..identifier_end].to_vec();
        let setup = Setup::deserialize(&bytes[identifier_end..setup_end])
            .map_err(AuthenticationError::opaque)?;
        let password_file =
            PasswordFile::deserialize(&bytes[setup_end..]).map_err(AuthenticationError::opaque)?;

        Ok(Self {
            setup,
            password_file,
            credential_identifier,
            host_identity,
        })
    }
}

pub struct ClientAuthentication {
    state: ClientLogin<RustConsoleCipherSuite>,
    password: Zeroizing<Vec<u8>>,
    credential_identifier: Vec<u8>,
}

pub struct ClientAuthenticationStart {
    pub state: ClientAuthentication,
    pub credential_identifier: Vec<u8>,
    pub message: Vec<u8>,
}

pub struct ClientAuthenticationFinish {
    pub message: Vec<u8>,
    pub session_identity: Option<SessionIdentity>,
}

pub struct ServerAuthentication {
    state: ServerLogin<RustConsoleCipherSuite>,
    binding: [u8; TLS_EXPORTER_SIZE],
}

pub struct ServerAuthenticationStart {
    pub state: ServerAuthentication,
    pub message: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionIdentity([u8; 32]);

impl SessionIdentity {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

pub fn start_client_authentication(
    password: Vec<u8>,
    credential_identifier: Vec<u8>,
) -> Result<ClientAuthenticationStart, AuthenticationError> {
    let mut rng = OsRng;
    start_client_authentication_with_rng(password, credential_identifier, &mut rng)
}

fn start_client_authentication_with_rng<R: CryptoRng + RngCore>(
    password: Vec<u8>,
    credential_identifier: Vec<u8>,
    rng: &mut R,
) -> Result<ClientAuthenticationStart, AuthenticationError> {
    if password.is_empty() {
        return Err(AuthenticationError::EmptyPassword);
    }
    validate_credential_identifier(&credential_identifier)?;
    let password = Zeroizing::new(password);
    let start = ClientLogin::<RustConsoleCipherSuite>::start(rng, &password)
        .map_err(AuthenticationError::opaque)?;
    Ok(ClientAuthenticationStart {
        message: start.message.serialize().to_vec(),
        credential_identifier: credential_identifier.clone(),
        state: ClientAuthentication {
            state: start.state,
            password,
            credential_identifier,
        },
    })
}

impl ClientAuthentication {
    pub fn finish(
        self,
        response: &[u8],
        binding: [u8; TLS_EXPORTER_SIZE],
    ) -> Result<ClientAuthenticationFinish, AuthenticationError> {
        let mut rng = OsRng;
        self.finish_with_rng(response, binding, &mut rng)
    }

    fn finish_with_rng<R: CryptoRng + RngCore>(
        self,
        response: &[u8],
        binding: [u8; TLS_EXPORTER_SIZE],
        rng: &mut R,
    ) -> Result<ClientAuthenticationFinish, AuthenticationError> {
        require_exact_size::<CredentialResponseLen<RustConsoleCipherSuite>>(response)?;
        let response =
            CredentialResponse::deserialize(response).map_err(AuthenticationError::opaque)?;
        let ksf = Argon2::default();
        match self.state.finish(
            rng,
            &self.password,
            response,
            ClientLoginFinishParameters::new(Some(&binding), identifiers(), Some(&ksf)),
        ) {
            Ok(finish) => Ok(ClientAuthenticationFinish {
                message: finish.message.serialize().to_vec(),
                session_identity: Some(derive_session_identity(&finish.session_key, &binding)),
            }),
            Err(_) => {
                let mut message =
                    vec![0; CredentialFinalizationLen::<RustConsoleCipherSuite>::USIZE];
                rng.fill_bytes(&mut message);
                Ok(ClientAuthenticationFinish {
                    message,
                    session_identity: None,
                })
            }
        }
    }

    #[must_use]
    pub fn credential_identifier(&self) -> &[u8] {
        &self.credential_identifier
    }
}

pub fn start_server_authentication(
    record: &OpaqueServerRecord,
    credential_identifier: &[u8],
    request: &[u8],
    binding: [u8; TLS_EXPORTER_SIZE],
) -> Result<ServerAuthenticationStart, AuthenticationError> {
    let mut rng = OsRng;
    start_server_authentication_with_rng(record, credential_identifier, request, binding, &mut rng)
}

fn start_server_authentication_with_rng<R: CryptoRng + RngCore>(
    record: &OpaqueServerRecord,
    credential_identifier: &[u8],
    request: &[u8],
    binding: [u8; TLS_EXPORTER_SIZE],
    rng: &mut R,
) -> Result<ServerAuthenticationStart, AuthenticationError> {
    validate_credential_identifier(credential_identifier)?;
    require_exact_size::<CredentialRequestLen<RustConsoleCipherSuite>>(request)?;
    let request = CredentialRequest::deserialize(request).map_err(AuthenticationError::opaque)?;
    let password_file = (credential_identifier == record.credential_identifier)
        .then(|| record.password_file.clone());
    let start = ServerLogin::start(
        rng,
        &record.setup,
        password_file,
        request,
        credential_identifier,
        ServerLoginParameters {
            context: Some(&binding),
            identifiers: identifiers(),
        },
    )
    .map_err(AuthenticationError::opaque)?;
    Ok(ServerAuthenticationStart {
        message: start.message.serialize().to_vec(),
        state: ServerAuthentication {
            state: start.state,
            binding,
        },
    })
}

impl ServerAuthentication {
    pub fn finish(self, finalization: &[u8]) -> Result<SessionIdentity, AuthenticationError> {
        require_exact_size::<CredentialFinalizationLen<RustConsoleCipherSuite>>(finalization)?;
        let finalization = CredentialFinalization::deserialize(finalization)
            .map_err(AuthenticationError::opaque)?;
        let finish = self
            .state
            .finish(
                finalization,
                ServerLoginParameters {
                    context: Some(&self.binding),
                    identifiers: identifiers(),
                },
            )
            .map_err(|_| AuthenticationError::AuthenticationFailed)?;
        Ok(derive_session_identity(&finish.session_key, &self.binding))
    }
}

fn identifiers() -> Identifiers<'static> {
    Identifiers {
        client: Some(CLIENT_IDENTIFIER),
        server: Some(SERVER_IDENTIFIER),
    }
}

fn validate_credential_identifier(identifier: &[u8]) -> Result<(), AuthenticationError> {
    if identifier.is_empty() || identifier.len() > MAX_CREDENTIAL_IDENTIFIER_SIZE {
        return Err(AuthenticationError::InvalidCredentialIdentifier);
    }
    Ok(())
}

fn require_exact_size<L: Unsigned>(bytes: &[u8]) -> Result<(), AuthenticationError> {
    if bytes.len() != L::USIZE {
        return Err(AuthenticationError::InvalidMessage);
    }
    Ok(())
}

fn derive_session_identity(session_key: &[u8], binding: &[u8]) -> SessionIdentity {
    let mut hash = Sha256::new();
    hash.update(SESSION_IDENTITY_LABEL);
    hash.update(session_key);
    hash.update(binding);
    SessionIdentity(hash.finalize().into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthenticationError {
    EmptyPassword,
    InvalidCredentialIdentifier,
    InvalidHostIdentity,
    InvalidMessage,
    InvalidRecord,
    UnsupportedRecordVersion(u16),
    AuthenticationFailed,
    OpaqueProtocol,
}

impl AuthenticationError {
    fn opaque<E>(_error: E) -> Self {
        Self::OpaqueProtocol
    }
}

impl fmt::Display for AuthenticationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPassword => formatter.write_str("password must not be empty"),
            Self::InvalidCredentialIdentifier => {
                formatter.write_str("credential identifier is invalid")
            }
            Self::InvalidHostIdentity => formatter.write_str("host identity is invalid"),
            Self::InvalidMessage => formatter.write_str("authentication message is invalid"),
            Self::InvalidRecord => formatter.write_str("OPAQUE server record is invalid"),
            Self::UnsupportedRecordVersion(version) => {
                write!(
                    formatter,
                    "OPAQUE server record version {version} is unsupported"
                )
            }
            Self::AuthenticationFailed => formatter.write_str("authentication failed"),
            Self::OpaqueProtocol => formatter.write_str("OPAQUE protocol operation failed"),
        }
    }
}

impl std::error::Error for AuthenticationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use opaque_ke::rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    fn record(password: &[u8]) -> OpaqueServerRecord {
        let mut rng = ChaCha20Rng::from_seed([7; 32]);
        OpaqueServerRecord::enroll_with_rng(None, password.to_vec(), &mut rng).unwrap()
    }

    fn login(
        record: &OpaqueServerRecord,
        password: &[u8],
        client_binding: [u8; TLS_EXPORTER_SIZE],
        server_binding: [u8; TLS_EXPORTER_SIZE],
        credential_identifier: &[u8],
    ) -> (
        Option<SessionIdentity>,
        Result<SessionIdentity, AuthenticationError>,
    ) {
        let mut client_rng = ChaCha20Rng::from_seed([11; 32]);
        let client = start_client_authentication_with_rng(
            password.to_vec(),
            credential_identifier.to_vec(),
            &mut client_rng,
        )
        .unwrap();
        let mut server_rng = ChaCha20Rng::from_seed([13; 32]);
        let server = start_server_authentication_with_rng(
            record,
            &client.credential_identifier,
            &client.message,
            server_binding,
            &mut server_rng,
        )
        .unwrap();
        let client = client
            .state
            .finish_with_rng(&server.message, client_binding, &mut client_rng)
            .unwrap();
        let server = server.state.finish(&client.message);
        (client.session_identity, server)
    }

    #[test]
    fn correct_password_and_connection_binding_produce_one_identity() {
        let record = record(b"correct horse battery staple");
        let binding = [0x42; TLS_EXPORTER_SIZE];
        let (client, server) = login(
            &record,
            b"correct horse battery staple",
            binding,
            binding,
            DEFAULT_CREDENTIAL_IDENTIFIER,
        );

        assert_eq!(client.unwrap(), server.unwrap());
    }

    #[test]
    fn wrong_password_completes_the_message_flow_but_authentication_fails() {
        let record = record(b"right password");
        let binding = [3; TLS_EXPORTER_SIZE];
        let (client, server) = login(
            &record,
            b"wrong password",
            binding,
            binding,
            DEFAULT_CREDENTIAL_IDENTIFIER,
        );

        assert_eq!(client, None);
        assert_eq!(server, Err(AuthenticationError::AuthenticationFailed));
    }

    #[test]
    fn active_interception_with_different_tls_exporters_fails() {
        let record = record(b"password");
        let (client, server) = login(
            &record,
            b"password",
            [1; TLS_EXPORTER_SIZE],
            [2; TLS_EXPORTER_SIZE],
            DEFAULT_CREDENTIAL_IDENTIFIER,
        );

        assert_eq!(client, None);
        assert_eq!(server, Err(AuthenticationError::AuthenticationFailed));
    }

    #[test]
    fn unknown_credential_uses_a_full_dummy_exchange() {
        let record = record(b"password");
        let binding = [4; TLS_EXPORTER_SIZE];
        let (client, server) = login(
            &record,
            b"password",
            binding,
            binding,
            b"unknown-credential",
        );

        assert_eq!(client, None);
        assert_eq!(server, Err(AuthenticationError::AuthenticationFailed));
    }

    #[test]
    fn password_replacement_preserves_setup_and_invalidates_old_password() {
        let record = record(b"old password");
        let old_setup = record.setup.serialize();
        let host_identity = record.host_identity();
        let mut rng = ChaCha20Rng::from_seed([17; 32]);
        let replaced =
            OpaqueServerRecord::enroll_with_rng(Some(&record), b"new password".to_vec(), &mut rng)
                .unwrap();

        assert_eq!(replaced.setup.serialize(), old_setup);
        assert_eq!(replaced.host_identity(), host_identity);
        assert!(
            login(
                &replaced,
                b"new password",
                [5; TLS_EXPORTER_SIZE],
                [5; TLS_EXPORTER_SIZE],
                DEFAULT_CREDENTIAL_IDENTIFIER,
            )
            .1
            .is_ok()
        );
        assert_eq!(
            login(
                &replaced,
                b"old password",
                [5; TLS_EXPORTER_SIZE],
                [5; TLS_EXPORTER_SIZE],
                DEFAULT_CREDENTIAL_IDENTIFIER,
            )
            .1,
            Err(AuthenticationError::AuthenticationFailed)
        );
    }

    #[test]
    fn serialized_record_round_trips_and_rejects_trailing_data() {
        let record = record(b"password");
        let bytes = record.serialize();
        let restored = OpaqueServerRecord::deserialize(&bytes).unwrap();
        assert_eq!(restored.host_identity(), record.host_identity());
        assert!(
            login(
                &restored,
                b"password",
                [9; TLS_EXPORTER_SIZE],
                [9; TLS_EXPORTER_SIZE],
                DEFAULT_CREDENTIAL_IDENTIFIER,
            )
            .1
            .is_ok()
        );

        let mut invalid = bytes;
        invalid.push(0);
        assert!(matches!(
            OpaqueServerRecord::deserialize(&invalid),
            Err(AuthenticationError::InvalidRecord)
        ));
    }

    #[test]
    fn opaque_transcript_matches_fixed_vector() {
        let record = record(b"vector password");
        let binding = [0x5a; TLS_EXPORTER_SIZE];
        let mut client_rng = ChaCha20Rng::from_seed([23; 32]);
        let client = start_client_authentication_with_rng(
            b"vector password".to_vec(),
            DEFAULT_CREDENTIAL_IDENTIFIER.to_vec(),
            &mut client_rng,
        )
        .unwrap();
        let mut server_rng = ChaCha20Rng::from_seed([29; 32]);
        let server = start_server_authentication_with_rng(
            &record,
            &client.credential_identifier,
            &client.message,
            binding,
            &mut server_rng,
        )
        .unwrap();
        let client_finish = client
            .state
            .finish_with_rng(&server.message, binding, &mut client_rng)
            .unwrap();
        let server_identity = server.state.finish(&client_finish.message).unwrap();
        let client_identity = client_finish.session_identity.unwrap();
        assert_eq!(client_identity, server_identity);

        let mut vector = Sha256::new();
        for part in [
            record.serialize().as_slice(),
            client.message.as_slice(),
            server.message.as_slice(),
            client_finish.message.as_slice(),
            binding.as_slice(),
            client_identity.as_bytes().as_slice(),
        ] {
            vector.update((part.len() as u32).to_be_bytes());
            vector.update(part);
        }
        assert_eq!(
            <[u8; 32]>::from(vector.finalize()),
            [
                0x1b, 0xe4, 0x6b, 0x6a, 0xb7, 0xa9, 0x98, 0x00, 0x12, 0xd6, 0xa8, 0x30, 0x1a, 0x2a,
                0x1f, 0x52, 0xd2, 0xb3, 0xe3, 0x74, 0xb2, 0x76, 0x3c, 0x3d, 0xf7, 0x7f, 0xf7, 0x23,
                0x99, 0xfc, 0xc5, 0x91,
            ]
        );
    }
}
