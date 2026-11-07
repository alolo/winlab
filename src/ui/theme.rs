// Runtime visual theme — QLab-inspired, with Dark / Light / Black / Custom presets.
use crate::config::{AppearanceSettings, ThemePreset};
use egui::{Color32, CornerRadius, Shadow, Stroke, Style};
use once_cell::sync::Lazy;
use parking_lot::RwLock;

#[derive(Debug, Clone, Copy)]
struct ThemePalette {
    bg_dark: Color32,
    bg_panel: Color32,
    bg_row: Color32,
    bg_row_alt: Color32,
    bg_selected: Color32,
    bg_active: Color32,
    bg_paused: Color32,
    hovered: Color32,
    accent: Color32,
    accent2: Color32,
    text: Color32,
    text_dim: Color32,
    text_active: Color32,
    border: Color32,
    go_btn: Color32,
    stop_btn: Color32,
    pause_btn: Color32,
    wave_fill: Color32,
    wave_border: Color32,
    marker_start: Color32,
    marker_loop: Color32,
    loop_region: Color32,
    volume_automation: Color32,
    marker_end: Color32,
    dark_mode: bool,
}

impl ThemePalette {
    fn dark() -> Self {
        Self {
            bg_dark: Color32::from_rgb(18, 18, 22),
            bg_panel: Color32::from_rgb(26, 26, 32),
            bg_row: Color32::from_rgb(32, 32, 40),
            bg_row_alt: Color32::from_rgb(28, 28, 36),
            bg_selected: Color32::from_rgb(40, 80, 140),
            bg_active: Color32::from_rgb(20, 100, 60),
            bg_paused: Color32::from_rgb(100, 80, 20),
            hovered: Color32::from_rgb(50, 50, 65),
            accent: Color32::from_rgb(90, 170, 255),
            accent2: Color32::from_rgb(60, 220, 130),
            text: Color32::from_rgb(220, 220, 230),
            text_dim: Color32::from_rgb(150, 150, 165),
            text_active: Color32::from_rgb(80, 255, 140),
            border: Color32::from_rgb(50, 50, 65),
            go_btn: Color32::from_rgb(40, 180, 80),
            stop_btn: Color32::from_rgb(200, 50, 50),
            pause_btn: Color32::from_rgb(200, 160, 30),
            wave_fill: Color32::from_rgb(60, 140, 220),
            wave_border: Color32::from_rgb(90, 170, 255),
            marker_start: Color32::from_rgb(60, 220, 130),
            marker_loop: Color32::from_rgb(255, 180, 40),
            loop_region: Color32::from_rgb(75, 155, 245),
            volume_automation: Color32::from_rgb(80, 225, 175),
            marker_end: Color32::from_rgb(220, 60, 60),
            dark_mode: true,
        }
    }

    fn light() -> Self {
        Self {
            bg_dark: Color32::from_rgb(242, 244, 247),
            bg_panel: Color32::from_rgb(255, 255, 255),
            bg_row: Color32::from_rgb(248, 249, 251),
            bg_row_alt: Color32::from_rgb(237, 240, 244),
            bg_selected: Color32::from_rgb(210, 228, 248),
            bg_active: Color32::from_rgb(225, 244, 232),
            bg_paused: Color32::from_rgb(250, 241, 218),
            hovered: Color32::from_rgb(225, 232, 241),
            accent: Color32::from_rgb(40, 105, 190),
            accent2: Color32::from_rgb(30, 145, 90),
            text: Color32::from_rgb(32, 36, 42),
            text_dim: Color32::from_rgb(91, 100, 112),
            text_active: Color32::from_rgb(20, 125, 70),
            border: Color32::from_rgb(202, 209, 218),
            go_btn: Color32::from_rgb(35, 145, 75),
            stop_btn: Color32::from_rgb(185, 55, 55),
            pause_btn: Color32::from_rgb(170, 120, 20),
            wave_fill: Color32::from_rgb(70, 125, 195),
            wave_border: Color32::from_rgb(40, 105, 190),
            marker_start: Color32::from_rgb(30, 145, 90),
            marker_loop: Color32::from_rgb(210, 135, 25),
            loop_region: Color32::from_rgb(45, 115, 205),
            volume_automation: Color32::from_rgb(0, 145, 115),
            marker_end: Color32::from_rgb(190, 55, 55),
            dark_mode: false,
        }
    }

