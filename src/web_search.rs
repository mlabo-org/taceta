//! Optional, explicit web-search harness for Taceta.
//!
//! This module is deliberately independent from `InferenceBackend`: when the
//! caller does not create a provider, no web request can be made.  Providers
//! never silently fall back to another provider.

use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, ToSocketAddrs},
    process::Command,
    time::Duration,
};
use url::Url;

const MAX_RESULTS: usize = 5;
const MAX_BODY: usize = 1024 * 1024;
const MAX_URL: usize = 2048;
const MAX_REDIRECTS: usize = 3;
pub const KEYCHAIN_SERVICE: &str = "org.mlabo.taceta.web-search";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProviderKind {
    Brave,
    Ollama,
    DefaultSearch,
    GoogleSearch,
    ChatGptWeb,
    #[serde(other)]
    Unknown,
}

impl Default for ProviderKind {
    fn default() -> Self {
        Self::Brave
    }
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Brave => "Brave Search",
            Self::Ollama => "Ollama Web Search",
            Self::DefaultSearch => "Default Browser Search",
            Self::GoogleSearch => "Google Search",
            Self::ChatGptWeb => "ChatGPT Web",
            Self::Unknown => "Unconfigured Web Executor",
        }
    }

    /// Stable provider identifier used in `ChatRequest`; never localised.
    pub fn wire_value(self) -> &'static str {
        match self {
            Self::Brave => "brave",
            Self::Ollama => "ollama",
            Self::DefaultSearch => "default_search",
            Self::GoogleSearch => "google_search",
            Self::ChatGptWeb => "chatgpt_web",
            Self::Unknown => "unknown",
        }
    }

    pub fn account_name(self) -> Option<&'static str> {
        match self {
            Self::Brave => Some("brave"),
            Self::Ollama => Some("ollama"),
            Self::DefaultSearch | Self::GoogleSearch | Self::ChatGptWeb => None,
            Self::Unknown => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchedPage {
    pub url: String,
    pub content_type: String,
    pub text: String,
}

/// Ollama-compatible function declarations. These are supplied only when
/// search is explicitly enabled for a conversation.
pub fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {"type":"function","function":{"name":"web_search","description":"Search the web and return up to five sources. For factual answers, inspect one to five relevant returned sources with web_fetch before synthesizing the answer.","parameters":{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":5}},"required":["query"]}}},
        {"type":"function","function":{"name":"web_fetch","description":"Fetch readable text from one public HTTPS URL. After web_search, fetch one to five relevant independent sources before synthesizing a factual answer.","parameters":{"type":"object","properties":{"url":{"type":"string","maxLength":2048}},"required":["url"]}}}
    ])
}

/// Returns true when the user asks the local model to invent the concrete
/// research question before searching. Such prompts must reach the model
/// first; sending the meta-instruction itself to a search provider loses the
/// question the user asked the model to create.
pub fn requests_model_generated_search_query(input: &str) -> bool {
    let lower = input.to_lowercase();
    let compact: String = lower
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let asks_to_search = [
        "web検索",
        "webサーチ",
        "ウェブ検索",
        "ウェブサーチ",
        "インターネット検索",
        "ネット検索",
    ]
    .iter()
    .any(|marker| compact.contains(marker))
        || [
            "web search",
            "search the web",
            "search online",
            "browse the web",
        ]
        .iter()
        .any(|marker| lower.contains(marker));
    let asks_to_generate_query = [
        "質問作って",
        "質問を作って",
        "質問考えて",
        "質問を考えて",
        "検索用の質問",
        "検索する質問",
        "検索クエリを作って",
        "検索語を作って",
    ]
    .iter()
    .any(|marker| compact.contains(marker))
        || [
            "make up a question",
            "come up with a question",
            "create a question",
            "generate a question",
            "create a search query",
        ]
        .iter()
        .any(|marker| lower.contains(marker));
    asks_to_search && asks_to_generate_query
}

/// Bypasses semantic routing only for an explicit command to search now.
/// Freshness, source, and factual-verification needs are deliberately absent:
/// the local structured router decides those cases without a phrase list.
pub fn requires_mandatory_search(input: &str) -> bool {
    let lower = input.to_lowercase();
    let compact: String = lower
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();

    if requests_model_generated_search_query(input) {
        return false;
    }

    const DIRECT_JA: &[&str] = &[
        "web検索して",
        "web検索しろ",
        "web検索せよ",
        "web検索を実行",
        "web検索を起動",
        "web検索を使って",
        "webサーチして",
        "webサーチしろ",
        "webサーチせよ",
        "webサーチを実行",
        "webサーチを起動",
        "webサーチを使って",
        "ウェブ検索して",
        "ウェブ検索しろ",
        "ウェブ検索せよ",
        "ウェブ検索を実行",
        "ウェブ検索を起動",
        "ウェブで調べて",
        "インターネットで調べて",
        "インターネットで検索して",
        "ネットで調べて",
        "ネットで検索して",
    ];
    const DIRECT_EN: &[&str] = &[
        "search the web",
        "search online",
        "browse the web",
        "use web search",
        "run a web search",
        "look it up online",
    ];
    DIRECT_JA.iter().any(|marker| compact.contains(marker))
        || DIRECT_EN.iter().any(|marker| lower.contains(marker))
}

/// Extracts the concrete question from a model response that announced an
/// immediate search without issuing a tool call. Only a question-terminated
/// sentence is accepted; callers retain their original-input fallback when
/// the model did not actually state a usable query.
pub fn concrete_search_query_from_assistant(output: &str) -> Option<String> {
    let output = output.trim();
    let (question_index, question_mark) = output
        .char_indices()
        .filter(|(_, character)| matches!(character, '?' | '？'))
        .last()?;
    let before_question = &output[..question_index];
    let start = before_question
        .char_indices()
        .rev()
        .find(|(_, character)| {
            matches!(
                character,
                '\n' | '「' | '『' | '“' | '"' | ':' | '：' | '。' | '！' | '!'
            )
        })
        .map_or(0, |(index, character)| index + character.len_utf8());
    let end = question_index + question_mark.len_utf8();
    let query = output[start..end]
        .trim()
        .trim_start_matches(|character: char| {
            character.is_whitespace()
                || matches!(character, '-' | '*' | '#' | '・' | '「' | '『' | '“' | '"')
        })
        .trim();
    let length = query.chars().count();
    (4..=512).contains(&length).then(|| query.to_owned())
}

/// Detects a model response that promises an immediate Web search instead of
/// issuing the declared tool call. This intentionally accepts only explicit
/// action phrases; discussion about search, past searches, inability, and
/// questions are excluded so ordinary chat is not sent outside the Mac.
pub fn expresses_immediate_search_intent(output: &str) -> bool {
    let lower = output.to_lowercase();
    let compact: String = lower
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();

    const NEGATIVE_JA: &[&str] = &[
        "検索しません",
        "検索できません",
        "検索はできません",
        "検索を実行できません",
        "検索する必要はありません",
        "検索は不要",
        "検索しました",
        "検索済み",
        "検索しますか",
        "検索しましょうか",
        "検索してください",
        "検索について",
        "検索という",
        "調べません",
        "調べられません",
        "調べました",
        "調べますか",
        "調べましょうか",
        "調べてください",
    ];
    const NEGATIVE_EN: &[&str] = &[
        "won't search",
        "will not search",
        "can't search",
        "cannot search",
        "already searched",
        "searched the web",
        "should i search",
        "would you like me to search",
        "how to search",
        "about web search",
    ];
    if NEGATIVE_JA.iter().any(|marker| compact.contains(marker))
        || NEGATIVE_EN.iter().any(|marker| lower.contains(marker))
    {
        return false;
    }

    const INTENT_JA: &[&str] = &[
        "webサーチします",
        "webサーチを実行します",
        "ウェブサーチします",
        "ウェブサーチを実行します",
        "検索します",
        "検索してみます",
        "検索を実行します",
        "検索を開始します",
        "検索に移ります",
        "webで調べます",
        "ウェブで調べます",
        "インターネットで調べます",
        "ネットで調べます",
        "オンラインで調べます",
    ];
    const INTENT_EN: &[&str] = &[
        "i'll search the web",
        "i’ll search the web",
        "i will search the web",
        "let me search the web",
        "i'm going to search the web",
        "i am going to search the web",
        "i'll search online",
        "i’ll search online",
        "i will search online",
        "let me search online",
        "i'll browse the web",
        "i’ll browse the web",
        "i will browse the web",
        "let me browse the web",
        "i'll look it up online",
        "i’ll look it up online",
        "i will look it up online",
        "let me look it up online",
    ];

    INTENT_JA.iter().any(|marker| compact.contains(marker))
        || INTENT_EN.iter().any(|marker| lower.contains(marker))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

pub fn parse_tool_call(value: &serde_json::Value) -> Result<ToolCall, WebError> {
    let function = value
        .get("function")
        .ok_or_else(|| WebError::Protocol("tool call missing function".into()))?;
    let name = function
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| WebError::Protocol("tool call missing name".into()))?;
    if !matches!(name, "web_search" | "web_fetch") {
        return Err(WebError::Protocol("unsupported web tool".into()));
    }
    let arguments = function
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let arguments = if arguments.is_string() {
        serde_json::from_str(arguments.as_str().unwrap())
            .map_err(|_| WebError::Protocol("tool arguments are not JSON".into()))?
    } else {
        arguments
    };
    Ok(ToolCall {
        name: name.into(),
        arguments,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("web search is disabled")]
    Disabled,
    #[error("invalid or unsafe URL: {0}")]
    UnsafeUrl(String),
    #[error("web provider request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("web provider {stage} transport failure ({kind})")]
    Transport {
        stage: &'static str,
        kind: &'static str,
    },
    #[error("web provider returned HTTP {0}")]
    Http(StatusCode),
    #[error("web provider response was invalid: {0}")]
    Protocol(String),
    #[error("Keychain credential is not configured for {0}")]
    MissingCredential(&'static str),
    #[error("Keychain access failed")]
    Keychain,
    #[error("Taceta Link is unavailable")]
    LinkUnavailable,
    #[error("browser authentication is required")]
    AuthRequired,
    #[error("browser request timed out")]
    Timeout,
    #[error("browser request outcome is ambiguous; retry explicitly")]
    Ambiguous,
}

#[derive(Clone)]
pub struct WebSearchProvider {
    kind: ProviderKind,
    client: Client,
    endpoint: String,
    bearer: Option<String>,
}

impl WebSearchProvider {
    pub fn brave() -> Result<Self, WebError> {
        Self::new(ProviderKind::Brave, keychain_secret("brave"))
    }
    pub fn ollama() -> Result<Self, WebError> {
        Self::new(ProviderKind::Ollama, keychain_secret("ollama"))
    }
    pub fn from_kind(kind: ProviderKind) -> Result<Self, WebError> {
        match kind {
            ProviderKind::Brave => Self::brave(),
            ProviderKind::Ollama => Self::ollama(),
            ProviderKind::DefaultSearch | ProviderKind::GoogleSearch | ProviderKind::ChatGptWeb => {
                Err(WebError::LinkUnavailable)
            }
            ProviderKind::Unknown => Err(WebError::Protocol("unknown web provider".into())),
        }
    }

    fn new(kind: ProviderKind, bearer: Option<String>) -> Result<Self, WebError> {
        let endpoint = match kind {
            ProviderKind::Brave => "https://api.search.brave.com/res/v1/web/search".into(),
            ProviderKind::Ollama => "https://ollama.com/api/web_search".into(),
            ProviderKind::DefaultSearch | ProviderKind::GoogleSearch | ProviderKind::ChatGptWeb => {
                return Err(WebError::LinkUnavailable);
            }
            ProviderKind::Unknown => return Err(WebError::Protocol("unknown web provider".into())),
        };
        Self::new_with_endpoint(kind, endpoint, bearer)
    }

    fn new_with_endpoint(
        kind: ProviderKind,
        endpoint: String,
        bearer: Option<String>,
    ) -> Result<Self, WebError> {
        if bearer.as_deref().is_none_or(str::is_empty) {
            return Err(WebError::MissingCredential(match kind {
                ProviderKind::Brave => "Brave",
                _ => "Ollama",
            }));
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(45))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("Taceta/0.1 (+local web search)")
            .build()?;
        Ok(Self {
            kind,
            client,
            endpoint,
            bearer,
        })
    }

    pub fn kind(&self) -> ProviderKind {
        self.kind
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, WebError> {
        let limit = limit.clamp(1, MAX_RESULTS);
        let response = match self.kind {
            ProviderKind::Brave => {
                let mut endpoint = Url::parse(&self.endpoint).expect("valid Brave endpoint");
                endpoint
                    .query_pairs_mut()
                    .append_pair("q", query)
                    .append_pair("count", &limit.to_string());
                self.client
                    .get(endpoint)
                    .header(
                        "X-Subscription-Token",
                        self.bearer.as_deref().unwrap_or_default(),
                    )
                    .send()
                    .await
                    .map_err(|error| transport_error("search", &error))?
            }
            ProviderKind::Ollama | ProviderKind::Unknown => self
                .client
                .post(&self.endpoint)
                .bearer_auth(self.bearer.as_deref().unwrap_or_default())
                .json(&serde_json::json!({"query": query, "max_results": limit}))
                .send()
                .await
                .map_err(|error| transport_error("search", &error))?,
            ProviderKind::DefaultSearch | ProviderKind::GoogleSearch | ProviderKind::ChatGptWeb => {
                return Err(WebError::LinkUnavailable);
            }
        };
        let response = response
            .error_for_status()
            .map_err(|e| status_or_transport("search", &e))?;
        match self.kind {
            ProviderKind::Brave => Ok(response
                .json::<BraveResponse>()
                .await?
                .web
                .results
                .into_iter()
                .take(limit)
                .map(|r| SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet: r.description,
                })
                .collect()),
            ProviderKind::Ollama | ProviderKind::Unknown => Ok(response
                .json::<OllamaResponse>()
                .await?
                .results
                .into_iter()
                .take(limit)
                .map(|r| SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet: r.content,
                })
                .collect()),
            ProviderKind::DefaultSearch | ProviderKind::GoogleSearch | ProviderKind::ChatGptWeb => {
                Err(WebError::LinkUnavailable)
            }
        }
    }

    /// Fetches one page with manual redirect validation so every hop receives
    /// the same SSRF checks. Only http(s), public IPs and text-like content are allowed.
    pub async fn fetch(&self, url: &str) -> Result<FetchedPage, WebError> {
        let validated = validate_public_url(url)?;
        if matches!(
            self.kind,
            ProviderKind::DefaultSearch | ProviderKind::GoogleSearch | ProviderKind::ChatGptWeb
        ) {
            return Err(WebError::LinkUnavailable);
        }
        if self.kind == ProviderKind::Ollama {
            let response = self
                .client
                .post("https://ollama.com/api/web_fetch")
                .bearer_auth(self.bearer.as_deref().unwrap_or_default())
                .json(&serde_json::json!({"url": validated.as_str()}))
                .send()
                .await
                .map_err(|error| transport_error("fetch", &error))?;
            let response = response
                .error_for_status()
                .map_err(|e| status_or_transport("fetch", &e))?;
            let page: OllamaFetchResponse = response.json().await?;
            return Ok(FetchedPage {
                url: validated.to_string(),
                content_type: "text/html".into(),
                text: page.content,
            });
        }
        let mut current = validated;
        for _ in 0..=MAX_REDIRECTS {
            let response = self
                .client
                .get(current.clone())
                .send()
                .await
                .map_err(|error| transport_error("fetch", &error))?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| WebError::Protocol("redirect without Location".into()))?;
                current = validate_public_url(
                    current
                        .join(location)
                        .map_err(|_| WebError::UnsafeUrl(location.into()))?
                        .as_str(),
                )?;
                continue;
            }
            let response = response
                .error_for_status()
                .map_err(|e| status_or_transport("fetch", &e))?;
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !content_type.is_empty()
                && ![
                    "text/html",
                    "text/plain",
                    "application/xhtml+xml",
                    "application/json",
                ]
                .iter()
                .any(|v| content_type.starts_with(v))
            {
                return Err(WebError::Protocol("unsupported content type".into()));
            }
            let bytes = response.bytes().await?;
            if bytes.len() > MAX_BODY {
                return Err(WebError::Protocol("response exceeds 1 MiB".into()));
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            return Ok(FetchedPage {
                url: current.to_string(),
                content_type,
                text: strip_html(&text),
            });
        }
        Err(WebError::Protocol("too many redirects".into()))
    }
}

/// Stores a provider token in the user's macOS login Keychain. The secret is
/// passed directly to `security` on stdin, never exposed in process arguments.
pub fn save_keychain_secret(account: &str, secret: &str) -> Result<(), WebError> {
    if !matches!(account, "brave" | "ollama") || secret.is_empty() {
        return Err(WebError::Keychain);
    }
    let mut child = Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            account,
            "-w",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| WebError::Keychain)?;
    use std::io::Write;
    child
        .stdin
        .take()
        .ok_or(WebError::Keychain)?
        .write_all(secret.as_bytes())
        .map_err(|_| WebError::Keychain)?;
    let status = child.wait().map_err(|_| WebError::Keychain)?;
    status.success().then_some(()).ok_or(WebError::Keychain)
}

