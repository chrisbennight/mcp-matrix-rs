//! The governed transfer plane: media arrives here instead of in a tool argument.
//!
//! Bytes are **pushed** to this server, never pulled by it. A trusted intermediary asks
//! for an upload authorization, this server mints a single-use ticket and hands back a
//! descriptor pointing at its own ingest route, and the intermediary streams the bytes
//! there. Only then does an ordinary `matrix_submit_asset` call name the staged source.
//!
//! That direction is the whole point. This server dereferences nothing, holds no
//! credential for anyone else's endpoint, and has no destination to get wrong — the
//! confused-deputy pattern `resolve_source` refuses is not merely blocked here, it is
//! absent. What arrives is validated against what was declared before it can be decoded.
//!
//! The plane is off unless an operator configures a public origin. Off means the
//! authorization method answers method-not-found, which is exactly what a file-aware
//! intermediary reads as "no native file transfer", so the default posture costs nothing.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Scheme and authority this server mints for a staged source.
///
/// Self-describing and specific to this server, so `resolve_source` can tell a reference
/// it issued from every other string without consulting configuration. It is not a
/// dereferenceable location and nothing ever treats it as one.
pub const STAGED_PREFIX: &str = "matrix-file://staged/";

/// Bytes of entropy in a ticket identifier and in a transfer credential.
///
/// The engine's ordinary handles are a process token plus a counter and are documented as
/// "not a security boundary". A transfer credential is one, and this server has no client
/// authentication to fall back on, so these come from the platform CSPRNG instead.
const TOKEN_BYTES: usize = 32;

#[derive(Debug, Error)]
pub enum FileError {
    #[error("no upload authorization matches that reference")]
    UnknownTicket,

    #[error("the transfer credential is not valid for this authorization")]
    BadCredential,

    #[error("the upload window has closed")]
    Expired,

    #[error("{staged} sources are already staged; consume or expire one first")]
    TooManyStaged { staged: usize },

    #[error("declared size {declared} is over the {limit}-byte source ceiling")]
    DeclaredTooLarge { declared: u64, limit: u64 },

    #[error("the transfer carried {actual} bytes, {declared} were declared")]
    SizeMismatch { actual: u64, declared: u64 },

    #[error("the transfer's digest does not match the declared digest")]
    DigestMismatch,

    #[error("unsupported digest algorithm {0:?}")]
    UnsupportedDigest(String),

    #[error("staging failed: {0}")]
    Staging(String),
}

impl FileError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownTicket => "matrix_file_unknown_ticket",
            Self::BadCredential => "matrix_file_bad_credential",
            Self::Expired => "matrix_file_expired",
            Self::TooManyStaged { .. } => "matrix_file_too_many_staged",
            Self::DeclaredTooLarge { .. } => "matrix_file_declared_too_large",
            Self::SizeMismatch { .. } => "matrix_file_size_mismatch",
            Self::DigestMismatch => "matrix_file_digest_mismatch",
            Self::UnsupportedDigest(_) => "matrix_file_unsupported_digest",
            Self::Staging(_) => "matrix_file_staging_failed",
        }
    }

    /// The code the ingest route puts on the wire.
    ///
    /// Every way of failing to present valid authority collapses to one value. The
    /// internal codes stay distinct for the operator's log, but a caller must not be
    /// able to tell a live authorization it guessed wrong from one that never existed,
    /// was already used, or has expired — each of those answers is an oracle for which
    /// identifiers are real.
    pub fn public_code(&self) -> &'static str {
        match self {
            Self::UnknownTicket | Self::BadCredential | Self::Expired => "matrix_file_unauthorized",
            other => other.code(),
        }
    }

    /// HTTP status for the ingest route.
    pub fn http_status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::UnknownTicket | Self::BadCredential | Self::Expired => StatusCode::FORBIDDEN,
            Self::TooManyStaged { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::DeclaredTooLarge { .. } | Self::SizeMismatch { .. } => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            Self::DigestMismatch | Self::UnsupportedDigest(_) => StatusCode::BAD_REQUEST,
            Self::Staging(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// A SEP-2631 digest. Only SHA-256 is accepted; the value is base64url without padding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDigest {
    pub algorithm: String,
    pub value: String,
}

/// Params of `files/authorizeUpload`.
///
/// Every field is optional in the draft. `_meta` carries the caller's file capability
/// declaration, which this server deliberately does not read: the pinned MCP library
/// cannot express that member in its typed client capabilities, so a caller that
/// negotiated the current protocol has it stripped before it arrives. Gating on it would
/// refuse callers that are behaving correctly.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizeUploadParams {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub digest: Option<FileDigest>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileValue {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<FileDigest>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferDescriptor {
    pub transport: &'static str,
    pub method: &'static str,
    pub url: String,
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthorizeUploadResult {
    pub file: FileValue,
    pub upload: TransferDescriptor,
}

/// An outstanding authorization: somewhere to put bytes and the terms they must meet.
struct Ticket {
    credential_hash: [u8; 32],
    partial: PathBuf,
    final_path: PathBuf,
    /// The exact size to verify against, present only when the caller stated one.
    declared_size: Option<u64>,
    /// What may be streamed at most, always present. Equals `declared_size` when there
    /// is one and the source ceiling otherwise.
    ceiling: u64,
    declared_digest: Option<[u8; 32]>,
    media_type: String,
    expires_at: Instant,
}

/// Whether an authorization is waiting for its transfer or already running one.
///
/// Both occupy a slot. An in-flight transfer that vanished from this map would be
/// counted by nothing while still holding a partial file and a connection.
enum TicketState {
    Idle(Ticket),
    InFlight,
}

impl Drop for Ticket {
    /// An authorization that never completed leaves nothing behind. After a successful
    /// rename the partial no longer exists and the removal is a no-op.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.partial);
    }
}

