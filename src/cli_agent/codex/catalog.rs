use super::{super::ModelOption, transport::Connection};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CatalogModel {
    pub model: String,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    supported_reasoning_efforts: Vec<EffortOption>,
    #[serde(default)]
    default_reasoning_effort: Option<String>,
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffortOption { reasoning_effort: String }

impl CatalogModel {
    /// Only what the catalog states: no effort, image support or context
    /// size is assumed for a model that does not advertise it.
    pub fn option(&self) -> ModelOption {
        let mut efforts: Vec<String> = Vec::new();
        for option in &self.supported_reasoning_efforts {
            if !efforts.contains(&option.reasoning_effort) { efforts.push(option.reasoning_effort.clone()); }
        }
        let default_effort = self.default_reasoning_effort.clone().filter(|effort| efforts.contains(effort));
        ModelOption {
            id: self.model.clone(), label: self.model.clone(), context_window: None,
            efforts, default_effort, images: self.images(),
        }
    }

    pub fn images(&self) -> bool { self.input_modalities.iter().any(|value| value == "image") }

    pub fn supports_effort(&self, effort: &str) -> bool {
        self.supported_reasoning_efforts.iter().any(|option| option.reasoning_effort == effort)
    }
}

/// Whether the CLI is signed in. Only that is read; the account itself is
/// never kept.
pub(super) async fn signed_in(connection: &mut Connection) -> Result<bool, String> {
    let response = connection.call("account/read", json!({"refreshToken":false})).await?;
    let account = response.get("account").ok_or("Codex account/read response has no account field.")?;
    Ok(!account.is_null())
}

pub(super) const SIGNED_OUT: &str = "The Codex CLI is not signed in. Run `codex login` in a terminal, then try again.";

pub(super) async fn require_account(connection: &mut Connection) -> Result<(), String> {
    if signed_in(connection).await? { Ok(()) } else { Err(SIGNED_OUT.into()) }
}

pub(super) async fn models(connection: &mut Connection) -> Result<Vec<CatalogModel>, String> {
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut names = HashSet::new();
    let mut models = Vec::new();
    loop {
        let response = connection.call("model/list", json!({"includeHidden":false,"cursor":cursor})).await?;
        let data = response.get("data").and_then(Value::as_array).ok_or("Codex model/list response has no model catalog.")?;
        for value in data {
            let model: CatalogModel = serde_json::from_value(value.clone())
                .map_err(|_| "Codex returned an invalid model catalog entry.")?;
            if model.model.trim().is_empty() { return Err("Codex returned an empty model identifier.".into()); }
            if !model.hidden && names.insert(model.model.clone()) { models.push(model); }
        }
        cursor = match response.get("nextCursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(cursor)) if !cursor.is_empty() => Some(cursor.clone()),
            _ => return Err("Codex returned an invalid model catalog cursor.".into()),
        };
        let Some(next) = &cursor else { break; };
        if !cursors.insert(next.clone()) { return Err("Codex repeated a model catalog cursor.".into()); }
    }
    if models.is_empty() { return Err("No GPT models are available for this account.".into()); }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex, split};

    #[test]
    fn a_catalog_entry_offers_only_what_it_advertises() {
        let model: CatalogModel = serde_json::from_value(json!({
            "model":"account-model","supportedReasoningEfforts":[{"reasoningEffort":"high"},{"reasoningEffort":"ultra"}],
            "defaultReasoningEffort":"high","inputModalities":["text"]
        })).unwrap();
        let option = model.option();
        assert!(!option.images);
        assert_eq!(option.context_window, None);
        assert_eq!(option.efforts, ["high", "ultra"]);
        assert_eq!(option.default_effort.as_deref(), Some("high"));
        assert!(model.supports_effort("ultra") && !model.supports_effort("none"));
    }

    #[tokio::test]
    async fn the_catalog_is_read_page_by_page_and_a_missing_account_is_reported() {
        let (client, server) = duplex(8192);
        let (read, write) = split(client);
        let mut connection = Connection::from_io(read, write);
        let peer = tokio::spawn(async move {
            let (read, mut write) = split(server);
            let mut read = BufReader::new(read);
            for index in 0..3 {
                let mut line = String::new(); read.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = match index {
                    0 => { assert!(request["params"]["cursor"].is_null()); json!({"data":[{"model":"one"}],"nextCursor":"next"}) },
                    1 => { assert_eq!(request["params"]["cursor"], "next"); json!({"data":[{"model":"two"}],"nextCursor":null}) },
                    _ => json!({"account":null}),
                };
                write.write_all(format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes()).await.unwrap();
            }
        });
        assert_eq!(models(&mut connection).await.unwrap().iter().map(|model| model.model.as_str()).collect::<Vec<_>>(), vec!["one", "two"]);
        assert_eq!(require_account(&mut connection).await.unwrap_err(), SIGNED_OUT);
        peer.await.unwrap();
    }
}
