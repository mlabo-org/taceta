//! Which models Ollama holds in memory, and releasing them.

use super::endpoint::OllamaEndpoint;
use crate::backend::BackendError;
use serde::Deserialize;
use std::{collections::HashSet, time::Duration};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RELEASE_DEADLINE: Duration = Duration::from_secs(30);

fn url(endpoint: &OllamaEndpoint, path: &str) -> String {
    format!("{}{path}", endpoint.base_url())
}

/// Names of the models currently loaded, without duplicates.
pub(super) async fn loaded(
    http: &reqwest::Client,
    endpoint: &OllamaEndpoint,
) -> Result<Vec<String>, BackendError> {
    #[derive(Deserialize)]
    struct RunningModels {
        models: Vec<RunningModel>,
    }
    #[derive(Deserialize)]
    struct RunningModel {
        name: String,
    }
    let running: RunningModels = http
        .get(url(endpoint, "/api/ps"))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut seen = HashSet::new();
    Ok(running
        .models
        .into_iter()
        .map(|model| model.name)
        .filter(|name| seen.insert(name.clone()))
        .collect())
}

/// Asks Ollama to unload each model, then waits until none of them is listed
/// as loaded. A model that cannot be released is an error, never a silent skip.
/// Returns the models that are loaded once the release is complete.
pub(super) async fn release(
    http: &reqwest::Client,
    endpoint: &OllamaEndpoint,
    models: &[String],
) -> Result<Vec<String>, BackendError> {
    if models.is_empty() {
        return Ok(Vec::new());
    }
    tokio::time::timeout(RELEASE_DEADLINE, async {
        let mut errors = Vec::new();
        for model in models {
            if let Err(error) = request_unload(http, endpoint, model).await {
                errors.push(format!("{model}: {error}"));
            }
        }
        if !errors.is_empty() {
            return Err(BackendError::Protocol(format!(
                "Some models could not be unloaded: {}",
                errors.join("; ")
            )));
        }
        loop {
            let running = loaded(http, endpoint).await?;
            if !running.iter().any(|name| models.contains(name)) {
                return Ok(running);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| BackendError::Protocol("model unload timed out after 30 seconds".into()))?
}

async fn request_unload(
    http: &reqwest::Client,
    endpoint: &OllamaEndpoint,
    model: &str,
) -> Result<(), BackendError> {
    let response = http
        .post(url(endpoint, "/api/generate"))
        .timeout(REQUEST_TIMEOUT)
        .json(&serde_json::json!({"model": model, "keep_alive": 0, "stream": false}))
        .send()
        .await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await?;
    if !status.is_success() {
        return Err(BackendError::Protocol(format!(
            "{status}: {}",
            body.get("error").and_then(|value| value.as_str()).unwrap_or("unknown error")
        )));
    }
    if body.get("done").and_then(|value| value.as_bool()) != Some(true) {
        return Err(BackendError::Protocol("model unload was not acknowledged".into()));
    }
    Ok(())
}

/// Frees the memory of every loaded model other than `keep` before `keep` is
/// used, so switching models never holds two of them at once. Returns the
/// models that were released.
pub(super) async fn release_all_but(
    http: &reqwest::Client,
    endpoint: &OllamaEndpoint,
    keep: &str,
) -> Result<Vec<String>, BackendError> {
    let others: Vec<String> = loaded(http, endpoint)
        .await?
        .into_iter()
        .filter(|name| name != keep)
        .collect();
    release(http, endpoint, &others).await?;
    Ok(others)
}