/// A completed transfer, waiting to be named by a tool call.
///
/// Owns its file: dropping it — on consumption, on expiry, or on shutdown — unlinks the
/// staged bytes, so there is one cleanup path rather than one per outcome.
#[derive(Debug)]
pub struct Staged {
    path: PathBuf,
    pub media_type: String,
    pub bytes: u64,
    staged_at: Instant,
}

impl Staged {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// How the transfer plane is configured. Absent public origin means the plane is off.
#[derive(Debug, Clone)]
pub struct FileConfig {
    /// Origin the intermediary will dial, scheme and authority only.
    pub public_origin: String,
    pub staging_dir: PathBuf,
    pub ttl: Duration,
    pub max_staged: usize,
    /// The decoder's own source ceiling, so an over-size transfer is refused at
    /// authorization rather than after it has been moved.
    pub max_source_bytes: u64,
}

pub struct FilePlane {
    config: FileConfig,
    tickets: Mutex<HashMap<String, TicketState>>,
    staged: Mutex<HashMap<String, Staged>>,
}

/// Reject anything that is not a bare `https` origin.
///
/// The descriptor URL is what a trusted intermediary will dial and what its own policy
/// checks; a path, query, or userinfo here would either be dropped or refused there, and
/// a plaintext origin is refused outright. Failing at startup beats minting descriptors
/// that every transfer rejects.
pub fn validate_public_origin(origin: &str) -> Result<String, String> {
    // Parsed rather than pattern-matched. Prefix and character checks admit strings that
    // look like origins without naming one — `https://:` and `https://[::1` among them —
    // and the first sign of that would be an intermediary refusing every descriptor.
    let parsed = url::Url::parse(origin).map_err(|e| format!("is not a URL: {e}"))?;

    if parsed.scheme() != "https" {
        return Err(format!("must use https, got {:?}", parsed.scheme()));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("must name a host".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("must not carry userinfo".into());
    }
    if !matches!(parsed.path(), "" | "/") || parsed.query().is_some() || parsed.fragment().is_some()
    {
        return Err("must be an origin only, with no path, query, or fragment".into());
    }

    // Rebuilt from the parse so the stored value is exactly what descriptors are built
    // from, with any default port and trailing slash normalised away.
    let host = parsed.host_str().expect("checked above");
    Ok(match parsed.port() {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    })
}

fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    // The platform CSPRNG. A transfer credential is the only authority on the ingest
    // route, and this server has no client authentication behind it.
    getrandom::fill(&mut bytes).expect("platform CSPRNG");
    base64_url(&bytes)
}

/// base64url without padding, the encoding SEP-2631 specifies for a digest value.
fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

impl FilePlane {
    pub async fn new(config: FileConfig) -> Result<Arc<Self>, FileError> {
        tokio::fs::create_dir_all(&config.staging_dir)
            .await
            .map_err(|e| FileError::Staging(format!("creating the staging directory: {e}")))?;
        restrict_directory(&config.staging_dir)?;

        Ok(Arc::new(Self {
            config,
            tickets: Mutex::new(HashMap::new()),
            staged: Mutex::new(HashMap::new()),
        }))
    }

