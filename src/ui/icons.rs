//! Theme-aware SVG icon assets and user icon overrides.

use crate::config::{AppearanceSettings, IconOverride, ThemePreset};
use egui::{
    Color32, Context, Pos2, Rect, Response, Sense, Stroke, StrokeKind, TextureHandle, TextureId,
    TextureOptions, Ui, Vec2,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use usvg::TreeParsing;

pub const BUILTIN_ICON_KEYS: &[(&str, &str)] = &[
    ("audio", "Audio"),
    ("comment", "Comment"),
    ("fade", "Fade (toolbar)"),
    ("fadeIn", "Fade In"),
    ("fadeOut", "Fade Out"),
    ("group", "Group"),
    ("midi", "MIDI"),
    ("notes", "Notes"),
    ("osc", "OSC"),
    ("pause", "Pause"),
    ("play", "Play"),
    ("settings", "Settings"),
    ("stop", "Stop"),
];

pub const TOOLBAR_ICON_ROLES: &[(&str, &str)] = &[
    ("audio", "Add Audio cue"),
    ("play", "Add Play cue"),
    ("pause", "Add Pause cue"),
    ("stop", "Add Stop cue"),
    ("fade", "Add Fade cue"),
    ("midi", "Add MIDI cue"),
    ("osc", "Add OSC cue"),
    ("group", "Add Group cue"),
    ("comment", "Add Comment cue"),
    ("notes", "Notes sidebar"),
    ("settings", "Settings"),
];

const TOOLBAR_BUTTON_ICON_INSET: f32 = 6.0;
const SVG_ICON_LOGICAL_SIZE: f32 = 38.0;
const SVG_SUPERSAMPLE_FACTOR: f32 = 4.0;
const MAX_SVG_TEXTURE_SIDE: u32 = 1024;

pub struct IconManager {
    textures: HashMap<String, TextureHandle>,
    failed_custom_paths: HashSet<String>,
}

impl IconManager {
    pub fn new(_ctx: &Context) -> Self {
        Self {
            textures: HashMap::new(),
            failed_custom_paths: HashSet::new(),
        }
    }

    /// Resolve a role to a built-in/custom icon and render it in the requested
    /// foreground color. SVGs are rasterized at 4x the physical display size,
    /// so the GPU only performs a small, high-quality downscale at paint time.
    pub fn texture_id_for_color(
        &mut self,
        ctx: &Context,
        role: &str,
        appearance: &AppearanceSettings,
        color: Color32,
    ) -> Option<TextureId> {
        let selected = if appearance.preset == ThemePreset::Custom {
            appearance.icon_overrides.get(role)
        } else {
            None
        };

        if let Some(IconOverride::CustomFile(path)) = selected {
            if let Some(texture_id) = self.custom_texture_id(ctx, path) {
                return Some(texture_id);
            }
        }

        let builtin_key = match selected {
            Some(IconOverride::Builtin(key)) if valid_builtin_icon_key(key) => key.as_str(),
            _ => role,
        };

        self.builtin_texture_id(ctx, builtin_key, color)
            .or_else(|| self.builtin_texture_id(ctx, role, color))
    }

    fn builtin_texture_id(
        &mut self,
        ctx: &Context,
        key: &str,
        color: Color32,
    ) -> Option<TextureId> {
        let normalized_key = normalize_icon_key(key)?;
        let target_side = svg_icon_target_side(ctx.pixels_per_point());
        let cache_key = format!(
            "svg:{normalized_key}:{:02x}{:02x}{:02x}:{}",
            color.r(),
            color.g(),
            color.b(),
            target_side
        );

        if !self.textures.contains_key(&cache_key) {
            let bytes = svg_icon_bytes(normalized_key)?;
            let texture = render_svg_texture(ctx, &cache_key, bytes, color, target_side)?;
            self.textures.insert(cache_key.clone(), texture);
        }

        self.textures.get(&cache_key).map(|texture| texture.id())
    }

    fn custom_texture_id(&mut self, ctx: &Context, path: &str) -> Option<TextureId> {
        let cache_key = format!("custom:{}", path);
        if self.failed_custom_paths.contains(path) {
            return None;
        }
        if !self.textures.contains_key(&cache_key) {
            let texture = std::fs::read(path)
                .ok()
                .and_then(|bytes| decode_raster_texture(ctx, &cache_key, &bytes));
            if let Some(texture) = texture {
                self.textures.insert(cache_key.clone(), texture);
            } else {
                self.failed_custom_paths.insert(path.to_string());
                tracing::warn!(
                    "Could not load custom toolbar icon '{}'; using the theme default",
                    path
                );
                return None;
            }
        }
        self.textures.get(&cache_key).map(|texture| texture.id())
    }
}

fn valid_builtin_icon_key(key: &str) -> bool {
    BUILTIN_ICON_KEYS.iter().any(|(known, _)| *known == key)
}

fn normalize_icon_key(key: &str) -> Option<&'static str> {
    match key {
        "audio" => Some("audio"),
        "comment" => Some("comment"),
        "fade" => Some("fadeOut"),
        "fadeIn" => Some("fadeIn"),
        "fadeOut" => Some("fadeOut"),
        "group" => Some("group"),
        "midi" => Some("midi"),
        "notes" => Some("notes"),
        "running" => Some("running"),
        "osc" => Some("osc"),
        "pause" => Some("pause"),
        "play" => Some("play"),
        "settings" => Some("settings"),
        "stop" => Some("stop"),
        _ => None,
    }
}

