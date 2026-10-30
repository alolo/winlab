// Visual theme — QLab-inspired dark design

use egui::{Color32, CornerRadius, Shadow, Stroke, Style};

pub struct Theme;

impl Theme {
    // Palette
    pub const BG_DARK: Color32      = Color32::from_rgb(18, 18, 22);
    pub const BG_PANEL: Color32     = Color32::from_rgb(26, 26, 32);
    pub const BG_ROW: Color32       = Color32::from_rgb(32, 32, 40);
    pub const BG_ROW_ALT: Color32   = Color32::from_rgb(28, 28, 36);
    pub const BG_SELECTED: Color32  = Color32::from_rgb(40, 80, 140);
    pub const BG_ACTIVE: Color32    = Color32::from_rgb(20, 100, 60);
    pub const BG_PAUSED: Color32    = Color32::from_rgb(100, 80, 20);
    pub const ACCENT: Color32       = Color32::from_rgb(90, 170, 255);
    pub const ACCENT2: Color32      = Color32::from_rgb(60, 220, 130);
    pub const TEXT: Color32         = Color32::from_rgb(220, 220, 230);
    pub const TEXT_DIM: Color32     = Color32::from_rgb(130, 130, 150);
    pub const TEXT_ACTIVE: Color32  = Color32::from_rgb(80, 255, 140);
    pub const BORDER: Color32       = Color32::from_rgb(50, 50, 65);
    pub const GO_BTN: Color32       = Color32::from_rgb(40, 180, 80);
    pub const STOP_BTN: Color32     = Color32::from_rgb(200, 50, 50);
    pub const PAUSE_BTN: Color32    = Color32::from_rgb(200, 160, 30);
    pub const WAVE_FILL: Color32    = Color32::from_rgb(60, 140, 220);
    pub const WAVE_BORDER: Color32  = Color32::from_rgb(90, 170, 255);
    pub const MARKER_START: Color32 = Color32::from_rgb(60, 220, 130);
    pub const MARKER_LOOP: Color32  = Color32::from_rgb(255, 180, 40);
    pub const MARKER_END: Color32   = Color32::from_rgb(220, 60, 60);

    pub fn apply(ctx: &egui::Context) {
        let mut style = Style::default();

        // Corner radius (egui 0.31: Rounding → CornerRadius, uses u8)
        let cr4 = CornerRadius::same(4);
        style.visuals.window_corner_radius = CornerRadius::same(8);
        style.visuals.widgets.noninteractive.corner_radius = cr4;
        style.visuals.widgets.inactive.corner_radius  = cr4;
        style.visuals.widgets.hovered.corner_radius   = cr4;
        style.visuals.widgets.active.corner_radius    = cr4;
        style.visuals.widgets.open.corner_radius      = cr4;

        // Colors
        style.visuals.dark_mode = true;
        style.visuals.panel_fill         = Self::BG_PANEL;
        style.visuals.window_fill        = Self::BG_DARK;
        style.visuals.extreme_bg_color   = Self::BG_DARK;
        style.visuals.faint_bg_color     = Self::BG_ROW_ALT;
        style.visuals.window_shadow      = Shadow::NONE;
        style.visuals.window_stroke      = Stroke::new(1.0_f32, Self::BORDER);

        // Widget colors
        style.visuals.widgets.noninteractive.bg_fill   = Self::BG_ROW;
        style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, Self::TEXT_DIM);
        style.visuals.widgets.inactive.bg_fill         = Self::BG_ROW;
        style.visuals.widgets.inactive.fg_stroke       = Stroke::new(1.0_f32, Self::TEXT);
        style.visuals.widgets.hovered.bg_fill          = Color32::from_rgb(50, 50, 65);
        style.visuals.widgets.hovered.fg_stroke        = Stroke::new(1.5_f32, Self::ACCENT);
        style.visuals.widgets.active.bg_fill           = Self::BG_SELECTED;
        style.visuals.widgets.active.fg_stroke         = Stroke::new(1.5_f32, Self::ACCENT);
        style.visuals.selection.bg_fill                = Self::BG_SELECTED;
        style.visuals.selection.stroke                 = Stroke::new(1.0_f32, Self::ACCENT);

        // Spacing (egui 0.31: Margin::same/symmetric take i8)
        style.spacing.item_spacing   = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.window_margin  = egui::Margin::same(8_i8);

        ctx.set_style(style);

        let fonts = egui::FontDefinitions::default();
        ctx.set_fonts(fonts);
    }
}
