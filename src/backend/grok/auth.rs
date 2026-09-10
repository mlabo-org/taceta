/*
Ported from grok-codex-bridge/src/credential.rs. Taceta adaptations are limited
to its own credential directory, official-helper lifecycle and async UI entry.

MIT License

Copyright (c) 2026 grok-codex-bridge contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use thiserror::Error;
use zeroize::Zeroize;

use super::GrokLoginEvent;
use tokio::sync::mpsc::UnboundedSender;

const XAI_SCOPE_PREFIX: &str = "https://auth.x.ai::";
const XAI_ISSUER: &str = "https://auth.x.ai";
const TOKEN_TTL: Duration = Duration::days(30);
const MAX_AUTH_FILE_BYTES: u64 = 1024 * 1024;
const RENEWAL_POLL_INTERVAL: StdDuration = StdDuration::from_millis(250);
const OFFICIAL_REFRESH_TIMEOUT: StdDuration = StdDuration::from_secs(7);
const OFFICIAL_LOGIN_TIMEOUT: StdDuration = StdDuration::from_secs(300);
const CREDENTIAL_RENEWAL_GRACE: StdDuration = StdDuration::from_secs(60);

/// Taceta's UI boundary around the reference credential store. OAuth and token
/// refresh remain entirely owned by the official Grok executable.
pub(super) struct AuthManager {
    store: Result<Arc<CredentialStore>, String>,
}

impl AuthManager {
    /// Resolves paths only. Construction neither reads credentials nor launches
    /// the official helper, so opening Taceta cannot trigger account actions.
    pub(super) fn new() -> Self {
        Self {
            store: CredentialStore::for_taceta()
                .map(Arc::new)
                .map_err(|error| error.to_string()),
        }
    }

    #[cfg(test)]
    pub(super) fn with_store(store: CredentialStore) -> Self {
        Self {
            store: Ok(Arc::new(store)),
        }
    }

    fn store(&self) -> Result<Arc<CredentialStore>, String> {
        self.store.as_ref().map(Arc::clone).map_err(Clone::clone)
    }

    pub(super) fn epoch(&self) -> u64 {
        self.store
            .as_ref()
            .map_or(0, |store| store.epoch.load(Ordering::SeqCst))
    }

    pub(super) fn is_signed_in(&self) -> Result<bool, String> {
        match self.store()?.load() {
            Ok(_) => Ok(true),
            Err(error) if error.allows_official_login() => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Stops Taceta's pending helper and removes its isolated auth.json only.
    pub(super) fn sign_out(&self) -> Result<(), String> {
        self.store()?.clear().map_err(|error| error.to_string())
    }

    pub(super) async fn session_credential(&self) -> Result<Arc<SessionCredential>, String> {
        let store = self.store()?;
        let epoch = store.epoch.load(Ordering::SeqCst);
        let worker_store = Arc::clone(&store);
        let credential = tokio::task::spawn_blocking(move || {
            worker_store.load_with_renewal_grace_at(CREDENTIAL_RENEWAL_GRACE, epoch)
        })
        .await
        .map_err(|_| "Grok credential access did not finish.".to_string())?
        .map_err(|error| error.to_string())?;
        store
            .ensure_epoch(epoch)
            .map_err(|error| error.to_string())?;
        Ok(credential)
    }

    pub(super) async fn sign_in(
        &self,
        events: UnboundedSender<GrokLoginEvent>,
    ) -> Result<(), String> {
        let store = self.store()?;
        let epoch = store.epoch.load(Ordering::SeqCst);
        let mut cancellation = LoginCancellation {
            store: Arc::clone(&store),
            epoch,
            completed: false,
        };
        events
            .send(GrokLoginEvent::Progress(
                "Complete the official Grok sign-in in your browser.".into(),
            ))
            .map_err(|_| "Grok sign-in was cancelled.".to_string())?;
        let worker_store = Arc::clone(&store);
        let task =
            tokio::task::spawn_blocking(move || worker_store.ensure_with_official_login_at(epoch));
        tokio::select! {
            _ = events.closed() => Err("Grok sign-in was cancelled.".into()),
            result = task => {
                let credential = result
                    .map_err(|_| "The official Grok sign-in did not finish.".to_string())?
                    .map_err(|error| error.to_string())?;
                store.ensure_epoch(epoch).map_err(|error| error.to_string())?;
                drop(credential);
                store.epoch.compare_exchange(
                    epoch,
                    epoch.wrapping_add(1),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ).map_err(|_| CredentialError::AuthenticationCancelled.to_string())?;
                cancellation.completed = true;
                Ok(())
            }
        }
    }
}

struct LoginCancellation {
    store: Arc<CredentialStore>,
    epoch: u64,
    completed: bool,
}

impl Drop for LoginCancellation {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self.store.cancel_pending(Some(self.epoch));
        }
    }
}

pub struct CredentialStore {
    path: PathBuf,
    official_cli: PathBuf,
    cached: Mutex<Option<CachedCredential>>,
    renewal: RenewalGate,
    epoch: AtomicU64,
    helper_operation: Mutex<()>,
    helper: Mutex<Option<Child>>,
}

impl CredentialStore {
    fn for_taceta() -> Result<Self, CredentialError> {
        let path = resolve_auth_path()?;
        let official_cli = resolve_official_cli_path(&path)?;
        Self::with_official_cli(path, official_cli)
    }

    #[cfg(test)]
    pub fn new(path: PathBuf) -> Result<Self, CredentialError> {
        let official_cli = path
            .parent()
            .ok_or(CredentialError::RelativeAuthPath)?
            .join("bin/grok");
        Self::with_official_cli(path, official_cli)
    }

    pub(super) fn with_official_cli(
        path: PathBuf,
        official_cli: PathBuf,
    ) -> Result<Self, CredentialError> {
        if !path.is_absolute() || !official_cli.is_absolute() {
            return Err(CredentialError::RelativeAuthPath);
        }
        Ok(Self {
            path,
            official_cli,
            cached: Mutex::new(None),
            renewal: RenewalGate::default(),
            epoch: AtomicU64::new(0),
            helper_operation: Mutex::new(()),
            helper: Mutex::new(None),
        })
    }

    pub fn load(&self) -> Result<Arc<SessionCredential>, CredentialError> {
        let mut file = open_read_only(&self.path)?;
        let metadata = file.metadata().map_err(CredentialError::ReadAuth)?;
        validate_auth_metadata(&metadata)?;
        let fingerprint = FileFingerprint {
            len: metadata.len(),
            modified: metadata.modified().map_err(CredentialError::ReadAuth)?,
        };

        let mut cached = self
            .cached
            .lock()
            .map_err(|_| CredentialError::CacheUnavailable)?;
        if let Some(current) = cached
            .as_ref()
            .filter(|entry| entry.fingerprint == fingerprint)
        {
            current.credential.ensure_current()?;
            return Ok(Arc::clone(&current.credential));
        }

        let capacity = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let mut raw = String::with_capacity(capacity);
        file.read_to_string(&mut raw)
            .map_err(CredentialError::ReadAuth)?;
        let parsed = parse_auth_map(&raw);
        raw.zeroize();
        let credential = Arc::new(parsed?);
        credential.ensure_current()?;
        *cached = Some(CachedCredential {
            fingerprint,
            credential: Arc::clone(&credential),
        });
        Ok(credential)
    }

    /// Waits briefly for the official Grok flow to replace a credential that
    /// expired while this long-lived bridge process was asleep.
    ///
    /// This method never refreshes, rewrites, or sends the expired credential
    /// itself. It delegates one headless refresh attempt to the official Grok
    /// helper, then repeats the same read-only load until the authoritative
    /// file is replaced or the bounded grace period ends.
    pub fn load_with_renewal_grace(
        &self,
        grace_period: StdDuration,
    ) -> Result<Arc<SessionCredential>, CredentialError> {
        self.load_with_renewal_grace_at(grace_period, self.epoch.load(Ordering::SeqCst))
    }

    fn load_with_renewal_grace_at(
        &self,
        grace_period: StdDuration,
        epoch: u64,
    ) -> Result<Arc<SessionCredential>, CredentialError> {
        self.ensure_epoch(epoch)?;
        let deadline = Instant::now() + grace_period;
        let initial_error = match self.load() {
            Ok(credential) => return Ok(credential),
            Err(error) if error.allows_official_login() => error,
            Err(error) => return Err(error),
        };
        if deadline.saturating_duration_since(Instant::now()).is_zero() {
            return Err(initial_error);
        }

        let mut gate = self
            .renewal
            .state
            .lock()
            .map_err(|_| CredentialError::CacheUnavailable)?;
        if gate.in_flight {
            let generation = gate.generation;
            while gate.in_flight && gate.generation == generation {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(initial_error);
                }
                let (next_gate, timeout) = self
                    .renewal
                    .completed
                    .wait_timeout(gate, remaining)
                    .map_err(|_| CredentialError::CacheUnavailable)?;
                gate = next_gate;
                if timeout.timed_out() {
                    return Err(initial_error);
                }
            }
            drop(gate);
            return match self.load() {
                Ok(credential) => Ok(credential),
                Err(error) if !error.allows_official_login() => Err(error),
                Err(_) => Err(initial_error),
            };
        }

        gate.in_flight = true;
        drop(gate);

        let result = self.renew_credential_until(initial_error, deadline, epoch);
        let mut gate = self
            .renewal
            .state
            .lock()
            .map_err(|_| CredentialError::CacheUnavailable)?;
        gate.in_flight = false;
        gate.generation = gate.generation.wrapping_add(1);
        self.renewal.completed.notify_all();
        drop(gate);
        result
    }

    fn renew_credential_until(
        &self,
        initial_error: CredentialError,
        deadline: Instant,
        epoch: u64,
    ) -> Result<Arc<SessionCredential>, CredentialError> {
        let _ = self.run_official_refresh(epoch);
        loop {
            self.ensure_epoch(epoch)?;
            match self.load() {
                Ok(credential) => return Ok(credential),
                Err(error) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if !error.allows_official_login() || remaining.is_zero() {
                        return Err(initial_error);
                    }
                    thread::sleep(RENEWAL_POLL_INTERVAL.min(remaining));
                }
            }
        }
    }

    /// Ensures that the official credential is usable for an explicit,
    /// foreground lifecycle operation.
    ///
    /// Recoverable missing, incomplete, or expired login state delegates once
    /// to the official desktop OAuth flow. The official CLI owns browser OAuth
    /// and credential writes; the bridge only waits for its exit and rereads
    /// the authoritative file. Malformed, ambiguous, or unsafe credential
    /// state fails closed without launching login.
    pub fn ensure_with_official_login(&self) -> Result<Arc<SessionCredential>, CredentialError> {
        self.ensure_with_official_login_at(self.epoch.load(Ordering::SeqCst))
    }

    fn ensure_with_official_login_at(
        &self,
        epoch: u64,
    ) -> Result<Arc<SessionCredential>, CredentialError> {
        self.ensure_epoch(epoch)?;
        match self.load() {
            Ok(credential) => return Ok(credential),
            Err(error) if error.allows_official_login() => {}
            Err(error) => return Err(error),
        }

        let _ = self.run_official_refresh(epoch);
        self.ensure_epoch(epoch)?;
        match self.reload_uncached() {
            Ok(credential) => return Ok(credential),
            Err(error) if error.allows_official_login() => {}
            Err(error) => return Err(error),
        }

        self.run_official_login(epoch)?;
        self.ensure_epoch(epoch)?;
        self.reload_uncached()
    }

    fn reload_uncached(&self) -> Result<Arc<SessionCredential>, CredentialError> {
        self.cached
            .lock()
            .map_err(|_| CredentialError::CacheUnavailable)?
            .take();
        self.load()
    }

    fn ensure_epoch(&self, epoch: u64) -> Result<(), CredentialError> {
        if self.epoch.load(Ordering::SeqCst) == epoch {
            Ok(())
        } else {
            Err(CredentialError::AuthenticationCancelled)
        }
    }

    fn cancel_pending(&self, expected_epoch: Option<u64>) -> Result<(), CredentialError> {
        let mut helper = self
            .helper
            .lock()
            .map_err(|_| CredentialError::CacheUnavailable)?;
        if expected_epoch.is_some_and(|epoch| self.ensure_epoch(epoch).is_err()) {
            return Ok(());
        }
        self.epoch.fetch_add(1, Ordering::SeqCst);
        stop_helper(&mut helper);
        Ok(())
    }

    fn clear(&self) -> Result<(), CredentialError> {
        // Hold the spawn boundary through deletion. A pending helper cannot
        // republish the credential after an explicit Taceta disconnect.
        let mut helper = self
            .helper
            .lock()
            .map_err(|_| CredentialError::CacheUnavailable)?;
        self.epoch.fetch_add(1, Ordering::SeqCst);
        stop_helper(&mut helper);
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CredentialError::DeleteAuth(error)),
        }
        self.cached
            .lock()
            .map_err(|_| CredentialError::CacheUnavailable)?
            .take();
        Ok(())
    }

    fn spawn_helper(&self, args: &[&str], epoch: u64) -> Result<(), CredentialError> {
        let mut helper = self
            .helper
            .lock()
            .map_err(|_| CredentialError::CacheUnavailable)?;
        self.ensure_epoch(epoch)?;
        let grok_home = self
            .path
            .parent()
            .ok_or(CredentialError::RelativeAuthPath)?;
        create_private_auth_home(grok_home)?;
        *helper = Some(
            Command::new(&self.official_cli)
                .args(args)
                // Both settings are supported by the official Grok 1.0.13
                // source. An inline GROK_AUTH must not bypass this own store.
                .env("GROK_HOME", grok_home)
                .env("GROK_AUTH_PATH", &self.path)
                .env_remove("GROK_AUTH")
                .current_dir(grok_home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|source| CredentialError::StartOfficialLogin {
                    path: self.official_cli.clone(),
                    source,
                })?,
        );
        Ok(())
    }

    fn run_official_refresh(&self, epoch: u64) -> bool {
        let Ok(_operation) = self.helper_operation.lock() else {
            return false;
        };
        if self.spawn_helper(&["models"], epoch).is_err() {
            return false;
        }
        let deadline = Instant::now() + OFFICIAL_REFRESH_TIMEOUT;
        loop {
            let Ok(mut helper) = self.helper.lock() else {
                return false;
            };
            let Some(child) = helper.as_mut() else {
                return false;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    helper.take();
                    return status.success();
                }
                Ok(None) if Instant::now() < deadline => {}
                Ok(None) | Err(_) => {
                    stop_helper(&mut helper);
                    return false;
                }
            }
            drop(helper);
            thread::sleep(
                RENEWAL_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    fn run_official_login(&self, epoch: u64) -> Result<(), CredentialError> {
        let _operation = self
            .helper_operation
            .lock()
            .map_err(|_| CredentialError::CacheUnavailable)?;
        self.spawn_helper(&["login", "--oauth"], epoch)?;
        let deadline = Instant::now() + OFFICIAL_LOGIN_TIMEOUT;
        loop {
            let mut helper = self
                .helper
                .lock()
                .map_err(|_| CredentialError::CacheUnavailable)?;
            let Some(child) = helper.as_mut() else {
                return Err(CredentialError::AuthenticationCancelled);
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    helper.take();
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(CredentialError::OfficialLoginFailed(status.code()))
                    };
                }
                Ok(None) if Instant::now() < deadline => {}
                Ok(None) => {
                    stop_helper(&mut helper);
                    return Err(CredentialError::OfficialLoginTimedOut);
                }
                Err(error) => {
                    stop_helper(&mut helper);
                    return Err(CredentialError::WaitForOfficialLogin(error));
                }
            }
            drop(helper);
            thread::sleep(
                RENEWAL_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

fn stop_helper(helper: &mut Option<Child>) {
    if let Some(mut child) = helper.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn create_private_auth_home(path: &Path) -> Result<(), CredentialError> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(CredentialError::UnsafeAuthDirectory);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(CredentialError::CreateAuthDirectory)?;
        }
        Err(error) => return Err(CredentialError::CreateAuthDirectory(error)),
    }
    Ok(())
}

fn resolve_auth_path() -> Result<PathBuf, CredentialError> {
    let home = nonempty_env("HOME").ok_or(CredentialError::HomeUnavailable)?;
    Ok(absolute_path(home)?.join("Library/Application Support/Taceta/grok/auth.json"))
}

fn resolve_official_cli_path(auth_path: &Path) -> Result<PathBuf, CredentialError> {
    if let Some(home) = nonempty_env("GROK_HOME") {
        return Ok(absolute_path(home)?.join("bin/grok"));
    }
    if let Some(home) = nonempty_env("HOME") {
        return Ok(absolute_path(home)?.join(".grok/bin/grok"));
    }
    auth_path
        .parent()
        .map(|parent| parent.join("bin/grok"))
        .ok_or(CredentialError::RelativeAuthPath)
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn absolute_path(value: String) -> Result<PathBuf, CredentialError> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(CredentialError::RelativeAuthPath);
    }
    Ok(path)
}

#[cfg(unix)]
fn open_read_only(path: &Path) -> Result<fs::File, CredentialError> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(CredentialError::ReadAuth)
}

#[cfg(not(unix))]
fn open_read_only(_path: &Path) -> Result<fs::File, CredentialError> {
    Err(CredentialError::UnsupportedPermissionPlatform)
}

#[cfg(unix)]
fn validate_auth_metadata(metadata: &fs::Metadata) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt;

    if !metadata.is_file() {
        return Err(CredentialError::UnsafeAuthFileType);
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(CredentialError::UnsafeAuthPermissions);
    }
    if metadata.len() == 0 || metadata.len() > MAX_AUTH_FILE_BYTES {
        return Err(CredentialError::InvalidAuthFileSize);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_auth_metadata(_metadata: &fs::Metadata) -> Result<(), CredentialError> {
    Err(CredentialError::UnsupportedPermissionPlatform)
}

fn parse_auth_map(source: &str) -> Result<SessionCredential, CredentialError> {
    let entries: BTreeMap<String, RawAuthRecord> =
        serde_json::from_str(source).map_err(CredentialError::ParseAuth)?;
    let candidates: Vec<_> = entries
        .iter()
        .filter(|(scope, record)| record.is_current_xai_session(scope))
        .collect();

    let [(scope, selected)] = candidates.as_slice() else {
        return Err(if candidates.is_empty() {
            CredentialError::SessionCredentialMissing
        } else {
            CredentialError::AmbiguousSessionCredential
        });
    };

    if selected.key.trim().is_empty() || selected.user_id.trim().is_empty() {
        return Err(CredentialError::InvalidSessionCredential);
    }
    let expires_at = selected
        .expires_at
        .unwrap_or(selected.create_time + TOKEN_TTL);
    let credential = SessionCredential {
        token: SecretString::new(selected.key.clone()),
        user_id: selected.user_id.clone(),
        scope: (*scope).clone(),
        expires_at,
    };
    credential.ensure_current()?;
    Ok(credential)
}

#[derive(Deserialize)]
struct RawAuthRecord {
    key: String,
    auth_mode: AuthMode,
    create_time: DateTime<Utc>,
    user_id: String,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    oidc_issuer: Option<String>,
}

impl RawAuthRecord {
    fn is_current_xai_session(&self, scope: &str) -> bool {
        scope.starts_with(XAI_SCOPE_PREFIX)
            && matches!(self.auth_mode, AuthMode::Oidc | AuthMode::External)
            && self
                .oidc_issuer
                .as_deref()
                .is_none_or(|issuer| issuer == XAI_ISSUER)
    }
}

impl Drop for RawAuthRecord {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AuthMode {
    Oidc,
    External,
    #[serde(other)]
    Unsupported,
}

pub struct SessionCredential {
    token: SecretString,
    user_id: String,
    scope: String,
    expires_at: DateTime<Utc>,
}

impl SessionCredential {
    pub(crate) fn token(&self) -> &str {
        self.token.expose()
    }

    pub(crate) fn user_id(&self) -> &str {
        &self.user_id
    }

    fn ensure_current(&self) -> Result<(), CredentialError> {
        if self.expires_at <= Utc::now() {
            return Err(CredentialError::ExpiredSessionCredential);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn for_test(token: &str, user_id: &str) -> Self {
        Self {
            token: SecretString::new(token.to_owned()),
            user_id: user_id.to_owned(),
            scope: format!("{XAI_SCOPE_PREFIX}test-client"),
            expires_at: DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }
}

impl fmt::Debug for SessionCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionCredential")
            .field("token", &"[REDACTED]")
            .field("scope", &self.scope)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Drop for SessionCredential {
    fn drop(&mut self) {
        self.user_id.zeroize();
        self.scope.zeroize();
    }
}

struct SecretString(String);

impl SecretString {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified: SystemTime,
}

struct CachedCredential {
    fingerprint: FileFingerprint,
    credential: Arc<SessionCredential>,
}

#[derive(Default)]
struct RenewalGate {
    state: Mutex<RenewalState>,
    completed: Condvar,
}

#[derive(Default)]
struct RenewalState {
    in_flight: bool,
    generation: u64,
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("Grok auth file path must be absolute")]
    RelativeAuthPath,
    #[error("HOME is unavailable; Taceta's private Grok credential directory cannot be resolved")]
    HomeUnavailable,
    #[error("failed to create Taceta's private Grok credential directory")]
    CreateAuthDirectory(#[source] std::io::Error),
    #[error("Taceta's Grok credential directory must be a non-symlink directory")]
    UnsafeAuthDirectory,
    #[error("failed to remove Taceta's Grok credential")]
    DeleteAuth(#[source] std::io::Error),
    #[error("failed to read Grok auth file")]
    ReadAuth(#[source] std::io::Error),
    #[error("Grok auth file must be a regular non-symlink file")]
    UnsafeAuthFileType,
    #[error("Grok auth file must not be accessible by group or other users")]
    UnsafeAuthPermissions,
    #[error("Grok auth file size is invalid")]
    InvalidAuthFileSize,
    #[error("Grok auth file is not valid JSON")]
    ParseAuth(#[source] serde_json::Error),
    #[error("one current xAI session credential was not found; run the official Grok login flow")]
    SessionCredentialMissing,
    #[error(
        "multiple current xAI session credentials were found; select one official Grok auth file"
    )]
    AmbiguousSessionCredential,
    #[error("the selected xAI session credential is incomplete")]
    InvalidSessionCredential,
    #[error("the selected xAI session credential is expired; run the official Grok login flow")]
    ExpiredSessionCredential,
    #[error("the official Grok CLI is unavailable at {path}; install Grok Build before connecting")]
    StartOfficialLogin {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the official Grok browser login failed with exit code {0:?}")]
    OfficialLoginFailed(Option<i32>),
    #[error("the official Grok browser login did not finish within five minutes")]
    OfficialLoginTimedOut,
    #[error("failed while waiting for the official Grok browser login")]
    WaitForOfficialLogin(#[source] std::io::Error),
    #[error("Grok authentication was cancelled because the Taceta connection changed")]
    AuthenticationCancelled,
    #[error("credential memory cache is unavailable")]
    CacheUnavailable,
    #[cfg(not(unix))]
    #[error("credential permission validation is unsupported on this platform")]
    UnsupportedPermissionPlatform,
}

impl CredentialError {
    fn allows_official_login(&self) -> bool {
        matches!(
            self,
            Self::SessionCredentialMissing
                | Self::InvalidSessionCredential
                | Self::ExpiredSessionCredential
        ) || matches!(self, Self::ReadAuth(error) if error.kind() == std::io::ErrorKind::NotFound)
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
