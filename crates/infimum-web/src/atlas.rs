//! HiDPI Unicode Glyph Atlas built at startup via `fontdue`.
//!
//! Uses free, open-source system fonts (`Ubuntu-R`, `Ubuntu-B`, `UbuntuMono-R`,
//! with automatic per-glyph fallback to `DejaVuSans` for symbols like `₪`, `✓`, `⚙`).
//!
//! Layout:
//! - 3 font styles stacked vertically:
//!   - Style 0 (Regular): rows 0..11
//!   - Style 1 (Bold):    rows 11..22
//!   - Style 2 (Mono):    rows 22..33
//! - 16 columns x 11 rows = 176 glyph slots per style (`0..175`).
//! - Cell size: 64x64 px -> total texture size: 1024 x 2112 (R8Unorm).
//! - Glyphs are LEFT-ALIGNED with true horizontal bearing (`pad_x = 4 + xmin`)
//!   and exact TrueType `advance_width` metrics are exported in `advances_256`
//!   so capsules achieve Chrome/native-grade proportional spacing.

use fontdue::{Font, FontSettings};
#[cfg(not(target_arch = "wasm32"))]
use std::fs;

pub const ATLAS_COLS: u32 = 16;
pub const ATLAS_ROWS_PER_STYLE: u32 = 11;
pub const ATLAS_SLOTS_PER_STYLE: usize = (ATLAS_COLS * ATLAS_ROWS_PER_STYLE) as usize; // 176
pub const ATLAS_STYLES: u32 = 3;
pub const ATLAS_CELL_PX: u32 = 64;
pub const ATLAS_WIDTH: u32 = ATLAS_COLS * ATLAS_CELL_PX; // 1024
pub const ATLAS_HEIGHT: u32 = ATLAS_ROWS_PER_STYLE * ATLAS_STYLES * ATLAS_CELL_PX; // 2112
pub const ATLAS_FONT_PX: f32 = 48.0;

pub struct GlyphAtlas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>, // single-channel alpha (R8Unorm)
    /// Exact TrueType horizontal advance in 1/256ths of `size` for `[style][slot]`.
    pub advances_256: [[i32; ATLAS_SLOTS_PER_STYLE]; 3],
}

#[cfg(not(target_arch = "wasm32"))]
const EMBEDDED_FALLBACK_FONT: &[u8] = include_bytes!("../assets/fallback.ttf");

#[cfg(not(target_arch = "wasm32"))]
fn win_font(name: &str) -> String {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
    format!("{windir}\\Fonts\\{name}")
}

#[cfg(not(target_arch = "wasm32"))]
fn try_load_font_with_fallbacks(candidates: &[String]) -> Option<Font> {
    for path in candidates {
        if let Ok(bytes) = fs::read(path) {
            if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                return Some(font);
            }
        }
    }
    None
}

/// Map atlas slot `0..175` back to its Unicode character.
pub fn atlas_slot_to_char(slot: u32) -> Option<char> {
    match slot {
        0..=94 => char::from_u32(32 + slot),
        96..=127 => char::from_u32(0x0410 + (slot - 96)),
        128..=159 => char::from_u32(0x0430 + (slot - 128)),
        160 => Some('Ё'),
        161 => Some('ё'),
        162 => Some('₪'),
        163 => Some('✓'),
        164 => Some('№'),
        165 => Some('—'),
        166 => Some('•'),
        167 => Some('→'),
        168 => Some('■'),
        169 => Some('⚙'),
        170 => Some('€'),
        _ => None,
    }
}

