use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex as AsyncMutex, mpsc::UnboundedSender},
};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use super::{GROK_BUILD_COMPATIBILITY_VERSION, GrokLoginEvent, read_bounded};

// Public Grok Build registration, not a Taceta registration or client secret.
// Protocol provenance: xai-org/grok-build, commit
// 37949780c144e37df692e3d669051a21fec24f20, xai-grok-login/src/config.rs.
// Taceta is independent; successful authorization and proxy access depend on
// xAI accepting this public client flow for the user's account.
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";
const KEYCHAIN_SERVICE: &str = "org.mlabo.taceta.grok.oauth";
const KEYCHAIN_ACCOUNT: &str = "oauth";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Serialize, Deserialize)]
pub(super) struct Credentials {
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) expires_at: Option<u64>,
    #[serde(default)]
    pub(super) user_id: Option<String>,
    #[serde(default)]
    pub(super) principal_type: Option<String>,
    #[serde(default)]
    pub(super) principal_id: Option<String>,
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.user_id.zeroize();
        self.principal_type.zeroize();
        self.principal_id.zeroize();
    }
}

// The working bridge's proxy contract is a token AND its authenticated user ID.
// Keep that complete pair together; an old token-only Keychain entry must first
// acquire its identity from the official OAuth service.
pub(super) struct SessionCredential {
    token: Zeroizing<String>,
    user_id: Zeroizing<String>,
}

impl SessionCredential {
    pub(super) fn token(&self) -> &str {
        self.token.as_str()
    }

    pub(super) fn user_id(&self) -> &str {
        self.user_id.as_str()
    }
}

pub(super) trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Credentials>, String>;
    fn save(&self, credentials: &Credentials) -> Result<(), String>;
    fn delete(&self) -> Result<(), String>;
}

pub(super) struct KeychainStore;

impl CredentialStore for KeychainStore {
    fn load(&self) -> Result<Option<Credentials>, String> {
        use security_framework::passwords::{PasswordOptions, generic_password};
        let bytes = match generic_password(PasswordOptions::new_generic_password(
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
        )) {
            Ok(bytes) => Zeroizing::new(bytes),
            Err(error) if error.code() == -25300 => return Ok(None),
            Err(_) => return Err("Unable to read Taceta's Grok credential from Keychain.".into()),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "Taceta's Grok credential is unreadable. Connect to Grok again.".into())
    }

    fn save(&self, credentials: &Credentials) -> Result<(), String> {
        let bytes = Zeroizing::new(
            serde_json::to_vec(credentials)
                .map_err(|_| "Unable to encode Taceta's Grok credential.".to_string())?,
        );
        security_framework::passwords::set_generic_password(
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
            &bytes,
        )
        .map_err(|_| "Unable to save Taceta's Grok credential in Keychain.".into())
    }

    fn delete(&self) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
        ) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == -25300 => Ok(()),
            Err(_) => Err("Unable to remove Taceta's Grok credential from Keychain.".into()),
        }
    }
}

pub(super) struct AuthEndpoints {
    pub(super) authorize: String,
    pub(super) token: String,
    pub(super) userinfo: String,
}

impl Default for AuthEndpoints {
    fn default() -> Self {
        Self {
            authorize: "https://auth.x.ai/oauth2/authorize".into(),
            token: "https://auth.x.ai/oauth2/token".into(),
            userinfo: "https://auth.x.ai/oauth2/userinfo".into(),
        }
    }
}

pub(super) struct AuthManager {
    http: reqwest::Client,
    endpoints: AuthEndpoints,
    store: Arc<dyn CredentialStore>,
    refresh: AsyncMutex<()>,
    login: AsyncMutex<()>,
    storage: Mutex<()>,
    epoch: AtomicU64,
}