pub fn delete_keychain_secret(account: &str) -> Result<(), WebError> {
    if !matches!(account, "brave" | "ollama") {
        return Err(WebError::Keychain);
    }
    let status = Command::new("/usr/bin/security")
        .args([
            "delete-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            account,
        ])
        .status()
        .map_err(|_| WebError::Keychain)?;
    if status.success() || status.code() == Some(44) {
        Ok(())
    } else {
        Err(WebError::Keychain)
    }
}

pub fn has_keychain_secret(account: &str) -> bool {
    let account = match account {
        "brave" => "brave",
        "ollama" => "ollama",
        _ => return false,
    };
    keychain_secret(account).is_some()
}

/// Validates a URL before it is handed to a network-capable web provider or
/// browser workflow.  It rejects non-web schemes, embedded credentials, and
/// addresses that resolve to local or private networks.
pub fn validate_public_url(raw: &str) -> Result<Url, WebError> {
    if raw.len() > MAX_URL {
        return Err(WebError::UnsafeUrl("URL too long".into()));
    }
    let url = Url::parse(raw).map_err(|_| WebError::UnsafeUrl(raw.into()))?;
    if !matches!(url.scheme(), "http" | "https") || url.username() != "" || url.password().is_some()
    {
        return Err(WebError::UnsafeUrl(raw.into()));
    }
    let host = url
        .host_str()
        .ok_or_else(|| WebError::UnsafeUrl(raw.into()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| WebError::UnsafeUrl(raw.into()))?;
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|_| WebError::UnsafeUrl(raw.into()))?;
    if addrs.map(|a| a.ip()).any(is_private_or_local) {
        return Err(WebError::UnsafeUrl(raw.into()));
    }
    Ok(url)
}