    fn black() -> Self {
        let mut palette = Self::dark();
        palette.bg_dark = Color32::BLACK;
        palette.bg_panel = Color32::from_rgb(8, 8, 8);
        palette.bg_row = Color32::from_rgb(18, 18, 18);
        palette.bg_row_alt = Color32::from_rgb(11, 11, 11);
        palette.bg_selected = Color32::from_rgb(42, 58, 78);
        palette.bg_active = Color32::from_rgb(20, 48, 30);
        palette.bg_paused = Color32::from_rgb(52, 42, 18);
        palette.hovered = Color32::from_rgb(35, 35, 35);
        palette.text = Color32::from_rgb(245, 245, 245);
        palette.text_dim = Color32::from_rgb(180, 180, 180);
        palette.border = Color32::from_rgb(48, 48, 48);
        palette.loop_region = Color32::from_rgb(75, 155, 245);
        palette.volume_automation = Color32::from_rgb(80, 225, 175);
        palette.dark_mode = true;
        palette
    }

    fn from_settings(settings: &AppearanceSettings) -> Self {
        match settings.preset {
            ThemePreset::Dark => Self::dark(),
            ThemePreset::Light => Self::light(),
            ThemePreset::Black => Self::black(),
            ThemePreset::Custom => {
                let mut palette = Self::dark();
                palette.bg_dark = rgb(settings.window_color);
                palette.bg_panel = rgb(settings.panel_color);
                palette.text = rgb(settings.text_color);
                palette.text_dim = rgb(settings.muted_text_color);
                palette.loop_region = rgb(settings.loop_region_color);
                palette.dark_mode = luminance(palette.bg_dark) < 128.0;
                let blend = |base: Color32, tint: Color32, amount: f32| mix(base, tint, amount);
                palette.bg_row = blend(palette.bg_panel, palette.text, 0.045);
                palette.bg_row_alt = blend(palette.bg_panel, palette.text, 0.075);
                palette.hovered = blend(palette.bg_panel, palette.text, 0.12);
                palette.border = blend(palette.bg_panel, palette.text, 0.20);
                palette.bg_selected = blend(palette.bg_panel, palette.accent, 0.28);
                palette.bg_active = blend(palette.bg_panel, palette.accent2, 0.16);
                palette.bg_paused = blend(palette.bg_panel, palette.pause_btn, 0.16);
                palette
            }
        }
    }
}

fn rgb(value: [u8; 3]) -> Color32 {
    Color32::from_rgb(value[0], value[1], value[2])
}

fn luminance(color: Color32) -> f32 {
    0.2126 * color.r() as f32 + 0.7152 * color.g() as f32 + 0.0722 * color.b() as f32
}

fn mix(a: Color32, b: Color32, amount: f32) -> Color32 {
    let t = amount.clamp(0.0, 1.0);
    let channel =
        |left: u8, right: u8| (left as f32 + (right as f32 - left as f32) * t).round() as u8;
    Color32::from_rgb(
        channel(a.r(), b.r()),
        channel(a.g(), b.g()),
        channel(a.b(), b.b()),
    )
}

static ACTIVE_PALETTE: Lazy<RwLock<ThemePalette>> = Lazy::new(|| RwLock::new(ThemePalette::dark()));

pub struct Theme;

impl Theme {
    fn palette() -> ThemePalette {
        *ACTIVE_PALETTE.read()
    }

