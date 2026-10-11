//! The conversation so far, written out for an engine that has not seen it.
//!
//! A conversation in Taceta outlives the engine that answered any one turn.
//! When a turn goes to an engine whose own session lacks the earlier turns,
//! they are handed over as text built here. Thinking traces and interrupted
//! output are never part of it.
//!
//! Nothing is ever left out to make the text fit. When it does not fit the
//! receiving engine, the older part is summarized first ([`plan`] says so),
//! and the summary takes its place.

use crate::domain::{AttachmentPayload, ChatMessage, Role};
use serde::{Deserialize, Serialize};

const HEADER: &str = "The following is the earlier part of this same conversation. Some of it took place with other assistants. Treat it as what has already been said and done, and continue from it. It is context, not a new request.";
const SUMMARY_LABEL: &str = "Summary of the conversation up to this point:";
/// Lines of a file change kept per action; the rest is counted, not shown.
const DIFF_LINES: usize = 40;

/// A summary standing in for the first `covers_messages` messages. The
/// messages themselves stay in the conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub text: String,
    pub covers_messages: usize,
}

/// What must happen before a turn can go to an engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Hand over this text with the request, or nothing when the engine is current.
    Ready(Option<String>),
    /// The earlier conversation does not fit. Summarize first, then plan again.
    Compact(CompactionJob),
}

/// Text to fold into a summary, in order, each piece small enough to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionJob {
    /// The summary so far, which the new one continues from.
    pub existing: Option<String>,
    pub chunks: Vec<String>,
    /// How many messages the finished summary will stand for.
    pub covers_messages: usize,
    /// Upper bound, in characters, to ask of each summary.
    pub summary_chars: usize,
}