fn transport_error(stage: &'static str, error: &reqwest::Error) -> WebError {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else {
        "network"
    };
    WebError::Transport { stage, kind }
}

fn status_or_transport(stage: &'static str, error: &reqwest::Error) -> WebError {
    error
        .status()
        .map(WebError::Http)
        .unwrap_or_else(|| transport_error(stage, error))
}

fn is_private_or_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_unspecified()
                || v.octets()[0] == 169 && v.octets()[1] == 254
        }
        IpAddr::V6(v) => {
            v.is_loopback()
                || v.is_unspecified()
                || (v.segments()[0] & 0xfe00) == 0xfc00
                || (v.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn keychain_secret(account: &'static str) -> Option<String> {
    Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            account,
            "-w",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let value = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            (!value.is_empty()).then_some(value)
        })
}

fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut tag = false;
    for c in input.chars() {
        match c {
            '<' => tag = true,
            '>' => tag = false,
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Deserialize)]
struct BraveResponse {
    web: BraveWeb,
}
#[derive(Deserialize)]
struct BraveWeb {
    results: Vec<BraveResult>,
}
#[derive(Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    #[serde(default)]
    description: String,
}
#[derive(Deserialize)]
struct OllamaResponse {
    results: Vec<OllamaResult>,
}
#[derive(Deserialize)]
struct OllamaResult {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
}
#[derive(Deserialize)]
struct OllamaFetchResponse {
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_ssrf_targets() {
        assert!(validate_public_url("http://127.0.0.1/").is_err());
        assert!(validate_public_url("http://localhost/").is_err());
        assert!(validate_public_url("file:///tmp/a").is_err());
        assert!(validate_public_url("http://user:pass@example.com/").is_err());
    }
    #[test]
    fn clamps_result_limit() {
        assert_eq!(MAX_RESULTS, 5);
    }

