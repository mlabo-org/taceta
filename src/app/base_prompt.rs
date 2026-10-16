//! The short system prompt put ahead of a plain Ollama chat. Local models
//! are told nothing else, so this gives them today's date, which model they
//! are, and how to treat what they do not know. Web search and agent mode
//! keep their own instructions and do not use it.

use super::*;
use chrono::{DateTime, Local, TimeZone};

pub(super) const DATE_PLACEHOLDER: &str = "{date}";
pub(super) const MODEL_PLACEHOLDER: &str = "{model}";

/// The built-in prompt, used until the user edits it.
pub(super) fn default_text(language: AppShellLanguage) -> &'static str {
    text(
        language,
        "今日の日付は{date}です。\n\
あなたは、ユーザーがTacetaで選んだモデル「{model}」です。名前やどのモデルかを聞かれたら、この名前を答えてください。この会話の前の回答は別のモデルが書いたことがあり、そこで名乗っている名前はあなたのものではありません。\n\
知らないことや、学習した時点より後の出来事は、知らないと言ってください。推測を事実のように言わないでください。\n\
ユーザーと同じ言語で答えてください。",
        "Today's date is {date}.\n\
You are the model \"{model}\", chosen by the user in Taceta. When asked your name or which model you are, give this name. Earlier answers in this conversation may have been written by another model; a name they give is not yours.\n\
When you do not know something, or it happened after your training, say that you do not know. Do not state a guess as a fact.\n\
Answer in the user's language.",
    )
}

/// The date as the prompt gives it. It carries no time of day, so the
/// prompt stays the same for the whole day.
fn date_text<Tz: TimeZone>(now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    now.format("%Y-%m-%d (%a), UTC%:z").to_string()
}

/// The prompt with its placeholders filled in, or `None` when it is blank.
pub(super) fn render<Tz: TimeZone>(template: &str, model: &str, now: &DateTime<Tz>) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    let template = template.trim();
    if template.is_empty() {
        return None;
    }
    Some(template.replace(DATE_PLACEHOLDER, &date_text(now)).replace(MODEL_PLACEHOLDER, model))
}

/// The one system message a request starts with: the base prompt, then the
/// summary of the conversation's older part. A single message leaves no
/// question of how a model's template treats several.
pub(super) fn leading_message(base: Option<String>, summary: Option<&str>) -> Option<ChatMessage> {
    let summary = summary.map(|summary| format!("Summary of the conversation up to this point:\n{summary}"));
    let content = match (base, summary) {
        (Some(base), Some(summary)) => format!("{base}\n\n{summary}"),
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => return None,
    };
    Some(ChatMessage::new_system(content))
}

impl TacetaApp {
    /// The prompt as it would be sent to `model` now.
    pub(super) fn rendered_base_prompt(&self, model: &str) -> Option<String> {
        let template = match &self.state.local_system_prompt {
            Some(edited) => edited.as_str(),
            None => default_text(self.language()),
        };
        render(template, model, &Local::now())
    }

    pub(super) fn show_base_prompt_settings(&mut self, ui: &mut Ui) {
        let language = self.language();
        let palette = theme::palette(ui);
        theme::card(ui.visuals().faint_bg_color, palette.border, 14, 16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.strong(text(language, "ベースのシステムプロンプト", "Base system prompt"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(self.state.local_system_prompt.is_some(), Button::new(text(language, "既定に戻す", "Reset to default")).small())
                        .clicked()
                    {
                        self.state.local_system_prompt = None;
                    }
                });
            });
            ui.add_space(4.0);
            ui.label(RichText::new(text(
                language,
                "OllamaのChatで、毎回会話の先頭に付けます。Web検索と作業モードは、それぞれ専用の指示を使うので付けません。",
                "Put ahead of every Ollama chat request. Web search and agent mode use their own instructions instead.",
            )).weak());
            ui.add_space(10.0);
            let mut prompt = self
                .state
                .local_system_prompt
                .clone()
                .unwrap_or_else(|| default_text(language).to_owned());
            let response = ui.add(
                TextEdit::multiline(&mut prompt)
                    .desired_width(f32::INFINITY)
                    .desired_rows(6),
            );
            if response.changed() {
                self.state.local_system_prompt = Some(prompt);
            }
            ui.add_space(6.0);
            ui.label(RichText::new(text(
                language,
                "{date} は今日の日付、{model} は選んでいるモデル名に置き換わります。空にすると何も付けません。",
                "{date} becomes today's date and {model} the selected model's name. Leave it empty to send nothing.",
            )).small().weak());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn tokyo_noon() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339("2026-10-16T12:00:00+09:00").unwrap()
    }

    #[test]
    fn the_default_prompt_names_the_date_and_model_in_either_language() {
        for language in [AppShellLanguage::Japanese, AppShellLanguage::English] {
            let prompt = render(default_text(language), "gpt-oss:20b", &tokyo_noon()).unwrap();
            assert!(prompt.contains("2026-10-16 (Fri), UTC+09:00"), "{prompt}");
            assert!(prompt.contains("gpt-oss:20b"), "{prompt}");
            assert!(!prompt.contains(DATE_PLACEHOLDER) && !prompt.contains(MODEL_PLACEHOLDER));
        }
    }

    #[test]
    fn a_blank_prompt_sends_nothing_and_the_summary_shares_the_one_system_message() {
        assert_eq!(render("  \n ", "model", &tokyo_noon()), None);
        assert!(leading_message(None, None).is_none());
        let only_summary = leading_message(None, Some("earlier")).unwrap();
        assert_eq!(only_summary.content, "Summary of the conversation up to this point:\nearlier");
        let both = leading_message(Some("base".into()), Some("earlier")).unwrap();
        assert_eq!(both.role, Role::System);
        assert_eq!(both.content, "base\n\nSummary of the conversation up to this point:\nearlier");
    }
}
