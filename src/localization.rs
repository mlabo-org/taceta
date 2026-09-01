use crate::app_shell_foundation::AppShellLanguage;

pub fn system_language() -> AppShellLanguage {
    let locale = sys_locale::get_locale()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if locale.starts_with("ja") {
        AppShellLanguage::Japanese
    } else {
        AppShellLanguage::English
    }
}

pub fn text<'a>(language: AppShellLanguage, japanese: &'a str, english: &'a str) -> &'a str {
    match language {
        AppShellLanguage::Japanese => japanese,
        AppShellLanguage::English => english,
    }
}

pub fn conversation_title(input: &str, fallback: &str) -> String {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let source = if normalized.is_empty() {
        fallback.trim()
    } else {
        normalized.trim()
    };

    let mut title = source.chars().take(30).collect::<String>();
    if source.chars().count() > 30 {
        title.push('…');
    }
    if title.is_empty() {
        "Taceta".to_owned()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_compact_and_unicode_safe() {
        let source = "日本語の長い会話タイトルを安全に短くしてサイドバーへ表示するための文章です";
        let title = conversation_title(source, "fallback");
        assert!(title.chars().count() <= 31);
        assert!(title.ends_with('…'));
    }
}