    #[test]
    fn exposes_only_the_two_declared_web_tools() {
        let tools = tool_definitions();
        assert_eq!(tools.as_array().unwrap().len(), 2);
        assert_eq!(tools[0]["function"]["name"], "web_search");
        assert_eq!(tools[1]["function"]["name"], "web_fetch");
    }

    #[test]
    fn mandatory_search_bypass_covers_only_explicit_search_commands() {
        assert!(requires_mandatory_search(
            "喋らずに WEBサーチを起動しろ。WEBサーチを実行してください。"
        ));
        assert!(requires_mandatory_search(
            "Please search the web and show me the official URL."
        ));
        assert!(!requires_mandatory_search(
            "2026年9月3日時点で最も注目されているモデルを選び、出典元URLを教えてください。"
        ));
        assert!(!requires_mandatory_search(
            "最新のQwenモデルを教えてください"
        ));
        assert!(!requires_mandatory_search(
            "今日リリースされたOAIのGPT-6はAGIですか？"
        ));
    }

    #[test]
    fn generated_question_search_waits_for_the_local_model_query() {
        assert!(requests_model_generated_search_query(
            "何か質問作ってWEBサーチしてみてよ"
        ));
        assert!(!requires_mandatory_search(
            "何か質問作ってWEBサーチしてみてよ"
        ));
        assert!(requests_model_generated_search_query(
            "Come up with a question and search the web."
        ));
    }

