use eframe::egui::{self, Color32, CornerRadius, Frame, Margin, Stroke};

#[derive(Clone, Copy)]
pub struct TacetaPalette {
    pub sidebar: Color32,
    pub composer: Color32,
    pub user_bubble: Color32,
    pub thinking: Color32,
    pub border: Color32,
    pub contrast_text: Color32,
    pub placeholder_text: Color32,
    pub accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
}

pub fn apply_text_contrast(ui: &mut egui::Ui) {
    let dark = ui.visuals().dark_mode;
    let text_color = if dark { Color32::WHITE } else { Color32::BLACK };
    let visuals = ui.visuals_mut();
    visuals.override_text_color = Some(text_color);
    visuals.weak_text_color = Some(text_color);
    visuals.widgets.noninteractive.fg_stroke.color = text_color;
    visuals.widgets.inactive.fg_stroke.color = text_color;
    visuals.widgets.hovered.fg_stroke.color = text_color;
    visuals.widgets.active.fg_stroke.color = text_color;
    visuals.widgets.open.fg_stroke.color = text_color;
    apply_selection_contrast(visuals);
}

/// Apply Taceta's selection colors to a local UI scope.
///
/// egui menu popups are rendered in a separate `Ui` and therefore do not
/// inherit the root UI's local visual overrides. Keep this narrowly scoped to
/// selection visuals so menu-specific styling remains otherwise unchanged.
pub fn apply_selection_contrast(visuals: &mut egui::Visuals) {
    let selection_blue = if visuals.dark_mode {
        Color32::from_rgb(10, 132, 255)
    } else {
        Color32::from_rgb(0, 122, 255)
    };
    visuals.selection.bg_fill = selection_blue;
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
}

pub fn palette(ui: &egui::Ui) -> TacetaPalette {
    let dark = ui.visuals().dark_mode;
    if dark {
        TacetaPalette {
            sidebar: Color32::from_rgb(25, 25, 26),
            composer: Color32::from_rgb(47, 47, 49),
            user_bubble: Color32::from_rgb(38, 38, 40),
            thinking: Color32::from_rgb(31, 35, 40),
            border: Color32::from_rgb(67, 67, 70),
            contrast_text: Color32::WHITE,
            placeholder_text: Color32::from_rgb(128, 128, 134),
            accent: Color32::from_rgb(176, 135, 255),
            success: Color32::from_rgb(98, 208, 144),
            warning: Color32::from_rgb(244, 183, 82),
            error: Color32::from_rgb(239, 111, 108),
        }
    } else {
        TacetaPalette {
            sidebar: Color32::from_rgb(245, 245, 246),
            composer: Color32::from_rgb(250, 250, 251),
            user_bubble: Color32::from_rgb(239, 239, 242),
            thinking: Color32::from_rgb(243, 246, 251),
            border: Color32::from_rgb(216, 216, 220),
            contrast_text: Color32::BLACK,
            placeholder_text: Color32::from_rgb(128, 128, 134),
            accent: Color32::from_rgb(107, 68, 188),
            success: Color32::from_rgb(35, 139, 83),
            warning: Color32::from_rgb(173, 104, 15),
            error: Color32::from_rgb(188, 54, 50),
        }
    }
}

pub fn card(fill: Color32, border: Color32, radius: u8, margin: i8) -> Frame {
    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(radius))
        .inner_margin(Margin::same(margin))
}
