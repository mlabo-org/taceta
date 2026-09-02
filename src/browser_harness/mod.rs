//! Typed, local-only protocol primitives for the Taceta Link browser bridge.
//!
//! This module deliberately treats all page data as opaque JSON.  Browser
//! implementation details (tabs, CDP, cookies, and tokens) never appear in
//! the public protocol types.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::{
    fmt,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;
use uuid::Uuid;

pub const SCHEMA_VERSION: u16 = 1;
pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Ping,
    ExtensionReady,
    PollJob,
    JobResult,
    Cancel,
    CancelAck,
    Health,
    VersionFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Request,
    Response,
    Event,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationState {
    NotPerformed,
    Pending,
    Performed,
    PerformedOrUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Created,
    Running,
    ConfirmationRequired,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    pub schema_version: u16,
    pub product_version: String,
    pub protocol_version: u16,
    pub message_type: MessageType,
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub operation: Operation,
    pub payload: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_state: Option<MutationState>,
}

impl<T> Envelope<T> {
    pub fn new(
        product_version: impl Into<String>,
        session_id: Uuid,
        operation: Operation,
        payload: T,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            product_version: product_version.into(),
            protocol_version: PROTOCOL_VERSION,
            message_type: MessageType::Request,
            request_id: Uuid::new_v4(),
            session_id,
            operation,
            payload,
            mutation_state: None,
        }
    }
    pub fn response_for<U: Serialize>(
        request: &Envelope<T>,
        operation: Operation,
        payload: U,
    ) -> Envelope<U> {
        Envelope {
            schema_version: SCHEMA_VERSION,
            product_version: request.product_version.clone(),
            protocol_version: PROTOCOL_VERSION,
            message_type: MessageType::Response,
            request_id: request.request_id,
            session_id: request.session_id,
            operation,
            payload,
            mutation_state: None,
        }
    }
    pub fn with_mutation_state(mut self, state: MutationState) -> Self {
        self.mutation_state = Some(state);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponsePayload {
    pub accepted: bool,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPayload {
    pub state: LifecycleState,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lifecycle {
    pub state: LifecycleState,
    pub mutation_state: MutationState,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("frame exceeds maximum size ({0} bytes)")]
    Oversize(usize),
    #[error("truncated frame: expected {expected} bytes, received {received}")]
    Truncated { expected: usize, received: usize },
    #[error("invalid JSON frame: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("unsupported schema version {0}")]
    SchemaVersion(u16),
    #[error("unsupported protocol version {0}")]
    ProtocolVersion(u16),
    #[error("unexpected product version {0}")]
    ProductVersion(String),
    #[error("request/session correlation mismatch")]
    CorrelationMismatch,
    #[error("invalid lifecycle transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: LifecycleState,
        to: LifecycleState,
    },
    #[error("socket path must be under Application Support/Taceta: {0}")]
    InvalidSocketPath(PathBuf),
    #[error("socket endpoint is not user-only (mode {0:o})")]
    InsecureSocketMode(u32),
    #[error("socket parent directory is not user-only (mode {0:o})")]
    InsecureDirectoryMode(u32),
    #[error("socket endpoint does not exist: {0}")]
    MissingSocket(PathBuf),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<(), ProtocolError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::Oversize(bytes.len()));
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, ProtocolError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            ProtocolError::Truncated {
                expected: 4,
                received: 0,
            }
        } else {
            e.into()
        }
    })?;
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(ProtocolError::Oversize(length));
    }
    let mut bytes = vec![0u8; length];
    reader.read_exact(&mut bytes).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            ProtocolError::Truncated {
                expected: length,
                received: 0,
            }
        } else {
            e.into()
        }
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn validate_envelope<T>(
    envelope: &Envelope<T>,
    expected_product_version: &str,
) -> Result<(), ProtocolError> {
    if envelope.schema_version != SCHEMA_VERSION {
        return Err(ProtocolError::SchemaVersion(envelope.schema_version));
    }
    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err(ProtocolError::ProtocolVersion(envelope.protocol_version));
    }
    if envelope.product_version != expected_product_version {
        return Err(ProtocolError::ProductVersion(
            envelope.product_version.clone(),
        ));
    }
    if envelope.request_id == Uuid::nil() || envelope.session_id == Uuid::nil() {
        return Err(ProtocolError::CorrelationMismatch);
    }
    Ok(())
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            state: LifecycleState::Created,
            mutation_state: MutationState::NotPerformed,
        }
    }
    pub fn transition(&mut self, next: LifecycleState) -> Result<(), ProtocolError> {
        let valid = matches!(
            (&self.state, &next),
            (LifecycleState::Created, LifecycleState::Running)
                | (
                    LifecycleState::Running,
                    LifecycleState::ConfirmationRequired
                        | LifecycleState::Cancelling
                        | LifecycleState::Completed
                        | LifecycleState::Failed
                )
                | (
                    LifecycleState::ConfirmationRequired,
                    LifecycleState::Running
                        | LifecycleState::Cancelling
                        | LifecycleState::Cancelled
                )
                | (
                    LifecycleState::Cancelling,
                    LifecycleState::Cancelled | LifecycleState::Completed | LifecycleState::Failed
                )
        );
        if !valid {
            return Err(ProtocolError::InvalidTransition {
                from: self.state.clone(),
                to: next,
            });
        }
        self.state = next;
        Ok(())
    }
    pub fn mark_mutation(&mut self, state: MutationState) {
        self.mutation_state = state;
    }
}

