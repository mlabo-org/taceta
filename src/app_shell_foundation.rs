use eframe::egui::{self, FontFamily, RichText, ThemePreference, Vec2};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};

pub const APP_SHELL_FONT_SIZE_MIN_POINTS: u8 = 10;
pub const APP_SHELL_FONT_SIZE_MAX_POINTS: u8 = 32;
pub const APP_SHELL_FONT_SIZE_DEFAULT_POINTS: u8 = 16;

#[cfg(target_os = "macos")]
const APP_SHELL_HIRAGINO_FONT_NAME: &str = "app-shell-hiragino-kaku-gothic-w3";
#[cfg(target_os = "macos")]
const APP_SHELL_SF_PRO_FONT_NAME: &str = "app-shell-sf-pro";
#[cfg(target_os = "macos")]
const APP_SHELL_SF_MONO_FONT_NAME: &str = "app-shell-sf-mono";

#[cfg(target_os = "macos")]
pub fn install_macos_system_fonts(ctx: &egui::Context) -> Result<(), String> {
    let hiragino_path = find_hiragino_kaku_gothic()?;
    let hiragino = read_font(&hiragino_path)?;
    let sf_pro = read_font(Path::new("/System/Library/Fonts/SFNS.ttf"))?;
    let sf_mono = read_font(Path::new("/System/Library/Fonts/SFNSMono.ttf"))?;

    ctx.set_fonts(macos_font_definitions(hiragino, sf_pro, sf_mono));
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install_macos_system_fonts(_ctx: &egui::Context) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_font_definitions(
    hiragino: Vec<u8>,
    sf_pro: Vec<u8>,
    sf_mono: Vec<u8>,
) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        APP_SHELL_HIRAGINO_FONT_NAME.to_owned(),
        egui::FontData::from_owned(hiragino).into(),
    );
    fonts.font_data.insert(
        APP_SHELL_SF_PRO_FONT_NAME.to_owned(),
        egui::FontData::from_owned(sf_pro).into(),
    );
    fonts.font_data.insert(
        APP_SHELL_SF_MONO_FONT_NAME.to_owned(),
        egui::FontData::from_owned(sf_mono).into(),
    );

    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, APP_SHELL_SF_PRO_FONT_NAME.to_owned());
    proportional.insert(0, APP_SHELL_HIRAGINO_FONT_NAME.to_owned());

    let monospace = fonts.families.entry(FontFamily::Monospace).or_default();
    monospace.insert(0, APP_SHELL_HIRAGINO_FONT_NAME.to_owned());
    monospace.insert(0, APP_SHELL_SF_MONO_FONT_NAME.to_owned());

    fonts
}

#[cfg(target_os = "macos")]
fn read_font(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| {
        format!(
            "failed to read macOS system font {}: {error}",
            path.display()
        )
    })
}

