use super::api::ChatChunk;
use crate::backend::BackendError;
use crate::domain::{GenerationEvent, GenerationStats};
use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

pub(super) async fn consume<S, E>(
    mut stream: S,
    events: UnboundedSender<GenerationEvent>,
) -> Result<(), BackendError>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut buffer = Vec::new();
    let mut stats = GenerationStats {
        prompt_tokens: None,
        completion_tokens: None,
        total_duration_ns: None,
    };
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| BackendError::Protocol(e.to_string()))?;
        buffer.extend_from_slice(&bytes);
        while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=pos).collect();
            let line = line[..line.len() - 1]
                .strip_suffix(&[b'\r'])
                .unwrap_or(&line[..line.len() - 1]);
            if line.is_empty() {
                continue;
            }
            let item: ChatChunk =
                serde_json::from_slice(line).map_err(|e| BackendError::Protocol(e.to_string()))?;
            if let Some(error) = item.error {
                return Err(BackendError::Protocol(error));
            }
            if let Some(message) = item.message {
                if !message.thinking.is_empty() {
                    let _ = events.send(GenerationEvent::ThinkingDelta(message.thinking));
                }
                if !message.content.is_empty() {
                    let _ = events.send(GenerationEvent::ContentDelta(message.content));
                }
            }
            if item.done {
                stats.prompt_tokens = item.prompt_eval_count;
                stats.completion_tokens = item.eval_count;
                stats.total_duration_ns = item.total_duration;
                let _ = events.send(GenerationEvent::Completed(stats.clone()));
                return Ok(());
            }
        }
    }
    if !buffer.iter().all(u8::is_ascii_whitespace) {
        let item: ChatChunk =
            serde_json::from_slice(&buffer).map_err(|e| BackendError::Protocol(e.to_string()))?;
        if let Some(error) = item.error {
            return Err(BackendError::Protocol(error));
        }
        if let Some(message) = item.message {
            if !message.thinking.is_empty() {
                let _ = events.send(GenerationEvent::ThinkingDelta(message.thinking));
            }
            if !message.content.is_empty() {
                let _ = events.send(GenerationEvent::ContentDelta(message.content));
            }
        }
        if item.done {
            stats.prompt_tokens = item.prompt_eval_count;
            stats.completion_tokens = item.eval_count;
            stats.total_duration_ns = item.total_duration;
            let _ = events.send(GenerationEvent::Completed(stats));
            return Ok(());
        }
    }
    Err(BackendError::Protocol(
        "stream ended before completion".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    #[tokio::test]
    async fn parses_arbitrary_chunk_boundaries_and_separates_fields() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let input = br#"{"message":{"thinking":"abc","content":""},"done":false}
{"message":{"thinking":"","content":"answer"},"done":true,"eval_count":2}"#;
        let pieces = input
            .chunks(7)
            .map(|x| Ok::<_, std::convert::Infallible>(bytes::Bytes::copy_from_slice(x)));
        consume(stream::iter(pieces), tx).await.unwrap();
        assert_eq!(
            rx.recv().await,
            Some(GenerationEvent::ThinkingDelta("abc".into()))
        );
        assert_eq!(
            rx.recv().await,
            Some(GenerationEvent::ContentDelta("answer".into()))
        );
        assert!(
            matches!(rx.recv().await, Some(GenerationEvent::Completed(s)) if s.completion_tokens == Some(2))
        );
    }
}