    /// Mint a single-use authorization for one transfer.
    pub async fn authorize_upload(
        &self,
        params: AuthorizeUploadParams,
    ) -> Result<AuthorizeUploadResult, FileError> {
        // A size the decoder would refuse costs nothing to refuse now, before any bytes
        // are moved. Declaring one is optional in the draft, so its two jobs are kept
        // apart: `ceiling` always bounds what may be streamed, and `declared_size` is an
        // exact figure to verify against only when the caller actually stated one.
        // Collapsing them would authorize an undeclared transfer and then refuse it
        // unless it happened to be exactly the ceiling.
        if let Some(declared) = params.size
            && declared > self.config.max_source_bytes
        {
            return Err(FileError::DeclaredTooLarge {
                declared,
                limit: self.config.max_source_bytes,
            });
        }
        let ceiling = params.size.unwrap_or(self.config.max_source_bytes);

        let declared_digest = match &params.digest {
            None => None,
            Some(digest) => {
                if !digest.algorithm.eq_ignore_ascii_case("sha-256") {
                    return Err(FileError::UnsupportedDigest(
                        digest.algorithm.chars().take(24).collect(),
                    ));
                }
                Some(decode_digest(&digest.value)?)
            }
        };

        let id = random_token();
        let credential = random_token();
        let media_type = params
            .mime_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".into());

        // Both paths are built from the minted identifier, never from anything a caller
        // supplied. The ingest route looks an identifier up in this map and uses the
        // stored path; it never joins a request component onto the staging directory.
        let ticket = Ticket {
            credential_hash: sha256(credential.as_bytes()),
            partial: self.config.staging_dir.join(format!("{id}.part")),
            final_path: self.config.staging_dir.join(&id),
            declared_size: params.size,
            ceiling,
            declared_digest,
            media_type: media_type.clone(),
            expires_at: Instant::now() + self.config.ttl,
        };

        // Counted and inserted without releasing the lock in between. Checking the
        // ceiling and then re-acquiring would let concurrent authorizations all observe
        // the same pre-insert count and all pass, which is how a bound on outstanding
        // work turns into no bound at all. Lock order is tickets-then-staged everywhere.
        {
            let mut tickets = self.tickets.lock().await;
            let staged = self.staged.lock().await;
            let outstanding = tickets.len() + staged.len();
            if outstanding >= self.config.max_staged {
                return Err(FileError::TooManyStaged {
                    staged: outstanding,
                });
            }
            tickets.insert(id.clone(), TicketState::Idle(ticket));
        }

        let url = format!("{}/files/upload/{id}", self.config.public_origin);

        let mut headers = HashMap::new();
        headers.insert("Authorization".to_string(), format!("Bearer {credential}"));

        Ok(AuthorizeUploadResult {
            file: FileValue {
                uri: format!("{STAGED_PREFIX}{id}"),
                name: params.name,
                // Echoed exactly as declared, never substituted: an intermediary refuses a
                // result that alters the metadata it just sent.
                mime_type: params.mime_type,
                size: params.size,
                digest: params.digest,
            },
            upload: TransferDescriptor {
                transport: "https",
                method: "PUT",
                url,
                // The only authority on the ingest route. It travels in a header rather
                // than the URL so it stays out of proxy access logs.
                headers,
                // `expiresAt` is deliberately absent. It is optional, client clocks differ,
                // and this endpoint enforces the window itself — publishing a timestamp
                // would add a date formatter without adding enforcement.
            },
        })
    }

