//! The web tools as Ollama's chat API declares and returns them. Nothing
//! outside this adapter sees these shapes.

use crate::backend::BackendError;

/// The web tools Chat and Code both offer. Page fetching is left out when
/// no call to it can succeed, since an offered tool that always fails ends
/// the turn.
pub(super) fn tools(page_fetch_available: bool) -> Vec<crate::agent::ToolDefinition> {
    use crate::agent::ToolDefinition;
    let search = ToolDefinition {
        name: "web_search".into(),
        description: if page_fetch_available {
            "Search the web and return up to five sources. For factual answers, inspect one to five relevant returned sources with web_fetch before synthesizing the answer."
        } else {
            "Search the web and return up to five sources."
        }
        .into(),
        parameters: serde_json::json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":5}},"required":["query"]}),
    };
    let fetch = ToolDefinition {
        name: "web_fetch".into(),
        description: "Fetch readable text from one public HTTPS URL. After web_search, fetch one to five relevant independent sources before synthesizing a factual answer.".into(),
        parameters: serde_json::json!({"type":"object","properties":{"url":{"type":"string","maxLength":2048}},"required":["url"]}),
    };
    if page_fetch_available { vec![search, fetch] } else { vec![search] }
}

/// [`tools`] as Ollama's chat API declares them.
pub(super) fn definitions(page_fetch_available: bool) -> serde_json::Value {
    serde_json::Value::Array(
        tools(page_fetch_available)
            .into_iter()
            .map(|tool| serde_json::json!({"type":"function","function":{
                "name": tool.name, "description": tool.description, "parameters": tool.parameters
            }}))
            .collect(),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

pub(super) fn parse_call(value: &serde_json::Value) -> Result<ToolCall, BackendError> {
    let function = value
        .get("function")
        .ok_or_else(|| BackendError::Web("tool call missing function".into()))?;
    let name = function
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| BackendError::Web("tool call missing name".into()))?;
    if !matches!(name, "web_search" | "web_fetch") {
        return Err(BackendError::Web("unsupported web tool".into()));
    }
    let arguments = function
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let arguments = if arguments.is_string() {
        serde_json::from_str(arguments.as_str().unwrap())
            .map_err(|_| BackendError::Web("tool arguments are not JSON".into()))?
    } else {
        arguments
    };
    Ok(ToolCall {
        name: name.into(),
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_only_the_two_declared_web_tools() {
        let tools = definitions(true);
        assert_eq!(tools.as_array().unwrap().len(), 2);
        assert_eq!(tools[0]["function"]["name"], "web_search");
        assert_eq!(tools[1]["function"]["name"], "web_fetch");
    }

    #[test]
    fn parses_ollama_string_arguments_without_accepting_other_tools() {
        let call = parse_call(&serde_json::json!({"function":{"name":"web_search","arguments":"{\"query\":\"rust\"}"}})).unwrap();
        assert_eq!(call.name, "web_search");
        assert_eq!(call.arguments["query"], "rust");
        assert!(
            parse_call(&serde_json::json!({"function":{"name":"shell","arguments":{}}}))
                .is_err()
        );
    }
}
