use super::transport::Connection;
use crate::domain::{ModelDescriptor, ReasoningEffort, ThinkingCapability, ThinkingLevel, ThinkingMode};
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

fn known_effort(value: &str) -> Option<ReasoningEffort> {
    serde_json::from_value(json!(value)).ok()
}

impl CatalogModel {
    pub fn descriptor(&self) -> ModelDescriptor {
        let mut supported = Vec::new();
        for option in &self.supported_reasoning_efforts {
            if let Some(effort) = known_effort(&option.reasoning_effort) {
                if !supported.contains(&effort) { supported.push(effort); }
            }
        }
        let default = self.default_reasoning_effort.as_deref().and_then(known_effort)
            .filter(|effort| supported.contains(effort));
        ModelDescriptor {
            name: self.model.clone(), size: 0,
            thinking: ThinkingCapability::Efforts { supported, default },
            vision: self.vision(), tools: true, context_length: None,
        }
    }

    pub fn vision(&self) -> bool { self.input_modalities.iter().any(|value| value == "image") }

    pub fn effort(&self, thinking: ThinkingMode) -> Result<Option<String>, String> {
        let requested = match thinking {
            ThinkingMode::Default => return Ok(None),
            ThinkingMode::On => self.default_reasoning_effort.as_deref()
                .ok_or("This GPT model did not advertise a default reasoning effort.")?,
            ThinkingMode::Off => "none",
            ThinkingMode::Level(ThinkingLevel::Low) => "low",
            ThinkingMode::Level(ThinkingLevel::Medium) => "medium",
            ThinkingMode::Level(ThinkingLevel::High) => "high",
            ThinkingMode::Effort(effort) => effort.as_str(),
        };
        if !self.supported_reasoning_efforts.iter().any(|option| option.reasoning_effort == requested) {
            return Err(format!("{} does not advertise reasoning effort '{requested}'. Select an available effort.", self.model));
        }
        Ok(Some(requested.into()))
    }
}

pub(super) async fn account(connection: &mut Connection) -> Result<Option<Value>, String> {
    let response = connection.call("account/read", json!({"refreshToken":false})).await?;
    let account = response.get("account").ok_or("Codex account/read response has no account field.")?;
    if account.is_null() { return Ok(None); }
    if account.get("type").and_then(Value::as_str) != Some("chatgpt") {
        return Err("Taceta GPT requires ChatGPT OAuth. The isolated Codex account uses another authentication type.".into());
    }
    Ok(Some(account.clone()))
}

pub(super) async fn require_account(connection: &mut Connection) -> Result<(), String> {
    if account(connection).await?.is_none() {
        return Err("GPT is signed out. Use the GPT sign-in button to connect your ChatGPT account.".into());
    }
    Ok(())
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
    if models.is_empty() { return Err("No GPT models are available for this ChatGPT account.".into()); }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex, split};

    #[test]
    fn gpt_catalog_never_invents_reasoning_vision_or_context() {
        let model: CatalogModel = serde_json::from_value(json!({
            "model":"account-model","supportedReasoningEfforts":[{"reasoningEffort":"high"},{"reasoningEffort":"ultra"}],
            "defaultReasoningEffort":"high","inputModalities":["text"]
        })).unwrap();
        let descriptor = model.descriptor();
        assert!(!descriptor.vision);
        assert_eq!(descriptor.context_length, None);
        assert_eq!(model.effort(ThinkingMode::Off), Err("account-model does not advertise reasoning effort 'none'. Select an available effort.".into()));
        assert_eq!(model.effort(ThinkingMode::Effort(ReasoningEffort::Ultra)).unwrap(), Some("ultra".into()));
        assert_eq!(model.effort(ThinkingMode::Default).unwrap(), None);
    }

    #[tokio::test]
    async fn gpt_catalog_paginates_and_rejects_non_oauth_accounts() {
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
                    _ => json!({"account":{"type":"apiKey"}}),
                };
                write.write_all(format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes()).await.unwrap();
            }
        });
        assert_eq!(models(&mut connection).await.unwrap().iter().map(|model| model.model.as_str()).collect::<Vec<_>>(), vec!["one", "two"]);
        assert!(require_account(&mut connection).await.unwrap_err().contains("ChatGPT OAuth"));
        peer.await.unwrap();
    }
}