    /// Claim an authorization, verifying the credential before taking it in flight.
    ///
    /// Look-up, verification, and the transition happen under one lock, so two concurrent
    /// transfers cannot both claim one authorization and a wrong credential cannot burn
    /// one.
    ///
    /// The entry is left behind as [`TicketState::InFlight`] rather than removed. A
    /// removed entry would be counted by nothing while its transfer was still running,
    /// so a caller could authorize, begin a slow upload to free the slot, and repeat —
    /// accumulating partial files and connections that `max_staged` was supposed to
    /// bound.
    async fn claim(&self, id: &str, credential: &str) -> Result<Ticket, FileError> {
        let mut tickets = self.tickets.lock().await;
        let Some(TicketState::Idle(ticket)) = tickets.get(id) else {
            // A transfer already in flight is not claimable twice, and says the same
            // thing to a caller as one that never existed.
            return Err(FileError::UnknownTicket);
        };

        let presented = sha256(credential.as_bytes());
        // Constant time: a byte-by-byte comparison leaks the prefix length of a guess.
        let matches: bool =
            subtle::ConstantTimeEq::ct_eq(&presented[..], &ticket.credential_hash[..]).into();
        if !matches {
            return Err(FileError::BadCredential);
        }
        if Instant::now() > ticket.expires_at {
            tickets.remove(id);
            return Err(FileError::Expired);
        }

        match tickets.insert(id.to_string(), TicketState::InFlight) {
            Some(TicketState::Idle(ticket)) => Ok(ticket),
            _ => unreachable!("the idle ticket was matched under this lock"),
        }
    }

    /// Release an in-flight slot once its transfer has finished, however it finished.
    async fn release(&self, id: &str) {
        self.tickets.lock().await.remove(id);
    }

    /// Receive one transfer's bytes and stage them if they are what was declared.
    ///
    /// `chunks` is the request body. The declared length is a hint about what should
    /// arrive; the ceiling is enforced against what does, so a body that lies about or
    /// omits its length is cut off rather than trusted.
    pub async fn receive<S, E>(
        &self,
        id: &str,
        credential: &str,
        chunks: S,
    ) -> Result<(), FileError>
    where
        S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        let ticket = self.claim(id, credential).await?;

        // Bounded in time as well as in bytes. A transfer that stalls forever would
        // otherwise hold its slot forever, and enough of those wedge the plane for
        // everyone; the same window that expires an unused authorization applies here.
        let outcome = tokio::time::timeout(self.config.ttl, self.stream_into(id, &ticket, chunks))
            .await
            .unwrap_or(Err(FileError::Expired));

        // However it ended, the slot is returned. On success `stream_into` has already
        // recorded the staged source, which is what the slot becomes.
        self.release(id).await;
        outcome
    }

    async fn stream_into<S, E>(
        &self,
        id: &str,
        ticket: &Ticket,
        mut chunks: S,
    ) -> Result<(), FileError>
    where
        S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
        E: std::fmt::Display,
    {
        use futures_util::StreamExt as _;

        let mut file = tokio::fs::File::create(&ticket.partial)
            .await
            .map_err(|e| FileError::Staging(format!("creating the staged file: {e}")))?;
        restrict_file(&file).await?;

        let mut written = 0u64;
        let mut hasher = Sha256::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|e| FileError::Staging(format!("reading the body: {e}")))?;
            written = written.saturating_add(chunk.len() as u64);
            // Checked as the bytes arrive rather than afterwards: a declared length is a
            // claim, and a body that exceeds what was authorized must stop costing disk
            // immediately. Undeclared transfers are bounded by the source ceiling.
            if written > ticket.ceiling {
                return Err(FileError::SizeMismatch {
                    actual: written,
                    declared: ticket.ceiling,
                });
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| FileError::Staging(format!("writing the staged file: {e}")))?;
        }

        file.flush()
            .await
            .map_err(|e| FileError::Staging(format!("flushing the staged file: {e}")))?;
        drop(file);

        // An exact match is required only against a size the caller actually stated.
        if let Some(declared) = ticket.declared_size
            && written != declared
        {
            return Err(FileError::SizeMismatch {
                actual: written,
                declared,
            });
        }
        if let Some(expected) = ticket.declared_digest {
            let actual: [u8; 32] = hasher.finalize().into();
            let matches: bool = subtle::ConstantTimeEq::ct_eq(&actual[..], &expected[..]).into();
            if !matches {
                return Err(FileError::DigestMismatch);
            }
        }

        // The rename is what publishes the bytes. Until it happens the file is a partial
        // that the ticket's own drop removes, so a failed or abandoned transfer can never
        // be named by a tool call.
        tokio::fs::rename(&ticket.partial, &ticket.final_path)
            .await
            .map_err(|e| FileError::Staging(format!("publishing the staged file: {e}")))?;

        self.staged.lock().await.insert(
            id.to_string(),
            Staged {
                path: ticket.final_path.clone(),
                media_type: ticket.media_type.clone(),
                bytes: written,
                staged_at: Instant::now(),
            },
        );
        Ok(())
    }

    /// Consume a staged source by the URI this server minted for it.
    ///
    /// Single-use: the entry is removed, so a reference cannot be replayed to decode the
    /// same bytes twice or to outlive the call that named it. Returns `None` for anything
    /// this plane did not stage, which is what makes an unrecognised reference a refusal
    /// rather than a lookup somewhere else.
    pub async fn take(&self, uri: &str) -> Option<Staged> {
        let id = uri.strip_prefix(STAGED_PREFIX)?;
        self.staged.lock().await.remove(id)
    }

    /// Drop authorizations and staged bytes that outlived their window.
    ///
    /// Both maps are swept: an authorization nobody used holds a partial file, and a
    /// staged source nobody named holds a whole one.
    pub async fn sweep(&self) {
        let now = Instant::now();
        let ttl = self.config.ttl;
        self.tickets.lock().await.retain(|_, state| match state {
            // An in-flight transfer is bounded by its own receive deadline, not by this
            // sweep: collecting it here would unlink a partial file still being written.
            TicketState::InFlight => true,
            TicketState::Idle(ticket) => now <= ticket.expires_at,
        });
        self.staged
            .lock()
            .await
            .retain(|_, s| now.saturating_duration_since(s.staged_at) <= ttl);
    }

    #[cfg(test)]
    pub async fn outstanding(&self) -> (usize, usize) {
        (
            self.tickets.lock().await.len(),
            self.staged.lock().await.len(),
        )
    }
}