impl AuthManager {
    pub(super) fn new(
        http: reqwest::Client,
        endpoints: AuthEndpoints,
        store: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            http,
            endpoints,
            store,
            refresh: AsyncMutex::new(()),
            login: AsyncMutex::new(()),
            storage: Mutex::new(()),
            epoch: AtomicU64::new(0),
        }
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub(super) fn is_signed_in(&self) -> Result<bool, String> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| "Grok credential access failed.")?;
        Ok(self
            .store
            .load()?
            .is_some_and(|token| !token.access_token.is_empty()))
    }

    pub(super) fn sign_out(&self) -> Result<(), String> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| "Grok credential access failed.")?;
        self.store.delete()?;
        self.epoch.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn load(&self) -> Result<Option<Credentials>, String> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| "Grok credential access failed.")?;
        self.store.load()
    }

    fn save_if_current(&self, token: &Credentials, epoch: u64, login: bool) -> Result<(), String> {
        let _guard = self
            .storage
            .lock()
            .map_err(|_| "Grok credential access failed.")?;
        if self.epoch() != epoch {
            return Err(
                "Grok connection changed. The pending authentication was cancelled.".into(),
            );
        }
        self.store.save(token)?;
        if login {
            self.epoch.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    pub(super) async fn session_credential(&self) -> Result<SessionCredential, String> {
        // The store is reread after acquiring this lock so every simultaneous
        // request observes the first request's rotated refresh token.
        let _refresh = self.refresh.lock().await;
        let epoch = self.epoch();
        let mut token = self
            .load()?
            .ok_or("Connect to Grok before using this provider.")?;
        if token.access_token.is_empty() {
            return Err("Taceta's Grok credential is empty. Connect to Grok again.".into());
        }
        if token
            .expires_at
            .is_some_and(|expiry| expiry <= now().saturating_add(60))
        {
            preserve_token_principal(&mut token);
            let refresh = token
                .refresh_token
                .as_deref()
                .filter(|token| !token.is_empty())
                .ok_or("Grok sign-in has expired. Connect to Grok again.")?;
            let mut form = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", CLIENT_ID),
            ];
            if let Some(kind) = token.principal_type.as_deref() {
                form.push(("principal_type", kind));
            }
            if let Some(id) = token.principal_id.as_deref() {
                form.push(("principal_id", id));
            }
            let mut updated = self.exchange(&form).await?;
            if updated.refresh_token.is_none() {
                updated.refresh_token = token.refresh_token.clone();
            }
            // Grok Build's oidc/refresh.rs deliberately reuses the identity
            // chosen at login, including principal selection, across rotation.
            updated.user_id = token.user_id.clone();
            updated.principal_type = token.principal_type.clone();
            updated.principal_id = token.principal_id.clone();
            // Preserve a rotated refresh token even if the subsequent one-time
            // identity lookup for an old token-only entry cannot finish.
            self.save_if_current(&updated, epoch, false)?;
            token = updated;
        }
        if token
            .user_id
            .as_deref()
            .is_none_or(|id| id.trim().is_empty())
        {
            self.complete_identity(&mut token).await?;
            self.save_if_current(&token, epoch, false)?;
        }
        let user_id = token
            .user_id
            .as_deref()
            .ok_or("Grok did not provide an account identity.")?;
        Ok(SessionCredential {
            token: Zeroizing::new(token.access_token.clone()),
            user_id: Zeroizing::new(user_id.to_owned()),
        })
    }

    pub(super) async fn sign_in(
        &self,
        events: UnboundedSender<GrokLoginEvent>,
    ) -> Result<(), String> {
        let _login = self.login.lock().await;
        let epoch = self.epoch();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| "Unable to start the local Grok sign-in callback.".to_string())?;
        let address = listener
            .local_addr()
            .map_err(|_| "Unable to read the local Grok sign-in callback address.".to_string())?;
        let redirect = format!("http://127.0.0.1:{}/callback", address.port());
        let verifier = Zeroizing::new(random_parameter());
        let state = Zeroizing::new(random_parameter());
        // Personal identity is obtained from authenticated OIDC userinfo.
        // An ID token is never accepted through an unverified local JWT decode.
        let nonce = Zeroizing::new(random_parameter());
        let mut authorize = Url::parse(&self.endpoints.authorize)
            .map_err(|_| "Grok authorization endpoint is invalid.".to_string())?;
        authorize.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect.as_str()),
            ("scope", SCOPES),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge(&verifier).as_str()),
            ("state", state.as_str()),
            ("nonce", nonce.as_str()),
        ]);
        events
            .send(GrokLoginEvent::OpenBrowser(authorize.into()))
            .map_err(|_| "Grok sign-in was cancelled.".to_string())?;
        events
            .send(GrokLoginEvent::Progress(
                "Complete Grok sign-in in your browser.".into(),
            ))
            .map_err(|_| "Grok sign-in was cancelled.".to_string())?;
        let callback =
            tokio::time::timeout(LOGIN_TIMEOUT, await_callback(listener, &redirect, &state));
        let code = tokio::select! {
            _ = events.closed() => return Err("Grok sign-in was cancelled.".into()),
            result = callback => result.map_err(|_| "Grok sign-in timed out. Connect again.")??,
        };
        if self.epoch() != epoch {
            return Err("Grok sign-in was cancelled.".into());
        }
        events
            .send(GrokLoginEvent::Progress("Completing Grok sign-in…".into()))
            .map_err(|_| "Grok sign-in was cancelled.".to_string())?;
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier.as_str()),
        ];
        let mut token = tokio::select! {
            _ = events.closed() => return Err("Grok sign-in was cancelled.".into()),
            token = self.exchange(&form) => token?,
        };
        tokio::select! {
            _ = events.closed() => return Err("Grok sign-in was cancelled.".into()),
            result = self.complete_identity(&mut token) => result?,
        }
        self.save_if_current(&token, epoch, true)
    }

    async fn complete_identity(&self, token: &mut Credentials) -> Result<(), String> {
        preserve_token_principal(token);
        if token.principal_type.as_deref() == Some("Team") {
            // This is the provider-issued principal from the access token, as
            // used by Grok Build's oidc/protocol.rs. It is only a routing hint;
            // the proxy validates the signed bearer and remains authoritative.
            token.user_id = token
                .principal_id
                .clone()
                .filter(|id| !id.trim().is_empty());
            return token.user_id.as_ref().map(|_| ()).ok_or_else(|| {
                "Grok's team credential has no principal ID. Connect to Grok again.".into()
            });
        }
        // Official endpoint published by auth.x.ai OIDC discovery. Use its
        // authenticated subject, never a guessed ID or an unverified JWT sub.
        let response = self
            .http
            .get(&self.endpoints.userinfo)
            .bearer_auth(&token.access_token)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(
                |_| "Unable to obtain Grok account identity. Your saved credential was preserved.",
            )?;
        if !response.status().is_success() {
            return Err(format!(
                "Grok account identity lookup failed (HTTP {}). Connect to Grok again.",
                response.status().as_u16()
            ));
        }
        #[derive(Deserialize)]
        struct UserInfo {
            sub: String,
        }
        let bytes = Zeroizing::new(read_bounded(response, 256 * 1024).await?);
        let info: UserInfo = serde_json::from_slice(&bytes)
            .map_err(|_| "Grok returned an invalid account identity.")?;
        if info.sub.trim().is_empty() {
            return Err("Grok returned an empty account identity.".into());
        }
        token.user_id = Some(info.sub);
        Ok(())
    }

    async fn exchange(&self, form: &[(&str, &str)]) -> Result<Credentials, String> {
        let response = self.http.post(&self.endpoints.token)
            .header("x-grok-client-version", GROK_BUILD_COMPATIBILITY_VERSION)
            .timeout(Duration::from_secs(15)).form(form).send().await
            .map_err(|_| "Unable to contact Grok's authorization service. Your saved credential was preserved.".to_string())?;
        let status = response.status();
        let bytes = Zeroizing::new(read_bounded(response, 256 * 1024).await?);
        if !status.is_success() {
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            return Err(match value.get("error").and_then(|value| value.as_str()) {
                Some("invalid_grant") => "Grok sign-in has expired or was revoked. Connect to Grok again.",
                Some("invalid_client" | "unauthorized_client") => "Grok did not accept the public OAuth client. This account connection is unavailable.",
                Some("access_denied") => "Grok authorization was declined.",
                _ => "Grok's authorization service rejected the request. Your saved credential was preserved.",
            }.into());
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            refresh_token: Option<String>,
            expires_in: Option<u64>,
            token_type: Option<String>,
        }
        impl Drop for TokenResponse {
            fn drop(&mut self) {
                self.access_token.zeroize();
                self.refresh_token.zeroize();
            }
        }
        let token: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| {
            "Grok returned an invalid credential. Your saved credential was preserved.".to_string()
        })?;
        if token.access_token.is_empty()
            || token
                .token_type
                .as_deref()
                .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            return Err(
                "Grok returned an unsupported credential. Your saved credential was preserved."
                    .into(),
            );
        }
        Ok(Credentials {
            access_token: token.access_token.clone(),
            refresh_token: token
                .refresh_token
                .clone()
                .filter(|token| !token.is_empty()),
            expires_at: token
                .expires_in
                .map(|seconds| now().saturating_add(seconds)),
            user_id: None,
            principal_type: None,
            principal_id: None,
        })
    }
}