/// The top-toolbar button fill and SVG foreground are deliberately inverted:
/// Light uses black buttons with white glyphs; Dark/Black use white buttons
/// with black glyphs. Custom uses its own selected button fill and chooses a
/// contrasting black or white SVG glyph from that fill's luminance.
pub fn toolbar_icon_colors(appearance: &AppearanceSettings) -> (Color32, Color32) {
    match appearance.preset {
        ThemePreset::Light => (Color32::BLACK, Color32::WHITE),
        ThemePreset::Dark | ThemePreset::Black => (Color32::WHITE, Color32::BLACK),
        ThemePreset::Custom => {
            let [red, green, blue] = appearance.toolbar_button_color;
            let fill = Color32::from_rgb(red, green, blue);
            let luminance = 0.2126 * red as f32 + 0.7152 * green as f32 + 0.0722 * blue as f32;
            let glyph = if luminance < 145.0 {
                Color32::WHITE
            } else {
                Color32::BLACK
            };
            (fill, glyph)
        }
    }
}

fn svg_icon_bytes(key: &str) -> Option<&'static [u8]> {
    Some(match key {
        "audio" => &include_bytes!("../../assets/icons/audio.svg")[..],
        "comment" => &include_bytes!("../../assets/icons/comment.svg")[..],
        "fadeIn" => &include_bytes!("../../assets/icons/fadeIn.svg")[..],
        "fadeOut" => &include_bytes!("../../assets/icons/fadeOut.svg")[..],
        "group" => &include_bytes!("../../assets/icons/group.svg")[..],
        "midi" => &include_bytes!("../../assets/icons/midi.svg")[..],
        "notes" => &include_bytes!("../../assets/icons/notes.svg")[..],
        "running" => &include_bytes!("../../assets/icons/running.svg")[..],
        "osc" => &include_bytes!("../../assets/icons/osc.svg")[..],
        "pause" => &include_bytes!("../../assets/icons/pause.svg")[..],
        "play" => &include_bytes!("../../assets/icons/play.svg")[..],
        "settings" => &include_bytes!("../../assets/icons/settings.svg")[..],
        "stop" => &include_bytes!("../../assets/icons/stop.svg")[..],
        _ => return None,
    })
}

fn svg_icon_target_side(pixels_per_point: f32) -> u32 {
    (SVG_ICON_LOGICAL_SIZE * pixels_per_point * SVG_SUPERSAMPLE_FACTOR)
        .round()
        .clamp(152.0, MAX_SVG_TEXTURE_SIDE as f32) as u32
}

fn render_svg_texture(
    ctx: &Context,
    name: &str,
    bytes: &[u8],
    color: Color32,
    target_side: u32,
) -> Option<TextureHandle> {
    let usvg_tree = usvg::Tree::from_data(bytes, &usvg::Options::default()).ok()?;
    let source_size = usvg_tree.size;
    let source_width = source_size.width();
    let source_height = source_size.height();
    if source_width <= 0.0 || source_height <= 0.0 {
        return None;
    }

    let mut pixmap = tiny_skia::Pixmap::new(target_side, target_side)?;
    let scale_x = target_side as f32 / source_width;
    let scale_y = target_side as f32 / source_height;

    // resvg 0.37 renders its own optimized tree, converted from the parsed usvg tree.
    let render_tree = resvg::Tree::from_usvg(&usvg_tree);
    render_tree.render(
        tiny_skia::Transform::from_scale(scale_x, scale_y),
        &mut pixmap.as_mut(),
    );

    // resvg/tiny-skia output is premultiplied RGBA. Use its alpha coverage
    // and apply the requested UI color as unpremultiplied RGB for egui.
    let mut rgba = Vec::with_capacity((target_side * target_side * 4) as usize);
    for pixel in pixmap.data().chunks_exact(4) {
        let alpha = pixel[3];
        rgba.extend_from_slice(&[color.r(), color.g(), color.b(), alpha]);
    }

    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        [target_side as usize, target_side as usize],
        &rgba,
    );
    Some(ctx.load_texture(name.to_string(), color_image, TextureOptions::LINEAR))
}