fn decode_digest(value: &str) -> Result<[u8; 32], FileError> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| FileError::DigestMismatch)?;
    raw.try_into().map_err(|_| FileError::DigestMismatch)
}

/// Owner-only on the staging directory. A newly created one gets `0700`.
#[cfg(unix)]
fn restrict_directory(dir: &Path) -> Result<(), FileError> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(dir, permissions)
        .map_err(|e| FileError::Staging(format!("restricting the staging directory: {e}")))
}

#[cfg(not(unix))]
fn restrict_directory(_dir: &Path) -> Result<(), FileError> {
    Ok(())
}

/// Owner-only on a staged file, applied before any byte is written to it.
#[cfg(unix)]
async fn restrict_file(file: &tokio::fs::File) -> Result<(), FileError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|e| FileError::Staging(format!("restricting the staged file: {e}")))
}

#[cfg(not(unix))]
async fn restrict_file(_file: &tokio::fs::File) -> Result<(), FileError> {
    Ok(())
}

/// The MCP method a file-aware intermediary calls to obtain a transfer descriptor.
pub const AUTHORIZE_UPLOAD: &str = "files/authorizeUpload";

/// The ingest route: where a trusted intermediary streams the bytes it authorized.
///
/// `PUT` because the descriptor names it, and because the target is one specific
/// authorization rather than a collection.
pub fn ingest_router(plane: Arc<FilePlane>) -> axum::Router {
    axum::Router::new()
        .route("/files/upload/{id}", axum::routing::put(ingest))
        .with_state(plane)
}

async fn ingest(
    axum::extract::State(plane): axum::extract::State<Arc<FilePlane>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse as _;

    let Some(credential) = bearer(&headers) else {
        return refusal(
            StatusCode::FORBIDDEN,
            FileError::BadCredential.public_code(),
        );
    };

    // `id` is only ever a map key. The path a transfer writes to was minted with the
    // authorization and is stored beside it, so nothing a caller sends is ever joined
    // onto the staging directory.
    match plane
        .receive(&id, credential, body.into_data_stream())
        .await
    {
        // No body, and deliberately nothing about where the bytes went.
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            // The operator's log keeps the precise reason; the caller gets the bounded one.
            tracing::warn!(code = e.code(), "refused a transfer");
            refusal(e.http_status(), e.public_code())
        }
    }
}

/// A bounded, machine-readable refusal. The stable contract is the code, and no refusal
/// carries a storage path, an identifier, or any part of a credential.
fn refusal(status: axum::http::StatusCode, code: &str) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (status, axum::Json(serde_json::json!({ "error": code }))).into_response()
}

fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// How often expired authorizations and staged sources are collected.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