    pub fn apply(ctx: &egui::Context, appearance: &AppearanceSettings) {
        let palette = ThemePalette::from_settings(appearance);
        *ACTIVE_PALETTE.write() = palette;

        let mut style = Style::default();
        let cr4 = CornerRadius::same(4);
        style.visuals.dark_mode = palette.dark_mode;
        style.visuals.window_corner_radius = CornerRadius::same(8);
        style.visuals.widgets.noninteractive.corner_radius = cr4;
        style.visuals.widgets.inactive.corner_radius = cr4;
        style.visuals.widgets.hovered.corner_radius = cr4;
        style.visuals.widgets.active.corner_radius = cr4;
        style.visuals.widgets.open.corner_radius = cr4;

        style.visuals.panel_fill = palette.bg_panel;
        style.visuals.window_fill = palette.bg_dark;
        style.visuals.extreme_bg_color = palette.bg_dark;
        style.visuals.faint_bg_color = palette.bg_row_alt;
        style.visuals.code_bg_color = palette.bg_dark;
        style.visuals.override_text_color = Some(palette.text);
        style.visuals.window_shadow = Shadow::NONE;
        style.visuals.window_stroke = Stroke::new(1.0_f32, palette.border);
        style.visuals.hyperlink_color = palette.accent;

        style.visuals.widgets.noninteractive.bg_fill = palette.bg_row;
        style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, palette.text_dim);
        style.visuals.widgets.inactive.bg_fill = palette.bg_row;
        style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, palette.text);
        style.visuals.widgets.hovered.bg_fill = palette.hovered;
        style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, palette.accent);
        style.visuals.widgets.active.bg_fill = palette.bg_selected;
        style.visuals.widgets.active.fg_stroke = Stroke::new(1.5_f32, palette.accent);
        style.visuals.widgets.open.bg_fill = palette.bg_panel;
        style.visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, palette.text);
        style.visuals.selection.bg_fill = palette.bg_selected;
        style.visuals.selection.stroke = Stroke::new(1.0_f32, palette.accent);

        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.window_margin = egui::Margin::same(8);
        ctx.set_style(style);
    }

    pub fn bg_dark() -> Color32 {
        Self::palette().bg_dark
    }
    pub fn bg_panel() -> Color32 {
        Self::palette().bg_panel
    }
    pub fn bg_row() -> Color32 {
        Self::palette().bg_row
    }
    pub fn bg_row_alt() -> Color32 {
        Self::palette().bg_row_alt
    }
    pub fn bg_selected() -> Color32 {
        Self::palette().bg_selected
    }
    pub fn bg_active() -> Color32 {
        Self::palette().bg_active
    }
    pub fn bg_paused() -> Color32 {
        Self::palette().bg_paused
    }
    pub fn accent() -> Color32 {
        Self::palette().accent
    }
    pub fn accent2() -> Color32 {
        Self::palette().accent2
    }
    pub fn text() -> Color32 {
        Self::palette().text
    }
    pub fn text_dim() -> Color32 {
        Self::palette().text_dim
    }
    pub fn text_active() -> Color32 {
        Self::palette().text_active
    }
    pub fn border() -> Color32 {
        Self::palette().border
    }
    pub fn go_btn() -> Color32 {
        Self::palette().go_btn
    }
    pub fn stop_btn() -> Color32 {
        Self::palette().stop_btn
    }
    pub fn pause_btn() -> Color32 {
        Self::palette().pause_btn
    }
    pub fn wave_fill() -> Color32 {
        Self::palette().wave_fill
    }
    pub fn wave_border() -> Color32 {
        Self::palette().wave_border
    }
    pub fn marker_start() -> Color32 {
        Self::palette().marker_start
    }
    pub fn marker_loop() -> Color32 {
        Self::palette().marker_loop
    }
    pub fn loop_region() -> Color32 {
        Self::palette().loop_region
    }
    pub fn volume_automation() -> Color32 {
        Self::palette().volume_automation
    }
    pub fn marker_end() -> Color32 {
        Self::palette().marker_end
    }

    /// Selected cues use a restrained 10%-opacity accent overlay.
    pub fn selected_overlay() -> Color32 {
        let accent = Self::accent();
        Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 26)
    }

    /// Playing cues use a neutral white overlay, distinct from cue tint and selection.
    pub fn playing_overlay() -> Color32 {
        Color32::from_white_alpha(26)
    }

    /// Keeps the playing state visible on light themes where a white overlay
    /// naturally has less contrast.
    pub fn playing_outline() -> Color32 {
        if Self::palette().dark_mode {
            Color32::from_white_alpha(90)
        } else {
            Color32::from_black_alpha(65)
        }
    }
}