fn preserve_token_principal(token: &mut Credentials) {
    if token.principal_type.is_some() || token.principal_id.is_some() {
        return;
    }
    // Match Grok Build's peek_access_token_principal: retain the principal
    // selected on the provider's consent screen for subsequent refresh grants.
    // These claims do not authenticate a user; the server validates the bearer.
    #[derive(Deserialize)]
    struct Principal {
        #[serde(default, alias = "principalType")]
        principal_type: Option<String>,
        #[serde(default, alias = "principalId")]
        principal_id: Option<String>,
    }
    let mut pieces = token.access_token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (pieces.next(), pieces.next(), pieces.next(), pieces.next())
    else {
        return;
    };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload) else {
        return;
    };
    let bytes = Zeroizing::new(bytes);
    let Ok(principal) = serde_json::from_slice::<Principal>(&bytes) else {
        return;
    };
    if let (Some(kind), Some(id)) = (principal.principal_type, principal.principal_id)
        && !kind.trim().is_empty()
        && !id.trim().is_empty()
    {
        token.principal_type = Some(kind);
        token.principal_id = Some(id);
    }
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn random_parameter() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}
pub(super) fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

async fn await_callback(
    listener: TcpListener,
    redirect: &str,
    state: &str,
) -> Result<Zeroizing<String>, String> {
    let base = Url::parse(redirect).map_err(|_| "Invalid Grok callback address.".to_string())?;
    let host = format!(
        "127.0.0.1:{}",
        base.port().ok_or("Missing Grok callback port.")?
    );
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|_| "The local Grok sign-in callback stopped.".to_string())?;
        let request =
            match tokio::time::timeout(Duration::from_secs(2), read_headers(&mut socket)).await {
                Ok(Ok(request)) => Zeroizing::new(request),
                _ => continue,
            };
        let result = callback_code(&request, &base, &host, state);
        let (status, message) = if result.is_ok() {
            (
                "200 OK",
                "Grok sign-in callback received. Return to Taceta to finish connecting.",
            )
        } else {
            (
                "400 Bad Request",
                "This sign-in callback was not accepted. Return to Taceta.",
            )
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\r\n{message}",
            message.len()
        );
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            socket.write_all(response.as_bytes()),
        )
        .await;
        match result {
            Ok(Callback::Code(code)) => return Ok(Zeroizing::new(code)),
            Ok(Callback::Denied) => {
                return Err("Grok authorization was declined in the browser.".into());
            }
            Err(()) => continue,
        }
    }
}