pub async fn run_sweeper(plane: Arc<FilePlane>) {
    let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        plane.sweep().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "matrix-files-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ))
    }

    async fn plane(name: &str, max_source_bytes: u64) -> Arc<FilePlane> {
        FilePlane::new(FileConfig {
            public_origin: "https://panel.example".into(),
            staging_dir: scratch(name),
            ttl: Duration::from_secs(300),
            max_staged: 4,
            max_source_bytes,
        })
        .await
        .expect("plane")
    }

    fn body(
        chunks: Vec<&'static [u8]>,
    ) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> + Unpin
    {
        futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(bytes::Bytes::from_static(c)))
                .collect::<Vec<_>>(),
        )
    }

    fn credential_of(result: &AuthorizeUploadResult) -> String {
        result.upload.headers["Authorization"]
            .strip_prefix("Bearer ")
            .expect("bearer credential")
            .to_string()
    }

    fn id_of(result: &AuthorizeUploadResult) -> String {
        result.file.uri.strip_prefix(STAGED_PREFIX).unwrap().into()
    }

    fn params(size: u64, digest: Option<[u8; 32]>) -> AuthorizeUploadParams {
        AuthorizeUploadParams {
            name: None,
            mime_type: Some("image/gif".into()),
            size: Some(size),
            digest: digest.map(|d| FileDigest {
                algorithm: "sha-256".into(),
                value: base64_url(&d),
            }),
        }
    }

    #[tokio::test]
    async fn an_authorized_transfer_stages_bytes_a_tool_call_can_consume() {
        let plane = plane("roundtrip", 1024).await;
        let payload: &[u8] = b"panel media";
        let authorized = plane
            .authorize_upload(params(payload.len() as u64, Some(sha256(payload))))
            .await
            .expect("authorized");

        plane
            .receive(
                &id_of(&authorized),
                &credential_of(&authorized),
                body(vec![b"panel ", b"media"]),
            )
            .await
            .expect("transfer completes");

        let staged = plane.take(&authorized.file.uri).await.expect("staged");
        assert_eq!(staged.bytes, payload.len() as u64);
        assert_eq!(staged.media_type, "image/gif");
        assert_eq!(std::fs::read(staged.path()).expect("readable"), payload);

        // Single use: the same reference cannot be redeemed twice.
        assert!(plane.take(&authorized.file.uri).await.is_none());
    }

    #[tokio::test]
    async fn the_descriptor_points_at_the_configured_origin_and_carries_its_own_authority() {
        let plane = plane("descriptor", 1024).await;
        let authorized = plane.authorize_upload(params(4, None)).await.expect("ok");

        assert_eq!(authorized.upload.transport, "https");
        assert_eq!(authorized.upload.method, "PUT");
        assert!(
            authorized
                .upload
                .url
                .starts_with("https://panel.example/files/upload/"),
            "{}",
            authorized.upload.url
        );
        assert!(
            authorized.upload.headers.contains_key("Authorization"),
            "the credential travels in a header, not the URL"
        );
        assert!(
            !authorized.upload.url.contains(&credential_of(&authorized)),
            "the credential must not appear in the URL"
        );
        // Declared metadata is echoed exactly; an intermediary refuses a result that
        // alters what it sent.
        assert_eq!(authorized.file.size, Some(4));
        assert_eq!(authorized.file.mime_type.as_deref(), Some("image/gif"));
    }

    #[tokio::test]
    async fn a_wrong_credential_is_refused_and_does_not_burn_the_authorization() {
        let plane = plane("badcred", 1024).await;
        let authorized = plane.authorize_upload(params(5, None)).await.expect("ok");
        let id = id_of(&authorized);

        let err = plane
            .receive(&id, "not-the-credential", body(vec![b"hello"]))
            .await
            .expect_err("refused");
        assert_eq!(err.code(), "matrix_file_bad_credential");

        // The real credential still works: a guess must not consume the authorization.
        plane
            .receive(&id, &credential_of(&authorized), body(vec![b"hello"]))
            .await
            .expect("the genuine credential still redeems");
    }

    #[tokio::test]
    async fn an_authorization_cannot_be_replayed() {
        let plane = plane("replay", 1024).await;
        let authorized = plane.authorize_upload(params(5, None)).await.expect("ok");
        let (id, credential) = (id_of(&authorized), credential_of(&authorized));

        plane
            .receive(&id, &credential, body(vec![b"hello"]))
            .await
            .expect("first transfer");
        let err = plane
            .receive(&id, &credential, body(vec![b"hello"]))
            .await
            .expect_err("second transfer must be refused");
        assert_eq!(err.code(), "matrix_file_unknown_ticket");
    }

    #[tokio::test]
    async fn a_body_over_its_declared_size_is_cut_off_rather_than_staged() {
        let plane = plane("oversize", 1024).await;
        let authorized = plane.authorize_upload(params(4, None)).await.expect("ok");

        let err = plane
            .receive(
                &id_of(&authorized),
                &credential_of(&authorized),
                body(vec![b"way", b" past four"]),
            )
            .await
            .expect_err("over the declared size");
        assert_eq!(err.code(), "matrix_file_size_mismatch");
        assert!(
            plane.take(&authorized.file.uri).await.is_none(),
            "nothing may be stageable after a refused transfer"
        );
    }

    #[tokio::test]
    async fn a_body_short_of_its_declared_size_is_refused() {
        let plane = plane("short", 1024).await;
        let authorized = plane.authorize_upload(params(32, None)).await.expect("ok");

        let err = plane
            .receive(
                &id_of(&authorized),
                &credential_of(&authorized),
                body(vec![b"too short"]),
            )
            .await
            .expect_err("under the declared size");
        assert_eq!(err.code(), "matrix_file_size_mismatch");
    }

    #[tokio::test]
    async fn content_that_does_not_match_its_declared_digest_is_refused() {
        let plane = plane("digest", 1024).await;
        let authorized = plane
            .authorize_upload(params(5, Some(sha256(b"other"))))
            .await
            .expect("ok");

        let err = plane
            .receive(
                &id_of(&authorized),
                &credential_of(&authorized),
                body(vec![b"hello"]),
            )
            .await
            .expect_err("digest mismatch");
        assert_eq!(err.code(), "matrix_file_digest_mismatch");
        assert!(plane.take(&authorized.file.uri).await.is_none());
    }

    #[tokio::test]
    async fn a_declared_size_over_the_decoder_ceiling_is_refused_before_any_transfer() {
        let plane = plane("ceiling", 64).await;
        let err = plane
            .authorize_upload(params(65, None))
            .await
            .expect_err("over the source ceiling");
        assert_eq!(err.code(), "matrix_file_declared_too_large");
        assert_eq!(plane.outstanding().await, (0, 0), "nothing was minted");
    }

    #[tokio::test]
    async fn outstanding_transfers_are_bounded() {
        let plane = plane("bounded", 1024).await;
        for _ in 0..4 {
            plane.authorize_upload(params(4, None)).await.expect("ok");
        }
        let err = plane
            .authorize_upload(params(4, None))
            .await
            .expect_err("at the ceiling");
        assert_eq!(err.code(), "matrix_file_too_many_staged");
    }

    #[tokio::test]
    async fn a_reference_this_plane_did_not_mint_resolves_to_nothing() {
        let plane = plane("foreign", 1024).await;
        for uri in [
            "https://example.invalid/clip.gif",
            "file:///etc/passwd",
            "matrix-file://staged/not-a-real-identifier",
            "matrix-file://staged/../../etc/passwd",
        ] {
            assert!(
                plane.take(uri).await.is_none(),
                "{uri} must not resolve to anything"
            );
        }
    }

    #[tokio::test]
    async fn an_expired_authorization_is_swept_with_its_partial_file() {
        let plane = FilePlane::new(FileConfig {
            public_origin: "https://panel.example".into(),
            staging_dir: scratch("expiry"),
            ttl: Duration::from_millis(1),
            max_staged: 4,
            max_source_bytes: 1024,
        })
        .await
        .expect("plane");

        let authorized = plane.authorize_upload(params(4, None)).await.expect("ok");
        tokio::time::sleep(Duration::from_millis(20)).await;
        plane.sweep().await;

        assert_eq!(plane.outstanding().await, (0, 0));
        let err = plane
            .receive(
                &id_of(&authorized),
                &credential_of(&authorized),
                body(vec![b"four"]),
            )
            .await
            .expect_err("swept");
        assert_eq!(err.code(), "matrix_file_unknown_ticket");
    }

    #[tokio::test]
    async fn a_transfer_that_declares_no_size_is_accepted_and_counted() {
        // Declaring a size is optional in the draft. Folding an absent one into the
        // ceiling and then demanding an exact match would authorize every such transfer
        // and refuse all but a 64 MiB one.
        let plane = plane("nosize", 1024).await;
        let authorized = plane
            .authorize_upload(AuthorizeUploadParams {
                mime_type: Some("image/gif".into()),
                ..AuthorizeUploadParams::default()
            })
            .await
            .expect("authorized without a size");

        plane
            .receive(
                &id_of(&authorized),
                &credential_of(&authorized),
                body(vec![b"eleven byte"]),
            )
            .await
            .expect("an undeclared transfer completes");

        let staged = plane.take(&authorized.file.uri).await.expect("staged");
        assert_eq!(staged.bytes, 11, "the count comes from the bytes");
    }

    #[tokio::test]
    async fn an_undeclared_transfer_is_still_bounded_by_the_source_ceiling() {
        let plane = plane("nosize-ceiling", 4).await;
        let authorized = plane
            .authorize_upload(AuthorizeUploadParams::default())
            .await
            .expect("authorized");

        let err = plane
            .receive(
                &id_of(&authorized),
                &credential_of(&authorized),
                body(vec![b"far more than four"]),
            )
            .await
            .expect_err("over the ceiling");
        assert_eq!(err.code(), "matrix_file_size_mismatch");
    }

    #[tokio::test]
    async fn a_transfer_in_flight_still_occupies_its_slot() {
        // A claimed authorization used to vanish from both maps while its body streamed,
        // so a caller could authorize, start a slow upload to free the slot, and repeat
        // — accumulating partial files the ceiling was meant to bound.
        let plane = FilePlane::new(FileConfig {
            public_origin: "https://panel.example".into(),
            staging_dir: scratch("inflight"),
            ttl: Duration::from_secs(30),
            max_staged: 1,
            max_source_bytes: 1024,
        })
        .await
        .expect("plane");

        let authorized = plane.authorize_upload(params(5, None)).await.expect("ok");
        let (id, credential) = (id_of(&authorized), credential_of(&authorized));

        // A body that does not finish until this fires.
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let streaming = {
            let plane = plane.clone();
            tokio::spawn(async move {
                let chunks = futures_util::stream::once(async move {
                    let _ = held.await;
                    Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(b"hello"))
                });
                plane.receive(&id, &credential, Box::pin(chunks)).await
            })
        };

        // Give the transfer time to claim before asking for another slot.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let err = plane
            .authorize_upload(params(5, None))
            .await
            .expect_err("the in-flight transfer still holds the only slot");
        assert_eq!(err.code(), "matrix_file_too_many_staged");

        release.send(()).expect("still receiving");
        streaming
            .await
            .expect("joined")
            .expect("transfer completes");
    }

    #[test]
    fn every_way_of_failing_to_present_authority_looks_the_same_on_the_wire() {
        // Telling these apart would confirm which identifiers are real.
        let uniform = FileError::UnknownTicket.public_code();
        assert_eq!(FileError::BadCredential.public_code(), uniform);
        assert_eq!(FileError::Expired.public_code(), uniform);
        assert_eq!(
            FileError::UnknownTicket.http_status(),
            FileError::BadCredential.http_status()
        );
        // Refusals that say nothing about identifiers keep their own code.
        assert_eq!(
            FileError::DigestMismatch.public_code(),
            "matrix_file_digest_mismatch"
        );
    }

    #[test]
    fn only_a_bare_https_origin_is_accepted() {
        assert_eq!(
            validate_public_origin("https://panel.example/").expect("trailing slash trimmed"),
            "https://panel.example"
        );
        assert_eq!(
            validate_public_origin("https://panel.example:8443")
                .expect("port is part of an origin"),
            "https://panel.example:8443"
        );
        for bad in [
            "http://panel.example",
            "https://user:pw@panel.example",
            "https://panel.example/files",
            "https://panel.example?x=1",
            "panel.example",
            "https://",
            // Shapes a prefix-and-character check waves through while naming no host.
            // The first sign would be an intermediary refusing every descriptor.
            "https://:",
            "https://:8443",
            "https://[::1",
            "https:// panel.example",
        ] {
            assert!(
                validate_public_origin(bad).is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn a_minted_identifier_carries_real_entropy() {
        // Handles elsewhere in this server are a process token plus a counter and are
        // documented as not being a security boundary. These are the boundary.
        let tokens: std::collections::HashSet<String> = (0..64).map(|_| random_token()).collect();
        assert_eq!(tokens.len(), 64, "minted tokens must not repeat");
        assert!(random_token().len() >= 43, "256 bits of base64url");
    }
}