#[cfg(target_os = "macos")]
fn find_hiragino_kaku_gothic() -> Result<PathBuf, String> {
    let font_directory = Path::new("/System/Library/Fonts");
    let mut candidates = std::fs::read_dir(font_directory)
        .map_err(|error| {
            format!(
                "failed to inspect macOS system fonts at {}: {error}",
                font_directory.display()
            )
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            path.extension().is_some_and(|extension| extension == "ttc")
                && (name.contains('角') || name == "Hiragino Sans GB.ttc")
        })
        .collect::<Vec<_>>();

    candidates.sort_by_key(|path| {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        if name.ends_with(" W3.ttc") {
            0
        } else if name.ends_with(" W4.ttc") {
            1
        } else if name.contains('角') {
            2
        } else {
            3
        }
    });

    candidates.into_iter().next().ok_or_else(|| {
        format!(
            "no Hiragino Gothic system font was found under {}",
            font_directory.display()
        )
    })
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppShellThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl AppShellThemePreference {
    fn egui_preference(self) -> ThemePreference {
        match self {
            Self::System => ThemePreference::System,
            Self::Light => ThemePreference::Light,
            Self::Dark => ThemePreference::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppShellLanguagePreference {
    #[default]
    System,
    Japanese,
    English,
}

impl AppShellLanguagePreference {
    pub fn resolve(self, system_language: AppShellLanguage) -> AppShellLanguage {
        match self {
            Self::System => system_language,
            Self::Japanese => AppShellLanguage::Japanese,
            Self::English => AppShellLanguage::English,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppShellLanguage {
    Japanese,
    English,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppShellFontFamily {
    #[default]
    System,
    Monospaced,
}

impl AppShellFontFamily {
    fn egui_family(self) -> FontFamily {
        match self {
            Self::System => FontFamily::Proportional,
            Self::Monospaced => FontFamily::Monospace,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AppShellPreferences {
    pub theme: AppShellThemePreference,
    pub language: AppShellLanguagePreference,
    pub font_family: AppShellFontFamily,
    pub font_size_points: u8,
}

impl Default for AppShellPreferences {
    fn default() -> Self {
        Self {
            theme: AppShellThemePreference::System,
            language: AppShellLanguagePreference::System,
            font_family: AppShellFontFamily::System,
            font_size_points: APP_SHELL_FONT_SIZE_DEFAULT_POINTS,
        }
    }
}

impl AppShellPreferences {
    pub fn normalized(mut self) -> Self {
        self.font_size_points = self.font_size_points.clamp(
            APP_SHELL_FONT_SIZE_MIN_POINTS,
            APP_SHELL_FONT_SIZE_MAX_POINTS,
        );
        self
    }

    pub fn zoom_factor(self) -> f32 {
        f32::from(self.font_size_points) / f32::from(APP_SHELL_FONT_SIZE_DEFAULT_POINTS)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AppShellPreferencesChange {
    pub changed: bool,
    pub font_size_changed: bool,
}

pub fn load_app_shell_preferences(
    storage: Option<&dyn eframe::Storage>,
    storage_key: &str,
) -> AppShellPreferences {
    storage
        .and_then(|storage| eframe::get_value::<AppShellPreferences>(storage, storage_key))
        .unwrap_or_default()
        .normalized()
}

pub fn save_app_shell_preferences(
    storage: &mut dyn eframe::Storage,
    storage_key: &str,
    preferences: &AppShellPreferences,
) {
    eframe::set_value(storage, storage_key, &preferences.normalized());
}

pub fn apply_app_shell_preferences(ctx: &egui::Context, preferences: AppShellPreferences) {
    let preferences = preferences.normalized();
    ctx.set_theme(preferences.theme.egui_preference());
    ctx.set_zoom_factor(preferences.zoom_factor());
    ctx.options_mut(|options| options.zoom_with_keyboard = false);

    let family = preferences.font_family.egui_family();
    ctx.global_style_mut(|style| {
        for font_id in style.text_styles.values_mut() {
            font_id.family = family.clone();
        }
    });
}

pub fn show_app_shell_preferences(
    ui: &mut egui::Ui,
    preferences: &mut AppShellPreferences,
    system_language: AppShellLanguage,
    id_source: &str,
) -> AppShellPreferencesChange {
    *preferences = preferences.normalized();
    let before = *preferences;
    let language = preferences.language.resolve(system_language);

    egui::Grid::new((id_source, "app-shell-preferences"))
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label(label(language, "テーマ", "Theme"));
            egui::ComboBox::from_id_salt((id_source, "theme"))
                .selected_text(theme_label(preferences.theme, language))
                .show_ui(ui, |ui| {
                    for choice in [
                        AppShellThemePreference::System,
                        AppShellThemePreference::Light,
                        AppShellThemePreference::Dark,
                    ] {
                        ui.selectable_value(
                            &mut preferences.theme,
                            choice,
                            theme_label(choice, language),
                        );
                    }
                });
            ui.end_row();

            ui.label(label(language, "言語", "Language"));
            egui::ComboBox::from_id_salt((id_source, "language"))
                .selected_text(language_preference_label(preferences.language, language))
                .show_ui(ui, |ui| {
                    for choice in [
                        AppShellLanguagePreference::System,
                        AppShellLanguagePreference::Japanese,
                        AppShellLanguagePreference::English,
                    ] {
                        ui.selectable_value(
                            &mut preferences.language,
                            choice,
                            language_preference_label(choice, language),
                        );
                    }
                });
            ui.end_row();

            ui.label(label(language, "フォント", "Font"));
            egui::ComboBox::from_id_salt((id_source, "font-family"))
                .selected_text(font_family_label(preferences.font_family, language))
                .show_ui(ui, |ui| {
                    for choice in [AppShellFontFamily::System, AppShellFontFamily::Monospaced] {
                        ui.selectable_value(
                            &mut preferences.font_family,
                            choice,
                            font_family_label(choice, language),
                        );
                    }
                });
            ui.end_row();

            ui.label(label(language, "文字サイズ", "Text size"));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.add_sized(
                    [58.0, 22.0],
                    egui::DragValue::new(&mut preferences.font_size_points)
                        .range(APP_SHELL_FONT_SIZE_MIN_POINTS..=APP_SHELL_FONT_SIZE_MAX_POINTS)
                        .speed(1.0)
                        .fixed_decimals(0)
                        .max_decimals(0)
                        .suffix(" pt"),
                )
                .on_hover_text(label(
                    language,
                    "10〜32 ptを直接入力",
                    "Enter a value from 10 through 32 pt",
                ));

                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.spacing_mut().button_padding = Vec2::ZERO;

                    if ui
                        .add_enabled(
                            preferences.font_size_points < APP_SHELL_FONT_SIZE_MAX_POINTS,
                            egui::Button::new(RichText::new("▲").size(7.0))
                                .min_size(Vec2::new(16.0, 10.0)),
                        )
                        .on_hover_text(label(
                            language,
                            "文字サイズを1 pt大きくする",
                            "Increase text size by 1 pt",
                        ))
                        .clicked()
                    {
                        preferences.font_size_points =
                            increment_font_size(preferences.font_size_points);
                    }

                    if ui
                        .add_enabled(
                            preferences.font_size_points > APP_SHELL_FONT_SIZE_MIN_POINTS,
                            egui::Button::new(RichText::new("▼").size(7.0))
                                .min_size(Vec2::new(16.0, 10.0)),
                        )
                        .on_hover_text(label(
                            language,
                            "文字サイズを1 pt小さくする",
                            "Decrease text size by 1 pt",
                        ))
                        .clicked()
                    {
                        preferences.font_size_points =
                            decrement_font_size(preferences.font_size_points);
                    }
                });
            });
            ui.end_row();
        });

    *preferences = preferences.normalized();
    AppShellPreferencesChange {
        changed: *preferences != before,
        font_size_changed: preferences.font_size_points != before.font_size_points,
    }
}

fn increment_font_size(points: u8) -> u8 {
    points.saturating_add(1).min(APP_SHELL_FONT_SIZE_MAX_POINTS)
}

fn decrement_font_size(points: u8) -> u8 {
    points.saturating_sub(1).max(APP_SHELL_FONT_SIZE_MIN_POINTS)
}

fn label<'a>(language: AppShellLanguage, japanese: &'a str, english: &'a str) -> &'a str {
    match language {
        AppShellLanguage::Japanese => japanese,
        AppShellLanguage::English => english,
    }
}

fn theme_label(choice: AppShellThemePreference, language: AppShellLanguage) -> &'static str {
    match (choice, language) {
        (AppShellThemePreference::System, AppShellLanguage::Japanese) => "システム",
        (AppShellThemePreference::System, AppShellLanguage::English) => "System",
        (AppShellThemePreference::Light, AppShellLanguage::Japanese) => "ライト",
        (AppShellThemePreference::Light, AppShellLanguage::English) => "Light",
        (AppShellThemePreference::Dark, AppShellLanguage::Japanese) => "ダーク",
        (AppShellThemePreference::Dark, AppShellLanguage::English) => "Dark",
    }
}

fn language_preference_label(
    choice: AppShellLanguagePreference,
    language: AppShellLanguage,
) -> &'static str {
    match (choice, language) {
        (AppShellLanguagePreference::System, AppShellLanguage::Japanese) => "システム",
        (AppShellLanguagePreference::System, AppShellLanguage::English) => "System",
        (AppShellLanguagePreference::Japanese, AppShellLanguage::Japanese) => "日本語",
        (AppShellLanguagePreference::Japanese, AppShellLanguage::English) => "Japanese",
        (AppShellLanguagePreference::English, AppShellLanguage::Japanese) => "英語",
        (AppShellLanguagePreference::English, AppShellLanguage::English) => "English",
    }
}

fn font_family_label(choice: AppShellFontFamily, language: AppShellLanguage) -> &'static str {
    match (choice, language) {
        (AppShellFontFamily::System, AppShellLanguage::Japanese) => "システム",
        (AppShellFontFamily::System, AppShellLanguage::English) => "System",
        (AppShellFontFamily::Monospaced, AppShellLanguage::Japanese) => "等幅",
        (AppShellFontFamily::Monospaced, AppShellLanguage::English) => "Monospaced",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_steps_move_one_point_and_stop_at_bounds() {
        assert_eq!(increment_font_size(16), 17);
        assert_eq!(decrement_font_size(16), 15);
        assert_eq!(increment_font_size(APP_SHELL_FONT_SIZE_MAX_POINTS), 32);
        assert_eq!(decrement_font_size(APP_SHELL_FONT_SIZE_MIN_POINTS), 10);
    }

    #[test]
    fn persisted_font_size_is_normalized() {
        assert_eq!(
            AppShellPreferences {
                font_size_points: 9,
                ..Default::default()
            }
            .normalized()
            .font_size_points,
            APP_SHELL_FONT_SIZE_MIN_POINTS
        );
        assert_eq!(
            AppShellPreferences {
                font_size_points: 99,
                ..Default::default()
            }
            .normalized()
            .font_size_points,
            APP_SHELL_FONT_SIZE_MAX_POINTS
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn normal_mixed_ui_text_uses_one_hiragino_face_before_sf_fallback() {
        let fonts = macos_font_definitions(Vec::new(), Vec::new(), Vec::new());
        let proportional = fonts
            .families
            .get(&FontFamily::Proportional)
            .expect("proportional font family");
        let monospace = fonts
            .families
            .get(&FontFamily::Monospace)
            .expect("monospace font family");

        assert_eq!(proportional[0], APP_SHELL_HIRAGINO_FONT_NAME);
        assert_eq!(proportional[1], APP_SHELL_SF_PRO_FONT_NAME);
        assert_eq!(monospace[0], APP_SHELL_SF_MONO_FONT_NAME);
        assert_eq!(monospace[1], APP_SHELL_HIRAGINO_FONT_NAME);
    }
}