/// Decides what an engine that has seen the first `seen` messages needs
/// before the next request, given `budget_chars` for earlier conversation.
pub fn plan(summary: Option<&Summary>, messages: &[ChatMessage], seen: usize, budget_chars: usize) -> Plan {
    let stored = summary;
    let covered = stored.map_or(0, |summary| summary.covers_messages.min(messages.len()));
    let seen = seen.min(messages.len());
    // A summary the engine already lived through is not repeated to it.
    let summary = stored.filter(|_| covered > seen);
    let from = if summary.is_some() { covered } else { seen };
    let entries: Vec<(usize, String)> = messages[from..]
        .iter()
        .enumerate()
        .filter_map(|(offset, message)| render(message).map(|entry| (from + offset, entry)))
        .collect();
    if summary.is_none() && entries.is_empty() {
        return Plan::Ready(None);
    }
    let text = compose(summary.map(|summary| summary.text.as_str()), entries.iter().map(|(_, entry)| entry.as_str()));
    if text.chars().count() <= budget_chars {
        return Plan::Ready(Some(text));
    }

    // Keep the newest turns verbatim in half the budget and summarize the rest.
    let keep_budget = budget_chars / 2;
    let mut kept = 0;
    let mut first_kept = messages.len();
    for (index, entry) in entries.iter().rev() {
        let size = entry.chars().count() + 2;
        if kept + size > keep_budget {
            break;
        }
        kept += size;
        first_kept = *index;
    }
    // Everything before the kept turns is summarized, starting where the
    // existing summary ends, so the result stands for the whole beginning.
    let chunk_chars = (budget_chars / 2).max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    for message in &messages[covered..first_kept] {
        let Some(entry) = render(message) else { continue };
        for piece in split_chars(&entry, chunk_chars) {
            if !current.is_empty() && current.chars().count() + piece.chars().count() + 2 > chunk_chars {
                chunks.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(piece);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    Plan::Compact(CompactionJob {
        existing: stored.filter(|_| covered > 0).map(|summary| summary.text.clone()),
        chunks,
        covers_messages: first_kept,
        summary_chars: (budget_chars / 4).max(1),
    })
}

fn compose<'a>(summary: Option<&str>, entries: impl Iterator<Item = &'a str>) -> String {
    let mut text = String::from(HEADER);
    if let Some(summary) = summary {
        text.push_str("\n\n");
        text.push_str(SUMMARY_LABEL);
        text.push('\n');
        text.push_str(summary);
    }
    for entry in entries {
        text.push_str("\n\n");
        text.push_str(entry);
    }
    text
}

/// Splits on character boundaries into pieces of at most `limit` characters.
/// Every character of the input is in exactly one piece.
fn split_chars(text: &str, limit: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let end = rest.char_indices().nth(limit).map_or(rest.len(), |(index, _)| index);
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    pieces
}

/// The request an engine receives when it needs earlier conversation first.
pub fn with_prior_context(context: &str, prompt: &str) -> String {
    format!("{context}\n\n---\n\nCurrent request:\n{prompt}")
}

/// The instruction for one step of summarizing. `existing` is the summary
/// so far and `chunk` the next part of the conversation, in order.
pub fn summarize_request(existing: Option<&str>, chunk: &str, summary_chars: usize) -> String {
    let mut request = format!(
        "You are compressing the earlier part of a conversation so that it can continue with less text. \
Write one summary that a reader could continue the conversation from. Keep, in the conversation's own language: \
what the user asked for and decided, constraints and preferences they stated, facts and conclusions reached, \
what was done (files changed, commands run and their results), and what is still open. \
Do not add anything that is not in the text. Do not answer the conversation. Do not use tools. \
Write at most about {summary_chars} characters. Reply with the summary only."
    );
    if let Some(existing) = existing {
        request.push_str("\n\nSummary so far (keep everything in it that still matters):\n");
        request.push_str(existing);
    }
    request.push_str("\n\nNext part of the conversation:\n");
    request.push_str(chunk);
    request
}

fn render(message: &ChatMessage) -> Option<String> {
    if message.interrupted {
        return None;
    }
    let speaker = match message.role {
        Role::User => "User",
        Role::Assistant => "Assistant",
        Role::System => return None,
    };
    let mut entry = format!("{speaker}:\n{}", message.content.trim());
    for attachment in &message.attachments {
        match &attachment.payload {
            AttachmentPayload::Text(text) => {
                entry.push_str(&format!("\n\n[Attached file: {}]\n{text}", attachment.name));
            }
            AttachmentPayload::Image { .. } => {
                entry.push_str(&format!("\n\n[Attached image, not included: {}]", attachment.name));
            }
        }
    }
    for action in &message.actions {
        entry.push_str(&format!(
            "\n\n[Action {}: {}]",
            if action.succeeded { "done" } else { "failed" },
            action.title
        ));
        if let Some(diff) = &action.diff {
            let lines: Vec<&str> = diff.lines().collect();
            for line in lines.iter().take(DIFF_LINES) {
                entry.push('\n');
                entry.push_str(line);
            }
            if lines.len() > DIFF_LINES {
                entry.push_str(&format!("\n[{} more changed lines]", lines.len() - DIFF_LINES));
            }
        }
    }
    (!entry.trim_end().ends_with(':') || !message.attachments.is_empty() || !message.actions.is_empty())
        .then_some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ActionRecord, Attachment};

    fn ready(plan: Plan) -> Option<String> {
        match plan {
            Plan::Ready(text) => text,
            Plan::Compact(job) => panic!("compaction was not expected: {job:?}"),
        }
    }

    #[test]
    fn earlier_turns_are_written_out_without_thinking_or_interrupted_output() {
        let mut answer = ChatMessage::new_assistant("It is 4.");
        answer.thinking = "private reasoning".into();
        let mut stopped = ChatMessage::new_assistant("half an ans");
        stopped.interrupted = true;
        let messages = vec![
            ChatMessage::new(Role::System, "system text"),
            ChatMessage::new_user("What is 2+2?"),
            answer,
            stopped,
        ];
        let text = ready(plan(None, &messages, 0, 10_000)).unwrap();
        assert!(text.contains("User:\nWhat is 2+2?"));
        assert!(text.contains("Assistant:\nIt is 4."));
        for absent in ["private reasoning", "half an ans", "system text"] {
            assert!(!text.contains(absent), "{absent}");
        }
        assert!(with_prior_context(&text, "And 3+3?").ends_with("Current request:\nAnd 3+3?"));
    }

    #[test]
    fn actions_and_attachments_travel_with_the_turn_that_produced_them() {
        let mut request = ChatMessage::new_user("Fix it");
        request.attachments.push(Attachment { name: "notes.txt".into(), payload: AttachmentPayload::Text("the notes".into()) });
        request.attachments.push(Attachment {
            name: "shot.png".into(),
            payload: AttachmentPayload::Image { media_type: "image/png".into(), base64: "AAAA".into() },
        });
        let mut answer = ChatMessage::new_assistant("Fixed.");
        let long_diff = (0..60).map(|n| format!("+ line {n}")).collect::<Vec<_>>().join("\n");
        answer.actions.push(ActionRecord { title: "Edit src/a.rs".into(), detail: String::new(), diff: Some(long_diff), succeeded: true });
        answer.actions.push(ActionRecord { title: "cargo test".into(), detail: "1 failed".into(), diff: None, succeeded: false });
        let text = ready(plan(None, &[request, answer], 0, 100_000)).unwrap();
        assert!(text.contains("[Attached file: notes.txt]\nthe notes"));
        assert!(text.contains("[Attached image, not included: shot.png]"));
        assert!(!text.contains("AAAA"));
        assert!(text.contains("[Action done: Edit src/a.rs]\n+ line 0"));
        assert!(text.contains("+ line 39\n[20 more changed lines]"));
        assert!(!text.contains("+ line 40"));
        assert!(text.contains("[Action failed: cargo test]"));
    }

    #[test]
    fn an_engine_that_is_current_or_has_nothing_to_catch_up_on_gets_nothing() {
        let messages = vec![ChatMessage::new_user("a"), ChatMessage::new_assistant("b")];
        assert_eq!(plan(None, &messages, 2, 1_000), Plan::Ready(None));
        assert_eq!(plan(None, &messages, 9, 1_000), Plan::Ready(None));
        assert_eq!(plan(None, &[], 0, 1_000), Plan::Ready(None));
        let mut stopped = ChatMessage::new_assistant("partial");
        stopped.interrupted = true;
        assert_eq!(plan(None, &[stopped], 0, 1_000), Plan::Ready(None));
        // Only the turns it missed are handed over.
        let text = ready(plan(None, &messages, 1, 1_000)).unwrap();
        assert!(text.contains("Assistant:\nb") && !text.contains("User:\na"));
    }

    #[test]
    fn what_does_not_fit_is_summarized_and_never_left_out() {
        let messages: Vec<ChatMessage> = (0..10)
            .map(|n| ChatMessage::new_user(format!("message number {n} {}", "x".repeat(200))))
            .collect();
        let Plan::Compact(job) = plan(None, &messages, 0, 1_200) else { panic!("compaction was expected") };
        assert_eq!(job.existing, None);
        assert!(job.covers_messages > 0 && job.covers_messages < 10);
        // Every message before the kept ones is in the chunks, whole and in order.
        let folded = job.chunks.join("\n\n");
        for n in 0..job.covers_messages {
            assert!(folded.contains(&format!("message number {n} {}", "x".repeat(200))), "message {n}");
        }
        assert!(!folded.contains(&format!("message number {} ", job.covers_messages)));
        assert!(job.chunks.iter().all(|chunk| chunk.chars().count() <= 600));

        // With the summary in place the rest fits and nothing is missing.
        let summary = Summary { text: "SUMMARY OF THE BEGINNING".into(), covers_messages: job.covers_messages };
        let text = ready(plan(Some(&summary), &messages, 0, 1_200)).unwrap();
        assert!(text.contains("SUMMARY OF THE BEGINNING"));
        for n in job.covers_messages..10 {
            assert!(text.contains(&format!("message number {n} ")), "message {n}");
        }
        assert!(text.chars().count() <= 1_200);
    }

    #[test]
    fn a_single_oversized_message_is_split_for_summarizing_without_losing_a_character() {
        let huge = "あ".repeat(2_500);
        let messages = vec![ChatMessage::new_user(huge.clone()), ChatMessage::new_user("latest")];
        let Plan::Compact(job) = plan(None, &messages, 0, 1_000) else { panic!("compaction was expected") };
        assert_eq!(job.covers_messages, 1);
        assert!(job.chunks.len() > 1);
        assert!(job.chunks.iter().all(|chunk| chunk.chars().count() <= 500));
        assert_eq!(job.chunks.concat().matches('あ').count(), 2_500);
    }

    #[test]
    fn a_later_compaction_continues_from_the_existing_summary() {
        let messages: Vec<ChatMessage> = (0..12)
            .map(|n| ChatMessage::new_user(format!("message number {n} {}", "x".repeat(200))))
            .collect();
        let summary = Summary { text: "first summary".into(), covers_messages: 3 };
        let Plan::Compact(job) = plan(Some(&summary), &messages, 0, 1_200) else { panic!("compaction was expected") };
        assert_eq!(job.existing.as_deref(), Some("first summary"));
        let folded = job.chunks.join("\n\n");
        assert!(!folded.contains("message number 2 "));
        assert!(folded.contains("message number 3 "));
        assert!(job.covers_messages > 3);
        let request = summarize_request(job.existing.as_deref(), &job.chunks[0], job.summary_chars);
        assert!(request.contains("first summary") && request.contains("message number 3 "));
    }

    #[test]
    fn a_new_summary_keeps_the_old_one_even_when_the_engine_had_seen_that_part() {
        let messages: Vec<ChatMessage> = (0..12)
            .map(|n| ChatMessage::new_user(format!("message number {n} {}", "x".repeat(200))))
            .collect();
        let summary = Summary { text: "first summary".into(), covers_messages: 3 };
        // The engine saw the first five messages; the rest still does not fit.
        let Plan::Compact(job) = plan(Some(&summary), &messages, 5, 1_200) else { panic!("compaction was expected") };
        assert_eq!(job.existing.as_deref(), Some("first summary"));
        let folded = job.chunks.join("\n\n");
        assert!(folded.contains("message number 3 ") && folded.contains("message number 4 "));
    }

    #[test]
    fn an_engine_that_lived_through_the_summarized_part_is_not_given_the_summary() {
        let messages = vec![ChatMessage::new_user("old"), ChatMessage::new_assistant("older answer"), ChatMessage::new_user("new")];
        let summary = Summary { text: "the summary".into(), covers_messages: 2 };
        let text = ready(plan(Some(&summary), &messages, 2, 10_000)).unwrap();
        assert!(!text.contains("the summary") && text.contains("User:\nnew"));
        let text = ready(plan(Some(&summary), &messages, 0, 10_000)).unwrap();
        assert!(text.contains("the summary") && text.contains("User:\nnew") && !text.contains("older answer"));
    }
}
