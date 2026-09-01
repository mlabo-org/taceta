use eframe::egui::{self, Color32, CornerRadius, Frame, Margin, Stroke};

#[derive(Clone, Copy)]
pub struct TacetaPalette {
    pub sidebar: Color32,
    pub composer: Color32,
    pub user_bubble: Color32,
    pub thinking: Color32,
    pub border: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
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
            muted: Color32::from_rgb(164, 164, 170),
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
            muted: Color32::from_rgb(101, 101, 108),
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
