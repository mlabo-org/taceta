use eframe::egui::{self, Color32, CornerRadius, Frame, Margin, Stroke};

#[derive(Clone, Copy)]
pub struct TacetaPalette {
    pub background: Color32,
    pub sidebar: Color32,
    pub composer: Color32,
    pub user_bubble: Color32,
    pub thinking: Color32,
    pub border: Color32,
    /// Fill of a row or quiet button under the pointer.
    pub hover: Color32,
    /// Fill of the row or choice that is current.
    pub selected: Color32,
    pub contrast_text: Color32,
    pub weak_text: Color32,
    pub placeholder_text: Color32,
    pub accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
}

/// Applies Taceta's look to the root UI: neutral surfaces, text in two
/// strengths, and controls that stay quiet until the pointer is on them.
pub fn apply_style(ui: &mut egui::Ui) {
    let palette = palette(ui);
    let visuals = ui.visuals_mut();
    visuals.override_text_color = Some(palette.contrast_text);
    visuals.weak_text_color = Some(palette.weak_text);
    visuals.panel_fill = palette.background;
    visuals.window_fill = palette.composer;
    visuals.window_stroke = Stroke::new(1.0, palette.border);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(10);
    // Text fields sit one step away from the page they are on.
    visuals.extreme_bg_color = palette.sidebar;
    visuals.faint_bg_color = palette.hover;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.border);
    visuals.widgets.noninteractive.bg_fill = palette.composer;
    for (widget, fill) in [
        (&mut visuals.widgets.inactive, palette.hover),
        (&mut visuals.widgets.hovered, palette.selected),
        (&mut visuals.widgets.active, palette.selected),
        (&mut visuals.widgets.open, palette.selected),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::NONE;
        widget.corner_radius = CornerRadius::same(8);
        widget.expansion = 0.0;
    }
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.fg_stroke.color = palette.contrast_text;
    }
    apply_selection_contrast(visuals);
    ui.spacing_mut().button_padding.x = 10.0;
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
            background: Color32::from_rgb(30, 30, 31),
            sidebar: Color32::from_rgb(23, 23, 24),
            composer: Color32::from_rgb(42, 42, 44),
            user_bubble: Color32::from_rgb(47, 47, 50),
            thinking: Color32::from_rgb(36, 36, 38),
            border: Color32::from_rgb(58, 58, 62),
            hover: Color32::from_rgb(44, 44, 47),
            selected: Color32::from_rgb(56, 56, 60),
            contrast_text: Color32::from_rgb(236, 236, 238),
            weak_text: Color32::from_rgb(160, 160, 168),
            placeholder_text: Color32::from_rgb(128, 128, 134),
            accent: Color32::from_rgb(176, 135, 255),
            success: Color32::from_rgb(98, 208, 144),
            warning: Color32::from_rgb(244, 183, 82),
            error: Color32::from_rgb(239, 111, 108),
        }
    } else {
        TacetaPalette {
            background: Color32::from_rgb(255, 255, 255),
            sidebar: Color32::from_rgb(247, 247, 248),
            composer: Color32::from_rgb(255, 255, 255),
            user_bubble: Color32::from_rgb(241, 241, 244),
            thinking: Color32::from_rgb(247, 247, 249),
            border: Color32::from_rgb(224, 224, 229),
            hover: Color32::from_rgb(238, 238, 241),
            selected: Color32::from_rgb(228, 228, 233),
            contrast_text: Color32::from_rgb(26, 26, 28),
            weak_text: Color32::from_rgb(104, 104, 112),
            placeholder_text: Color32::from_rgb(128, 128, 134),
            accent: Color32::from_rgb(107, 68, 188),
            success: Color32::from_rgb(35, 139, 83),
            warning: Color32::from_rgb(173, 104, 15),
            error: Color32::from_rgb(188, 54, 50),
        }
    }
}

/// Adds a slider whose rail stands out from the card it sits on.
///
/// egui paints the rail with `widgets.inactive.bg_fill`, which this theme
/// keeps equal to the card fill so idle controls stay quiet; without this the
/// rail disappears and only the handle shows.
pub fn slider(ui: &mut egui::Ui, slider: egui::Slider<'_>) -> egui::Response {
    let rail = palette(ui).border;
    ui.scope(|ui| {
        ui.visuals_mut().widgets.inactive.bg_fill = rail;
        ui.add(slider)
    })
    .inner
}

pub fn card(fill: Color32, border: Color32, radius: u8, margin: i8) -> Frame {
    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(radius))
        .inner_margin(Margin::same(margin))
}