async fn read_headers(socket: &mut TcpStream) -> Result<String, ()> {
    let mut bytes = Zeroizing::new(Vec::new());
    let mut chunk = [0u8; 1024];
    loop {
        let count = socket.read(&mut chunk).await.map_err(|_| ())?;
        if count == 0 {
            return Err(());
        }
        bytes.extend_from_slice(&chunk[..count]);
        chunk.zeroize();
        if bytes.len() > 16 * 1024 {
            return Err(());
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes.to_vec()).map_err(|_| ());
        }
    }
}

enum Callback {
    Code(String),
    Denied,
}

fn callback_code(
    request: &str,
    base: &Url,
    host: &str,
    expected_state: &str,
) -> Result<Callback, ()> {
    let mut lines = request.split("\r\n");
    let mut start = lines.next().ok_or(())?.split_whitespace();
    if start.next() != Some("GET") {
        return Err(());
    }
    let target = start.next().ok_or(())?;
    if !matches!(start.next(), Some("HTTP/1.0" | "HTTP/1.1"))
        || start.next().is_some()
        || !target.starts_with('/')
        || target.starts_with("//")
        || target.contains('#')
    {
        return Err(());
    }
    let hosts: Vec<_> = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.eq_ignore_ascii_case("host"))
        .collect();
    if hosts.len() != 1 || hosts[0].1.trim() != host {
        return Err(());
    }
    let url = base.join(target).map_err(|_| ())?;
    if url.path() != "/callback" || url.origin() != base.origin() {
        return Err(());
    }
    let mut state = None;
    let mut code = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            _ => continue,
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err(());
        }
    }
    let state = Zeroizing::new(state.ok_or(())?);
    if state.len() != expected_state.len()
        || state
            .bytes()
            .zip(expected_state.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            != 0
    {
        return Err(());
    }
    match (code, error) {
        (Some(code), None) if !code.is_empty() => Ok(Callback::Code(code)),
        (None, Some(_)) => Ok(Callback::Denied),
        _ => Err(()),
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