fn decode_raster_texture(ctx: &Context, name: &str, bytes: &[u8]) -> Option<TextureHandle> {
    let source = image::load_from_memory(bytes).ok()?.to_rgba8();
    let target_side = svg_icon_target_side(ctx.pixels_per_point());
    let filtered = downsample_icon(&source, target_side);
    let width = filtered.width();
    let height = filtered.height();
    if width == 0 || height == 0 {
        return None;
    }

    let side = width.max(height);
    let mut square = image::RgbaImage::new(side, side);
    image::imageops::overlay(
        &mut square,
        &filtered,
        ((side - width) / 2) as i64,
        ((side - height) / 2) as i64,
    );
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([side as usize, side as usize], square.as_raw());
    Some(ctx.load_texture(name.to_string(), color_image, TextureOptions::LINEAR))
}

/// Downsample a user-provided raster icon with premultiplied-alpha Lanczos3.
fn downsample_icon(source: &image::RgbaImage, max_side: u32) -> image::RgbaImage {
    let source_width = source.width();
    let source_height = source.height();
    let source_side = source_width.max(source_height);
    if source_side == 0 || source_side <= max_side {
        return source.clone();
    }

    let scale = max_side as f64 / source_side as f64;
    let target_width = ((source_width as f64 * scale).round() as u32).max(1);
    let target_height = ((source_height as f64 * scale).round() as u32).max(1);

    let mut premultiplied = source.clone();
    for pixel in premultiplied.pixels_mut() {
        let alpha = pixel[3] as u16;
        for channel in 0..3 {
            pixel[channel] = ((pixel[channel] as u16 * alpha + 127) / 255).min(255) as u8;
        }
    }

    let mut resized = image::imageops::resize(
        &premultiplied,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );

    for pixel in resized.pixels_mut() {
        let alpha = pixel[3] as u32;
        if alpha == 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            continue;
        }

        for channel in 0..3 {
            pixel[channel] = ((pixel[channel] as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }

    resized
}

/// Copy an imported image into TaSQ's config directory. The stored path remains
/// valid if the user later moves or deletes the original source image.
pub fn import_custom_icon(source: &Path, role: &str) -> Result<String, String> {
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| "The selected image has no file extension".to_string())?;
    if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp") {
        return Err("Supported custom icon formats are PNG, JPG and WebP".to_string());
    }
    let metadata = std::fs::metadata(source)
        .map_err(|error| format!("Could not read selected image: {error}"))?;
    if metadata.len() > 12 * 1024 * 1024 {
        return Err("Custom icon files must be 12 MB or smaller".to_string());
    }
    let bytes =
        std::fs::read(source).map_err(|error| format!("Could not read selected image: {error}"))?;
    image::load_from_memory(&bytes)
        .map_err(|error| format!("The selected file is not a supported image: {error}"))?;

    let config_dir = dirs::config_dir()
        .ok_or_else(|| "Could not locate the user configuration directory".to_string())?;
    let destination_dir = config_dir.join("TheatreShowCue").join("custom_icons");
    std::fs::create_dir_all(&destination_dir)
        .map_err(|error| format!("Could not create custom icon folder: {error}"))?;

    let safe_role: String = role
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect();
    let safe_role = if safe_role.is_empty() {
        "icon".to_string()
    } else {
        safe_role
    };
    let file_name = format!("{}_{}.{}", safe_role, uuid::Uuid::new_v4(), extension);
    let destination = destination_dir.join(file_name);
    std::fs::copy(source, &destination)
        .map_err(|error| format!("Could not copy custom icon: {error}"))?;
    Ok(destination.to_string_lossy().into_owned())
}

/// Draw a square toolbar button with a 38x38 SVG glyph inset into its 42x42
/// clickable surface, leaving the contrasting button background visible.
pub fn icon_button(
    ui: &mut Ui,
    texture_id: TextureId,
    button_size: Vec2,
    fill: Color32,
    border: Color32,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(button_size, Sense::click());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, 6_u8, fill);
        if response.hovered() {
            let hover_tint = if fill == Color32::WHITE {
                Color32::from_rgba_unmultiplied(0, 0, 0, 14)
            } else {
                Color32::from_white_alpha(14)
            };
            ui.painter().rect_filled(rect, 6_u8, hover_tint);
        }
        ui.painter()
            .rect_stroke(rect, 6_u8, Stroke::new(1.0_f32, border), StrokeKind::Inside);
        let icon_size =
            (button_size.x.min(button_size.y) - TOOLBAR_BUTTON_ICON_INSET * 2.0).max(1.0);
        let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(icon_size));
        ui.painter().image(
            texture_id,
            icon_rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    response
}

