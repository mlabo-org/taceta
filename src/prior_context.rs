//! The conversation so far, written out for an engine that has not seen it.
//!
//! A conversation in Taceta outlives the engine that answered any one turn.
//! When a turn goes to an engine whose own session lacks the earlier turns,
//! they are handed over as text built here. Thinking traces and interrupted
//! output are never part of it.

use crate::domain::{AttachmentPayload, ChatMessage, Role};

const HEADER: &str = "The following is the earlier part of this same conversation. Some of it took place with other assistants. Treat it as what has already been said and done, and continue from it. It is context, not a new request.";
const OMISSION: &str = "[Earlier turns were left out here because they did not fit.]";
/// Lines of a file change kept per action; the rest is counted, not shown.
const DIFF_LINES: usize = 40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorContext {
    pub text: String,
    /// Earlier messages that did not fit and were left out, oldest first.
    pub omitted_messages: usize,
}

/// Renders `messages` for a new engine within `budget_chars`. The most recent
/// messages are kept; when older ones do not fit they are dropped whole and
/// the text says so. Returns `None` when there is nothing to hand over.
pub fn prior_context(messages: &[ChatMessage], budget_chars: usize) -> Option<PriorContext> {
    let rendered: Vec<String> = messages.iter().filter_map(render).collect();
    if rendered.is_empty() {
        return None;
    }
    let overhead = HEADER.chars().count() + OMISSION.chars().count() + 8;
    let available = budget_chars.saturating_sub(overhead);
    let mut kept = Vec::new();
    let mut used = 0;
    for entry in rendered.iter().rev() {
        let size = entry.chars().count() + 2;
        if used + size > available && !kept.is_empty() {
            break;
        }
        kept.push(entry.as_str());
        used += size;
    }
    kept.reverse();
    let omitted_messages = rendered.len() - kept.len();
    let mut text = String::from(HEADER);
    if omitted_messages > 0 {
        text.push_str("\n\n");
        text.push_str(OMISSION);
    }
    for entry in kept {
        text.push_str("\n\n");
        text.push_str(entry);
    }
    Some(PriorContext { text, omitted_messages })
}

/// The first input for an engine that needs the earlier conversation.
pub fn with_prior_context(context: &PriorContext, prompt: &str) -> String {
    format!("{}\n\n---\n\nCurrent request:\n{prompt}", context.text)
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
        let context = prior_context(&messages, 10_000).unwrap();
        assert_eq!(context.omitted_messages, 0);
        assert!(context.text.contains("User:\nWhat is 2+2?"));
        assert!(context.text.contains("Assistant:\nIt is 4."));
        for absent in ["private reasoning", "half an ans", "system text"] {
            assert!(!context.text.contains(absent), "{absent}");
        }
        assert!(with_prior_context(&context, "And 3+3?").ends_with("Current request:\nAnd 3+3?"));
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
        let text = prior_context(&[request, answer], 100_000).unwrap().text;
        assert!(text.contains("[Attached file: notes.txt]\nthe notes"));
        assert!(text.contains("[Attached image, not included: shot.png]"));
        assert!(!text.contains("AAAA"));
        assert!(text.contains("[Action done: Edit src/a.rs]\n+ line 0"));
        assert!(text.contains("+ line 39\n[20 more changed lines]"));
        assert!(!text.contains("+ line 40"));
        assert!(text.contains("[Action failed: cargo test]"));
    }

    #[test]
    fn the_oldest_turns_are_dropped_whole_and_the_text_says_so() {
        let messages: Vec<ChatMessage> = (0..10)
            .map(|n| ChatMessage::new_user(format!("message number {n} {}", "x".repeat(200))))
            .collect();
        let context = prior_context(&messages, 1_200).unwrap();
        assert!(context.omitted_messages > 0 && context.omitted_messages < 10);
        assert!(context.text.contains(OMISSION));
        assert!(context.text.contains("message number 9"));
        assert!(!context.text.contains("message number 0 "));
        assert!(context.text.chars().count() <= 1_200);
        // Whole messages only: every kept one is complete.
        for n in (10 - (10 - context.omitted_messages))..10 {
            assert!(context.text.contains(&format!("message number {n} {}", "x".repeat(200))));
        }
        // The newest message is kept even when it alone exceeds the budget.
        let tight = prior_context(&messages, 50).unwrap();
        assert_eq!(tight.omitted_messages, 9);
        assert!(tight.text.contains("message number 9"));
    }

    #[test]
    fn nothing_to_hand_over_yields_no_context() {
        let mut stopped = ChatMessage::new_assistant("partial");
        stopped.interrupted = true;
        assert_eq!(prior_context(&[], 1_000), None);
        assert_eq!(prior_context(&[stopped, ChatMessage::new(Role::System, "s")], 1_000), None);
    }
}