impl GlyphAtlas {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn build_system_atlas() -> Self {
        let regular_candidates = vec![
            // Linux (prefer static TTFs before variable-font Ubuntu symlinks)
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf".to_string(),
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".to_string(),
            "/usr/share/fonts/TTF/DejaVuSans.ttf".to_string(),
            "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf".to_string(),
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf".to_string(),
            // macOS
            "/System/Library/Fonts/Supplemental/Arial.ttf".to_string(),
            "/System/Library/Fonts/Supplemental/Verdana.ttf".to_string(),
            "/Library/Fonts/Arial.ttf".to_string(),
            "/System/Library/Fonts/Helvetica.ttc".to_string(),
            // Windows
            win_font("segoeui.ttf"),
            win_font("arial.ttf"),
            win_font("calibri.ttf"),
        ];
        let bold_candidates = vec![
            // Linux (static bold TTFs before variable-font Ubuntu-B.ttf)
            "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf".to_string(),
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf".to_string(),
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf".to_string(),
            "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf".to_string(),
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-B.ttf".to_string(),
            // macOS
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf".to_string(),
            "/System/Library/Fonts/Supplemental/Verdana Bold.ttf".to_string(),
            "/Library/Fonts/Arial Bold.ttf".to_string(),
            // Windows
            win_font("segoeuib.ttf"),
            win_font("arialbd.ttf"),
            win_font("calibrib.ttf"),
        ];
        let mono_candidates = vec![
            // Linux
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf".to_string(),
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf".to_string(),
            "/usr/share/fonts/truetype/ubuntu/UbuntuMono-R.ttf".to_string(),
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf".to_string(),
            "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf".to_string(),
            // macOS
            "/System/Library/Fonts/Supplemental/Courier New.ttf".to_string(),
            "/System/Library/Fonts/Supplemental/Andale Mono.ttf".to_string(),
            "/Library/Fonts/Courier New.ttf".to_string(),
            "/System/Library/Fonts/Menlo.ttc".to_string(),
            // Windows
            win_font("consola.ttf"),
            win_font("lucon.ttf"),
            win_font("cour.ttf"),
        ];
        let symbol_candidates = vec![
            // Linux
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".to_string(),
            "/usr/share/fonts/TTF/DejaVuSans.ttf".to_string(),
            "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf".to_string(),
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf".to_string(),
            // macOS
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf".to_string(),
            "/System/Library/Fonts/Supplemental/Arial.ttf".to_string(),
            // Windows
            win_font("seguisym.ttf"),
            win_font("segoeui.ttf"),
            win_font("arial.ttf"),
        ];

        let regular = try_load_font_with_fallbacks(&regular_candidates).unwrap_or_else(|| {
            Font::from_bytes(EMBEDDED_FALLBACK_FONT, FontSettings::default())
                .expect("Embedded fallback TrueType font must be valid")
        });
        let bold = try_load_font_with_fallbacks(&bold_candidates);
        let mono = try_load_font_with_fallbacks(&mono_candidates);
        let symbol_fallback = try_load_font_with_fallbacks(&symbol_candidates);

        Self::build_from_fonts(
            &regular,
            bold.as_ref().unwrap_or(&regular),
            mono.as_ref().unwrap_or(&regular),
            symbol_fallback.as_ref().unwrap_or(&regular),
        )
    }

    #[allow(dead_code)]
    pub fn build_from_font_bytes(regular_bytes: &[u8]) -> Result<Self, String> {
        Self::build_from_font_styles(regular_bytes, None, None)
    }

    /// Build a 3-style (`Regular`, `Bold`, `Mono`) Unicode glyph atlas from raw TrueType bytes.
    /// Any omitted or empty `bold_bytes` / `mono_bytes` gracefully falls back to `regular_bytes`.
    #[allow(dead_code)]
    pub fn build_from_font_styles(
        regular_bytes: &[u8],
        bold_bytes: Option<&[u8]>,
        mono_bytes: Option<&[u8]>,
    ) -> Result<Self, String> {
        let regular = Font::from_bytes(regular_bytes, FontSettings::default())
            .map_err(|e| format!("Invalid Regular TrueType font: {e}"))?;
        let bold = match bold_bytes.filter(|b| !b.is_empty()) {
            Some(bytes) => Some(
                Font::from_bytes(bytes, FontSettings::default())
                    .map_err(|e| format!("Invalid Bold TrueType font: {e}"))?,
            ),
            None => None,
        };
        let mono = match mono_bytes.filter(|b| !b.is_empty()) {
            Some(bytes) => Some(
                Font::from_bytes(bytes, FontSettings::default())
                    .map_err(|e| format!("Invalid Mono TrueType font: {e}"))?,
            ),
            None => None,
        };
        Ok(Self::build_from_fonts(
            &regular,
            bold.as_ref().unwrap_or(&regular),
            mono.as_ref().unwrap_or(&regular),
            &regular,
        ))
    }