/// A selectable lower-panel tab with an SVG glyph in place of its old emoji.
pub fn icon_selectable_label(
    ui: &mut Ui,
    texture_id: Option<TextureId>,
    label: &str,
    selected: bool,
) -> Response {
    // Five leading spaces reserve the old emoji's visual slot while keeping
    // egui's native selectable-label styling and hit target.
    let response = ui.selectable_label(selected, format!("     {label}"));
    if let Some(texture_id) = texture_id {
        let icon_rect = Rect::from_center_size(
            Pos2::new(response.rect.left() + 12.0, response.rect.center().y),
            Vec2::splat(16.0),
        );
        ui.painter().image(
            texture_id,
            icon_rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_button_colors_follow_the_requested_theme_contrast() {
        let mut appearance = AppearanceSettings::default();
        appearance.preset = ThemePreset::Dark;
        assert_eq!(
            toolbar_icon_colors(&appearance),
            (Color32::WHITE, Color32::BLACK)
        );

        appearance.preset = ThemePreset::Black;
        assert_eq!(
            toolbar_icon_colors(&appearance),
            (Color32::WHITE, Color32::BLACK)
        );

        appearance.preset = ThemePreset::Light;
        assert_eq!(
            toolbar_icon_colors(&appearance),
            (Color32::BLACK, Color32::WHITE)
        );
    }

    #[test]
    fn custom_toolbar_button_color_is_user_selected_with_contrasting_glyph() {
        let mut appearance = AppearanceSettings::default();
        appearance.preset = ThemePreset::Custom;
        appearance.toolbar_button_color = [24, 26, 32];
        assert_eq!(
            toolbar_icon_colors(&appearance),
            (Color32::from_rgb(24, 26, 32), Color32::WHITE)
        );

        appearance.toolbar_button_color = [238, 240, 244];
        assert_eq!(
            toolbar_icon_colors(&appearance),
            (Color32::from_rgb(238, 240, 244), Color32::BLACK)
        );
    }

    #[test]
    fn all_svg_assets_parse_and_have_valid_canvas_sizes() {
        for (key, _) in BUILTIN_ICON_KEYS {
            let Some(normalized) = normalize_icon_key(key) else {
                panic!("invalid built-in icon key: {key}");
            };
            let bytes =
                svg_icon_bytes(normalized).unwrap_or_else(|| panic!("missing SVG asset: {key}"));
            let tree = usvg::Tree::from_data(bytes, &usvg::Options::default())
                .unwrap_or_else(|error| panic!("invalid SVG {key}: {error}"));
            assert!(tree.size.width() > 0.0);
            assert!(tree.size.height() > 0.0);
        }
    }

    #[test]
    fn svg_icon_texture_size_is_four_times_the_physical_glyph_size() {
        assert_eq!(svg_icon_target_side(1.0), 152);
        assert_eq!(svg_icon_target_side(1.25), 190);
        assert_eq!(svg_icon_target_side(2.0), 304);
        assert_eq!(svg_icon_target_side(3.0), 456);
    }

    #[test]
    fn raster_downsampling_keeps_aspect_ratio_and_transparent_pixels_clean() {
        let mut source = image::RgbaImage::new(256, 128);
        for pixel in source.pixels_mut() {
            *pixel = image::Rgba([220, 80, 40, 255]);
        }

        let resized = downsample_icon(&source, 84);
        assert_eq!((resized.width(), resized.height()), (84, 42));
        assert_eq!(resized.get_pixel(10, 10), &image::Rgba([220, 80, 40, 255]));

        let mut alpha_source = image::RgbaImage::new(256, 256);
        for y in 0..256 {
            for x in 0..256 {
                let alpha = if (64..192).contains(&x) && (64..192).contains(&y) {
                    255
                } else {
                    0
                };
                alpha_source.put_pixel(x, y, image::Rgba([240, 30, 20, alpha]));
            }
        }

        let alpha_resized = downsample_icon(&alpha_source, 84);
        assert!(alpha_resized.pixels().any(|pixel| pixel[3] > 0));
        assert!(alpha_resized
            .pixels()
            .all(|pixel| { pixel[3] != 0 || (pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0) }));
    }
}
