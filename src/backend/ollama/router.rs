use super::api::{ChatBody, ChatChunk, ChatOptions, WireMessage};
use crate::backend::BackendError;
use serde::Deserialize;

const WEB_ROUTER_PROMPT: &str = r#"You are Taceta's local Web-routing controller, not an answering assistant.
Web is ON: external research is mandatory for every input, including explanations, writing, and casual questions. Choose a search query only for the CURRENT USER INPUT below. You have no conversation history and must not answer, refuse, correct, or assess whether a named thing exists. Never decide that internal knowledge is sufficient.

Choose exactly one action:
- search_current: search the current user's question. This is the default for every input. Use this even when a claim seems false, impossible, unknown, or newer than your training; searching is how the premise is checked.
- search_generated: the user asks the local model to invent, choose, or formulate a concrete question/topic and then search it. Put that new self-contained research question in query.

If deciding whether a name, product, release, event, claim, date, or premise is real would require external verification, choose search_current. A premise that conflicts with your training is evidence that verification is needed, never a reason to refuse or skip research.

Examples:
- "AGIの定義ってなんだっけ" -> search_current
- "今日リリースされたOAIのGPT-6はAGIですか？" -> search_current, even if you believe GPT-6 does not exist
- "何か質問を作ってWeb検索して" -> search_generated

For search_current, put a concise self-contained search query in query. Never use stale factual knowledge to skip research. Return only the required JSON object."#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WebRouteDecision {
    Local,
    SearchCurrent { query: String },
    SearchGenerated { query: String },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RouteAction {
    SearchCurrent,
    SearchGenerated,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutePayload {
    action: RouteAction,
    query: String,
}

pub(super) async fn classify(
    http: &reqwest::Client,
    chat_url: String,
    model: &str,
    current_input: &str,
    context_length: u32,
) -> Result<WebRouteDecision, BackendError> {
    let body = routing_body(model, current_input, context_length);
    let response = http
        .post(chat_url)
        .json(&body)
        .send()
        .await
        .map_err(routing_request_error)?
        .error_for_status()
        .map_err(routing_request_error)?;
    let response: ChatChunk = response.json().await.map_err(routing_request_error)?;
    if let Some(error) = response.error {
        return Err(BackendError::Protocol(format!(
            "web routing failed: {error}"
        )));
    }
    let content = response
        .message
        .map(|message| message.content)
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| BackendError::Protocol("web routing returned no decision".into()))?;
    parse_decision(&content)
}

fn routing_request_error(error: reqwest::Error) -> BackendError {
    BackendError::Protocol(format!("web routing request failed: {error}"))
}

fn routing_body(model: &str, current_input: &str, context_length: u32) -> ChatBody {
    ChatBody {
        model: model.to_owned(),
        messages: vec![
            WireMessage {
                role: "system".into(),
                content: WEB_ROUTER_PROMPT.into(),
                images: Vec::new(),
                tool_calls: None,
                tool_name: None,
            },
            WireMessage {
                role: "user".into(),
                content: current_input.to_owned(),
                images: Vec::new(),
                tool_calls: None,
                tool_name: None,
            },
        ],
        stream: false,
        options: ChatOptions::web_routing(context_length),
        think: Some(false.into()),
        tools: None,
        format: Some(route_schema()),
    }
}

fn route_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["search_current", "search_generated"]
            },
            "query": {"type": "string", "maxLength": 512}
        },
        "required": ["action", "query"],
        "additionalProperties": false
    })
}

fn parse_decision(content: &str) -> Result<WebRouteDecision, BackendError> {
    let payload: RoutePayload = serde_json::from_str(content.trim()).map_err(|_| {
        BackendError::Protocol(
            "web routing returned invalid JSON; nothing was sent to the browser".into(),
        )
    })?;
    match payload.action {
        RouteAction::SearchCurrent => {
            valid_query(payload.query).map(|query| WebRouteDecision::SearchCurrent { query })
        }
        RouteAction::SearchGenerated => {
            valid_query(payload.query).map(|query| WebRouteDecision::SearchGenerated { query })
        }
    }
}

fn valid_query(query: String) -> Result<String, BackendError> {
    let query = query.trim();
    if !(4..=512).contains(&query.chars().count()) {
        return Err(BackendError::Protocol(
            "web routing returned an invalid search query; nothing was sent to the browser".into(),
        ));
    }
    Ok(query.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_search_routes_when_web_is_on() {
        assert!(parse_decision(r#"{"action":"local","query":""}"#).is_err());
        assert_eq!(
            parse_decision(
                r#"{"action":"search_current","query":"OpenAI GPT-6 release today AGI"}"#
            )
            .unwrap(),
            WebRouteDecision::SearchCurrent {
                query: "OpenAI GPT-6 release today AGI".into()
            }
        );
        assert_eq!(
            parse_decision(
                r#"{"action":"search_generated","query":"Which open-source AI model is most discussed in 2026?"}"#
            )
            .unwrap(),
            WebRouteDecision::SearchGenerated {
                query: "Which open-source AI model is most discussed in 2026?".into()
            }
        );
    }

    #[test]
    fn refusal_or_explanatory_text_never_silently_becomes_local() {
        assert!(parse_decision("GPT-6 does not exist, so I will not search.").is_err());
        assert!(parse_decision(r#"{"action":"local","query":"latest GPT release"}"#).is_err());
        assert!(parse_decision(r#"{"action":"search_current","query":""}"#).is_err());
    }

    #[test]
    fn routing_contract_treats_a_dubious_new_claim_as_a_reason_to_search() {
        assert!(WEB_ROUTER_PROMPT.contains("even when a claim seems false"));
        assert!(WEB_ROUTER_PROMPT.contains("must not answer, refuse, correct"));
        assert!(WEB_ROUTER_PROMPT.contains("external research is mandatory for every input"));
        assert_eq!(route_schema()["properties"]["action"]["enum"], serde_json::json!(["search_current", "search_generated"]));
        assert_eq!(route_schema()["additionalProperties"], false);
    }

    #[test]
    fn routing_request_contains_only_the_control_prompt_and_current_input() {
        let body = routing_body(
            "qwen3.8:27b-mlx",
            "今日リリースされたOAIのGPT-6はAGIですか？",
            32_768,
        );
        assert_eq!(body.messages.len(), 2);
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(body.messages[1].role, "user");
        assert_eq!(
            body.messages[1].content,
            "今日リリースされたOAIのGPT-6はAGIですか？"
        );
        assert_eq!(body.think, Some(false.into()));
        assert!(body.tools.is_none());
        assert!(body.format.is_some());
    }
}