    fn build_from_fonts(regular: &Font, bold: &Font, mono: &Font, symbol_fallback: &Font) -> Self {
        let fonts = [regular, bold, mono];
        let mut pixels = vec![0u8; (ATLAS_WIDTH * ATLAS_HEIGHT) as usize];
        let mut advances_256 = [[140i32; ATLAS_SLOTS_PER_STYLE]; 3];
        let baseline_y = 48i32;

        for (style_idx, primary_font) in fonts.iter().enumerate() {
            let style_row_offset = (style_idx as u32) * ATLAS_ROWS_PER_STYLE;
            for slot in 0..(ATLAS_COLS * ATLAS_ROWS_PER_STYLE) {
                let Some(ch) = atlas_slot_to_char(slot) else {
                    continue;
                };
                // Pick primary font if it contains the glyph, else fall back to DejaVuSans (for ₪, ✓, ⚙, etc.)
                let font = if primary_font.lookup_glyph_index(ch) != 0 {
                    *primary_font
                } else {
                    symbol_fallback
                };

                let (metrics, bitmap) = font.rasterize(ch, ATLAS_FONT_PX);
                let adv_norm = ((metrics.advance_width / ATLAS_FONT_PX) * 256.0).round() as i32;
                advances_256[style_idx][slot as usize] = adv_norm.clamp(48, 340);

                if ch == ' ' || metrics.width == 0 || metrics.height == 0 {
                    continue;
                }

                let col = slot % ATLAS_COLS;
                let row = style_row_offset + (slot / ATLAS_COLS);
                let cell_x = (col * ATLAS_CELL_PX) as i32;
                let cell_y = (row * ATLAS_CELL_PX) as i32;

                // Left-align glyph inside cell using true horizontal bearing (`xmin`) — NO centering jitter!
                let pad_x = 3 + metrics.xmin.clamp(-2, 10);
                let top_y = baseline_y - (metrics.height as i32) - metrics.ymin;

                for gy in 0..metrics.height {
                    let dst_y = cell_y + top_y + (gy as i32);
                    if dst_y < cell_y || dst_y >= cell_y + (ATLAS_CELL_PX as i32) {
                        continue;
                    }
                    for gx in 0..metrics.width {
                        let dst_x = cell_x + pad_x + (gx as i32);
                        if dst_x < cell_x || dst_x >= cell_x + (ATLAS_CELL_PX as i32) {
                            continue;
                        }
                        let alpha = bitmap[gy * metrics.width + gx];
                        let idx = (dst_y as u32 * ATLAS_WIDTH + dst_x as u32) as usize;
                        pixels[idx] = alpha;
                    }
                }
            }
        }

        Self {
            width: ATLAS_WIDTH,
            height: ATLAS_HEIGHT,
            pixels,
            advances_256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_cyrillic_and_shekel_atlas() {
        let atlas = GlyphAtlas::build_system_atlas();
        assert_eq!(atlas.width, 1024);
        assert_eq!(atlas.height, 2112);
        let non_zero = atlas.pixels.iter().filter(|&&b| b > 20).count();
        assert!(non_zero > 10_000, "Expected rasterized glyph pixels, got {non_zero}");
        // Regular: narrow 'i' must have smaller advance than wide 'W'
        let adv_i = atlas.advances_256[0][('i' as usize) - 32];
        let adv_w = atlas.advances_256[0][('W' as usize) - 32];
        assert!(adv_i < adv_w, "Expected Regular 'i' ({adv_i}) < 'W' ({adv_w})");
        // Mono: 'i' and 'W' must share fixed-width advance
        let mono_i = atlas.advances_256[2][('i' as usize) - 32];
        let mono_w = atlas.advances_256[2][('W' as usize) - 32];
        assert_eq!(mono_i, mono_w, "Expected Mono 'i' ({mono_i}) == 'W' ({mono_w})");
        // Bold pixels (rows 11..22) must differ from Regular pixels (rows 0..11)
        let style_bytes = (ATLAS_WIDTH * ATLAS_ROWS_PER_STYLE * ATLAS_CELL_PX) as usize;
        let reg_slice = &atlas.pixels[0..style_bytes];
        let bold_slice = &atlas.pixels[style_bytes..2 * style_bytes];
        assert_ne!(reg_slice, bold_slice, "Expected Bold style to differ visually from Regular");
    }

    #[test]
    fn build_from_font_styles_supports_optional_and_embedded_fallback() {
        let fallback_only = GlyphAtlas::build_from_font_styles(EMBEDDED_FALLBACK_FONT, None, None)
            .expect("Embedded fallback font must build");
        assert_eq!(fallback_only.width, 1024);
        assert_eq!(fallback_only.height, 2112);
        let non_zero = fallback_only.pixels.iter().filter(|&&b| b > 20).count();
        assert!(non_zero > 10_000, "Expected rasterized fallback pixels, got {non_zero}");
    }
}
