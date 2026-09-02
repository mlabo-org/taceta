use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    process::Command,
};

use serde::{Deserialize, Serialize};
use url::{Host, Url};

pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OllamaEndpointMode {
    #[default]
    Auto,
    Custom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OllamaEndpointSource {
    Custom,
    LaunchctlEnvironment,
    ProcessEnvironment,
    Default,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OllamaEndpoint {
    base_url: String,
    source: OllamaEndpointSource,
    allow_auto_start: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum OllamaEndpointError {
    #[error("a custom Ollama endpoint is required")]
    MissingCustomEndpoint,
    #[error("invalid Ollama endpoint from {origin}: {reason}")]
    InvalidEndpoint {
        origin: &'static str,
        reason: String,
    },
}

impl OllamaEndpoint {
    pub fn resolve(
        mode: OllamaEndpointMode,
        custom_endpoint: &str,
    ) -> Result<Self, OllamaEndpointError> {
        let launchctl = launchctl_environment_value("OLLAMA_HOST");
        let process = std::env::var("OLLAMA_HOST")
            .ok()
            .filter(|value| !value.trim().is_empty());
        Self::resolve_from_values(
            mode,
            custom_endpoint,
            launchctl.as_deref(),
            process.as_deref(),
        )
    }

    pub fn resolve_from_values(
        mode: OllamaEndpointMode,
        custom_endpoint: &str,
        launchctl_value: Option<&str>,
        process_value: Option<&str>,
    ) -> Result<Self, OllamaEndpointError> {
        match mode {
            OllamaEndpointMode::Custom => {
                let value = custom_endpoint.trim();
                if value.is_empty() {
                    return Err(OllamaEndpointError::MissingCustomEndpoint);
                }
                Self::from_value(value, OllamaEndpointSource::Custom, "manual setting", false)
            }
            OllamaEndpointMode::Auto => {
                if let Some(value) = launchctl_value.filter(|value| !value.trim().is_empty()) {
                    return Self::from_value(
                        value,
                        OllamaEndpointSource::LaunchctlEnvironment,
                        "launchctl OLLAMA_HOST",
                        false,
                    );
                }
                if let Some(value) = process_value.filter(|value| !value.trim().is_empty()) {
                    return Self::from_value(
                        value,
                        OllamaEndpointSource::ProcessEnvironment,
                        "process OLLAMA_HOST",
                        false,
                    );
                }
                Ok(Self::default_local())
            }
        }
    }

    pub fn default_local() -> Self {
        Self {
            base_url: DEFAULT_OLLAMA_BASE_URL.to_owned(),
            source: OllamaEndpointSource::Default,
            allow_auto_start: true,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn source(&self) -> OllamaEndpointSource {
        self.source
    }

    pub(super) fn allows_auto_start(&self) -> bool {
        self.allow_auto_start
    }

    fn from_value(
        value: &str,
        source: OllamaEndpointSource,
        source_name: &'static str,
        allow_auto_start: bool,
    ) -> Result<Self, OllamaEndpointError> {
        normalize_ollama_host(value)
            .map(|base_url| Self {
                base_url,
                source,
                allow_auto_start,
            })
            .map_err(|reason| OllamaEndpointError::InvalidEndpoint {
                origin: source_name,
                reason,
            })
    }
}

fn launchctl_environment_value(name: &str) -> Option<String> {
    let output = Command::new("/bin/launchctl")
        .args(["getenv", name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn normalize_ollama_host(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("the value is empty".to_owned());
    }

    let has_scheme = value.contains("://");
    let has_explicit_port = has_explicit_port(value);
    let candidate = if has_scheme {
        value.to_owned()
    } else if value.parse::<Ipv6Addr>().is_ok() {
        format!("http://[{value}]")
    } else if value.starts_with(':') {
        format!("http://127.0.0.1{value}")
    } else {
        format!("http://{value}")
    };
    let mut url = Url::parse(&candidate).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("only http and https endpoints are supported".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("credentials are not allowed in the endpoint".to_owned());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("query strings and fragments are not allowed".to_owned());
    }

    match url.host() {
        Some(Host::Ipv4(address)) if address == Ipv4Addr::UNSPECIFIED => url
            .set_ip_host(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .map_err(|_| "could not convert the wildcard IPv4 address".to_owned())?,
        Some(Host::Ipv6(address)) if address == Ipv6Addr::UNSPECIFIED => url
            .set_ip_host(IpAddr::V6(Ipv6Addr::LOCALHOST))
            .map_err(|_| "could not convert the wildcard IPv6 address".to_owned())?,
        Some(_) => {}
        None => return Err("a host is required".to_owned()),
    }

    if !has_explicit_port {
        let default_port = if has_scheme {
            match url.scheme() {
                "http" => 80,
                "https" => 443,
                _ => unreachable!(),
            }
        } else {
            11434
        };
        url.set_port(Some(default_port))
            .map_err(|_| "could not apply the default port".to_owned())?;
    }

    let normalized_path = url.path().trim_end_matches('/').to_owned();
    url.set_path(if normalized_path.is_empty() {
        "/"
    } else {
        &normalized_path
    });
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn has_explicit_port(value: &str) -> bool {
    let authority = value
        .split_once("://")
        .map_or(value, |(_, authority)| authority)
        .split('/')
        .next()
        .unwrap_or_default();
    if let Some(remainder) = authority.strip_prefix('[') {
        return remainder
            .split_once(']')
            .is_some_and(|(_, suffix)| suffix.starts_with(':') && suffix.len() > 1);
    }
    authority
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.contains(':') && !port.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_resolution_uses_the_default_only_without_configuration() {
        let endpoint =
            OllamaEndpoint::resolve_from_values(OllamaEndpointMode::Auto, "", None, None).unwrap();

        assert_eq!(endpoint.base_url(), DEFAULT_OLLAMA_BASE_URL);
        assert_eq!(endpoint.source(), OllamaEndpointSource::Default);
        assert!(endpoint.allows_auto_start());
    }

    #[test]
    fn launchctl_configuration_precedes_the_process_snapshot() {
        let endpoint = OllamaEndpoint::resolve_from_values(
            OllamaEndpointMode::Auto,
            "",
            Some("127.0.0.1:23456"),
            Some("127.0.0.1:34567"),
        )
        .unwrap();

        assert_eq!(endpoint.base_url(), "http://127.0.0.1:23456");
        assert_eq!(
            endpoint.source(),
            OllamaEndpointSource::LaunchctlEnvironment
        );
        assert!(!endpoint.allows_auto_start());
    }

    #[test]
    fn process_configuration_is_used_when_launchctl_has_no_value() {
        let endpoint = OllamaEndpoint::resolve_from_values(
            OllamaEndpointMode::Auto,
            "",
            None,
            Some("localhost:34567"),
        )
        .unwrap();

        assert_eq!(endpoint.base_url(), "http://localhost:34567");
        assert_eq!(endpoint.source(), OllamaEndpointSource::ProcessEnvironment);
    }

    #[test]
    fn an_explicit_http_default_port_is_not_replaced_by_ollamas_default() {
        let endpoint = OllamaEndpoint::resolve_from_values(
            OllamaEndpointMode::Auto,
            "",
            Some("localhost:80"),
            None,
        )
        .unwrap();

        assert_eq!(endpoint.base_url(), "http://localhost");
    }

    #[test]
    fn wildcard_bind_addresses_become_connectable_loopback_addresses() {
        let ipv4 = OllamaEndpoint::resolve_from_values(
            OllamaEndpointMode::Auto,
            "",
            Some("0.0.0.0:23456"),
            None,
        )
        .unwrap();
        let ipv6 = OllamaEndpoint::resolve_from_values(
            OllamaEndpointMode::Auto,
            "",
            Some("[::]:23456"),
            None,
        )
        .unwrap();

        assert_eq!(ipv4.base_url(), "http://127.0.0.1:23456");
        assert_eq!(ipv6.base_url(), "http://[::1]:23456");
    }

    #[test]
    fn an_unbracketed_ipv6_host_uses_ollamas_default_port() {
        let endpoint =
            OllamaEndpoint::resolve_from_values(OllamaEndpointMode::Auto, "", Some("::1"), None)
                .unwrap();

        assert_eq!(endpoint.base_url(), "http://[::1]:11434");
    }

    #[test]
    fn custom_endpoint_is_normalized_without_enabling_auto_start() {
        let endpoint = OllamaEndpoint::resolve_from_values(
            OllamaEndpointMode::Custom,
            "https://ollama.example:8443/prefix/",
            None,
            None,
        )
        .unwrap();

        assert_eq!(endpoint.base_url(), "https://ollama.example:8443/prefix");
        assert_eq!(endpoint.source(), OllamaEndpointSource::Custom);
        assert!(!endpoint.allows_auto_start());
    }

    #[test]
    fn invalid_configured_endpoint_does_not_silently_fall_back() {
        let error = OllamaEndpoint::resolve_from_values(
            OllamaEndpointMode::Auto,
            "",
            Some("ftp://127.0.0.1:11434"),
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("only http and https"));
    }
}