    #[test]
    fn mandatory_search_intent_does_not_capture_ordinary_web_discussion() {
        assert!(!requires_mandatory_search(
            "WEBサーチのトリガーってどうなっていたっけか"
        ));
        assert!(!requires_mandatory_search(
            "通常会話とWEBサーチ意図のある会話くらいは切り分けろよ"
        ));
        assert!(!requires_mandatory_search(
            "会話をすべてWEBサーチに送ったら収拾がつかなくなる"
        ));
        assert!(!requires_mandatory_search("今日は雑談しよう"));
        assert!(!requires_mandatory_search("このコードを説明して"));
    }

    #[test]
    fn assistant_search_intent_requires_an_immediate_action_statement() {
        assert!(expresses_immediate_search_intent(
            "承知しました。WEBサーチを実行します。"
        ));
        assert!(expresses_immediate_search_intent(
            "では、インターネットで調べます。"
        ));
        assert!(expresses_immediate_search_intent(
            "I'll search the web now."
        ));
        assert!(!expresses_immediate_search_intent(
            "WEBサーチの仕組みを説明します。"
        ));
        assert!(!expresses_immediate_search_intent(
            "必要なら検索しましょうか？"
        ));
        assert!(!expresses_immediate_search_intent("先ほど検索しました。"));
        assert!(!expresses_immediate_search_intent(
            "WEB検索は実行しません。"
        ));
    }