pub fn default_socket_path() -> Result<PathBuf, ProtocolError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| ProtocolError::InvalidSocketPath(PathBuf::from("$HOME")))?;
    Ok(PathBuf::from(home).join("Library/Application Support/Taceta/taceta-link.sock"))
}

pub fn validate_socket_path(path: &Path) -> Result<(), ProtocolError> {
    let path = path.to_path_buf();
    let home =
        std::env::var_os("HOME").ok_or_else(|| ProtocolError::InvalidSocketPath(path.clone()))?;
    let expected_root = PathBuf::from(home).join("Library/Application Support/Taceta");
    if !path.starts_with(&expected_root) || path.parent() != Some(expected_root.as_path()) {
        return Err(ProtocolError::InvalidSocketPath(path));
    }
    let parent = std::fs::metadata(&expected_root)
        .map_err(|_| ProtocolError::MissingSocket(expected_root.clone()))?;
    let parent_mode = std::os::unix::fs::MetadataExt::mode(&parent) & 0o777;
    if parent_mode != 0o700 {
        return Err(ProtocolError::InsecureDirectoryMode(parent_mode));
    }
    let endpoint =
        std::fs::symlink_metadata(&path).map_err(|_| ProtocolError::MissingSocket(path.clone()))?;
    let mode = std::os::unix::fs::MetadataExt::mode(&endpoint) & 0o777;
    if mode != 0o600 {
        return Err(ProtocolError::InsecureSocketMode(mode));
    }
    Ok(())
}

#[cfg(unix)]
pub struct ConnectionManager {
    stream: std::os::unix::net::UnixStream,
    pub session_id: Uuid,
    product_version: String,
    pending_request_id: Option<Uuid>,
}

/// App-owned endpoint for accepting connections from the Native Messaging host.
/// The app remains the only component that binds the user socket.
#[cfg(unix)]
pub struct SocketServer {
    listener: std::os::unix::net::UnixListener,
    path: PathBuf,
}

#[cfg(unix)]
impl SocketServer {
    pub fn bind(path: &Path) -> Result<Self, ProtocolError> {
        let path = path.to_path_buf();
        let home = std::env::var_os("HOME")
            .ok_or_else(|| ProtocolError::InvalidSocketPath(path.clone()))?;
        let root = PathBuf::from(home).join("Library/Application Support/Taceta");
        if path.parent() != Some(root.as_path()) {
            return Err(ProtocolError::InvalidSocketPath(path));
        }
        std::fs::create_dir_all(&root)?;
        std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        if path.exists() {
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_socket() {
                return Err(ProtocolError::InvalidSocketPath(path));
            }
            std::fs::remove_file(&path)?;
        }
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        Ok(Self { listener, path })
    }