    #[test]
    fn assistant_generated_question_becomes_the_search_query() {
        assert_eq!(
            concrete_search_query_from_assistant(
                "では別の質問です。『2026年に注目されるオープンソースAIモデルは何ですか？』をWeb検索します。"
            )
            .as_deref(),
            Some("2026年に注目されるオープンソースAIモデルは何ですか？")
        );
        assert_eq!(
            concrete_search_query_from_assistant(
                "Question: Which open-source AI model is drawing attention in 2026? I'll search the web."
            )
            .as_deref(),
            Some("Which open-source AI model is drawing attention in 2026?")
        );
        assert_eq!(
            concrete_search_query_from_assistant("承知しました。WEBサーチを実行します。"),
            None
        );
    }

    #[test]
    fn browser_workflows_never_open_a_direct_provider() {
        for kind in [
            ProviderKind::DefaultSearch,
            ProviderKind::GoogleSearch,
            ProviderKind::ChatGptWeb,
            ProviderKind::Unknown,
        ] {
            assert!(matches!(
                WebSearchProvider::from_kind(kind),
                Err(WebError::LinkUnavailable | WebError::Protocol(_))
            ));
        }
    }

    #[test]
    fn parses_ollama_string_arguments_without_accepting_other_tools() {
        let call = parse_tool_call(&serde_json::json!({"function":{"name":"web_search","arguments":"{\"query\":\"rust\"}"}})).unwrap();
        assert_eq!(call.name, "web_search");
        assert_eq!(call.arguments["query"], "rust");
        assert!(
            parse_tool_call(&serde_json::json!({"function":{"name":"shell","arguments":{}}}))
                .is_err()
        );
    }
}