    pub fn accept(&self) -> Result<std::os::unix::net::UnixStream, ProtocolError> {
        Ok(self.listener.accept()?.0)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for SocketServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
impl ConnectionManager {
    pub fn connect(
        path: &Path,
        product_version: impl Into<String>,
        session_id: Uuid,
    ) -> Result<Self, ProtocolError> {
        validate_socket_path(path)?;
        Ok(Self {
            stream: std::os::unix::net::UnixStream::connect(path)?,
            session_id,
            product_version: product_version.into(),
            pending_request_id: None,
        })
    }
    pub fn send<T: Serialize>(&mut self, request: &Envelope<T>) -> Result<(), ProtocolError> {
        if request.session_id != self.session_id {
            return Err(ProtocolError::CorrelationMismatch);
        }
        write_frame(&mut self.stream, request)?;
        self.pending_request_id = Some(request.request_id);
        Ok(())
    }
    pub fn receive<T: DeserializeOwned>(&mut self) -> Result<Envelope<T>, ProtocolError> {
        let response = read_frame(&mut self.stream)?;
        validate_envelope(&response, &self.product_version)?;
        if response.session_id != self.session_id {
            return Err(ProtocolError::CorrelationMismatch);
        }
        if self.pending_request_id != Some(response.request_id) {
            return Err(ProtocolError::CorrelationMismatch);
        }
        self.pending_request_id = None;
        Ok(response)
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_roundtrip() {
        let input = Envelope::new(
            "0.1.0",
            Uuid::new_v4(),
            Operation::Ping,
            ResponsePayload {
                accepted: true,
                message: None,
            },
        );
        let mut b = Vec::new();
        write_frame(&mut b, &input).unwrap();
        assert_eq!(
            read_frame::<_, Envelope<ResponsePayload>>(&mut b.as_slice()).unwrap(),
            input
        );
    }

    #[test]
    fn shared_protocol_fixture_is_accepted() {
        let fixture: Envelope<serde_json::Value> =
            serde_json::from_str(include_str!("../../protocol/fixture.json")).unwrap();
        validate_envelope(&fixture, "0.1.0").unwrap();
        assert_eq!(fixture.message_type, MessageType::Request);
        assert_eq!(fixture.operation, Operation::PollJob);
    }
    #[test]
    fn poll_response_serializes_to_the_shared_browser_contract() {
        let request: Envelope<serde_json::Value> =
            serde_json::from_str(include_str!("../../protocol/fixture.json")).unwrap();
        let response = Envelope::response_for(
            &request,
            Operation::PollJob,
            serde_json::json!({"job": null}),
        );
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../../protocol/response.json")).unwrap();

        assert_eq!(serde_json::to_value(response).unwrap(), expected);
    }
    #[test]
    fn framing_oversize_and_truncated() {
        let b = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes().to_vec();
        assert!(matches!(
            read_frame::<_, serde_json::Value>(&mut b.as_slice()),
            Err(ProtocolError::Oversize(_))
        ));
        let b = vec![3, 0, 0, 0, b'{'];
        assert!(matches!(
            read_frame::<_, serde_json::Value>(&mut b.as_slice()),
            Err(ProtocolError::Truncated { .. })
        ));
    }
    #[test]
    fn version_mismatch() {
        let mut e = Envelope::new("0.1.0", Uuid::new_v4(), Operation::Ping, ());
        e.protocol_version += 1;
        assert!(matches!(
            validate_envelope(&e, "0.1.0"),
            Err(ProtocolError::ProtocolVersion(2))
        ));
    }
    #[test]
    fn lifecycle_and_mutation() {
        let mut l = Lifecycle::new();
        l.transition(LifecycleState::Running).unwrap();
        l.transition(LifecycleState::ConfirmationRequired).unwrap();
        l.mark_mutation(MutationState::Pending);
        assert!(matches!(
            l.transition(LifecycleState::Created),
            Err(ProtocolError::InvalidTransition { .. })
        ));
        assert_eq!(l.mutation_state, MutationState::Pending);
    }
    #[test]
    fn socket_path_rejects_outside_root() {
        let p = PathBuf::from("/tmp/taceta-link.sock");
        assert!(matches!(
            validate_socket_path(&p),
            Err(ProtocolError::InvalidSocketPath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn socket_roundtrip_preserves_correlation() {
        let path = default_socket_path().unwrap().with_file_name("t.sock");
        let server = SocketServer::bind(&path).unwrap();
        let session = Uuid::new_v4();
        let thread = std::thread::spawn(move || {
            let mut stream = server.accept().unwrap();
            let request: Envelope<serde_json::Value> = read_frame(&mut stream).unwrap();
            let mut response = Envelope::new(
                "0.1.0",
                request.session_id,
                request.operation,
                serde_json::json!({"ok": true}),
            );
            response.request_id = request.request_id;
            write_frame(&mut stream, &response).unwrap();
        });
        let mut manager = ConnectionManager::connect(&path, "0.1.0", session).unwrap();
        let request = Envelope::new(
            "0.1.0",
            session,
            Operation::PollJob,
            serde_json::json!({"page": "untrusted"}),
        );
        manager.send(&request).unwrap();
        let response: Envelope<serde_json::Value> = manager.receive().unwrap();
        assert_eq!(response.request_id, request.request_id);
        assert_eq!(response.session_id, session);
        thread.join().unwrap();
    }
}
