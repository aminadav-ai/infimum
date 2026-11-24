//! Native OS clipboard bridge (UTF-8 text + RGBA/PNG screenshot images) + image file loader.

use arboard::Clipboard;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct PastedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub mime_type: String,
    pub base64_data: String,
}

pub struct NativeClipboard {
    inner: Option<Clipboard>,
}

impl NativeClipboard {
    pub fn new() -> Self {
        Self {
            inner: Clipboard::new().ok(),
        }
    }

    pub fn read_text(&mut self) -> Option<String> {
        if let Some(cb) = self.inner.as_mut() {
            if let Ok(s) = cb.get_text() {
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        // Wayland fallback (`wl-paste`) for synthetic uinput Shift+Insert right after `wl-copy`
        if let Ok(out) = Command::new("wl-paste")
            .args(["--no-newline", "--type", "text/plain;charset=utf-8"])
            .output()
        {
            if out.status.success() && !out.stdout.is_empty() {
                let s = String::from_utf8_lossy(&out.stdout).into_owned();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        if let Ok(out) = Command::new("wl-paste").arg("--no-newline").output() {
            if out.status.success() && !out.stdout.is_empty() {
                let s = String::from_utf8_lossy(&out.stdout).into_owned();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        // X11 fallback (`xclip`)
        if let Ok(out) = Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .output()
        {
            if out.status.success() && !out.stdout.is_empty() {
                let s = String::from_utf8_lossy(&out.stdout).into_owned();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        None
    }

    pub fn write_text(&mut self, text: &str) -> bool {
        if let Some(cb) = self.inner.as_mut() {
            cb.set_text(text.to_string()).is_ok()
        } else {
            false
        }
    }

    /// Read screenshot or copied image from the OS clipboard (`arboard` + Wayland `wl-paste` + X11 `xclip` + file URI fallback).
    pub fn read_image(&mut self) -> Option<PastedImage> {
        // 1. Try native `arboard` image first
        if let Some(cb) = self.inner.as_mut() {
            if let Ok(img) = cb.get_image() {
                if img.width > 0 && img.height > 0 {
                    let w = img.width as u32;
                    let h = img.height as u32;
                    let rgba = img.bytes.into_owned();
                    if let Some(png_bytes) = encode_rgba_to_png(w, h, &rgba) {
                        return Some(PastedImage {
                            width: w,
                            height: h,
                            rgba,
                            mime_type: "image/png".to_string(),
                            base64_data: encode_base64(&png_bytes),
                        });
                    }
                }
            }
        }

        // 2. Wayland fallback (`wl-paste`) — fast TARGETS check so text pastes stay instant
        if let Ok(types_out) = Command::new("wl-paste").arg("--list-types").output() {
            if types_out.status.success() {
                let types = String::from_utf8_lossy(&types_out.stdout);
                for mime in ["image/png", "image/jpeg", "image/webp", "image/bmp"] {
                    if types.lines().any(|l| l.trim() == mime) {
                        if let Ok(out) = Command::new("wl-paste")
                            .args(["--no-newline", "--type", mime])
                            .output()
                        {
                            if out.status.success() && !out.stdout.is_empty() {
                                if let Some(p) = pasted_image_from_any_bytes(&out.stdout) {
                                    return Some(p);
                                }
                            }
                        }
                    }
                }
                if types.lines().any(|l| l.trim() == "text/uri-list") {
                    if let Ok(out) = Command::new("wl-paste")
                        .args(["--no-newline", "--type", "text/uri-list"])
                        .output()
                    {
                        if out.status.success() {
                            if let Some(path) = parse_image_uri_list(&out.stdout) {
                                if let Some(p) = load_image_file(&path) {
                                    return Some(p);
                                }
                            }
                        }
                    }
                }
            }
        }

        // 3. X11 fallback (`xclip`)
        if let Ok(targets_out) = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
            .output()
        {
            if targets_out.status.success() {
                let types = String::from_utf8_lossy(&targets_out.stdout);
                for mime in ["image/png", "image/jpeg", "image/webp", "image/bmp"] {
                    if types.lines().any(|l| l.trim() == mime) {
                        if let Ok(out) = Command::new("xclip")
                            .args(["-selection", "clipboard", "-t", mime, "-o"])
                            .output()
                        {
                            if out.status.success() && !out.stdout.is_empty() {
                                if let Some(p) = pasted_image_from_any_bytes(&out.stdout) {
                                    return Some(p);
                                }
                            }
                        }
                    }
                }
                if types.lines().any(|l| l.trim() == "text/uri-list") {
                    if let Ok(out) = Command::new("xclip")
                        .args(["-selection", "clipboard", "-t", "text/uri-list", "-o"])
                        .output()
                    {
                        if out.status.success() {
                            if let Some(path) = parse_image_uri_list(&out.stdout) {
                                if let Some(p) = load_image_file(&path) {
                                    return Some(p);
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }
}

fn parse_image_uri_list(raw: &[u8]) -> Option<PathBuf> {
    let s = String::from_utf8_lossy(raw);
    for line in s.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("file://") {
            let decoded = percent_decode(rest);
            let p = PathBuf::from(decoded);
            if is_supported_image_ext(&p) && p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn is_supported_image_ext(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
    )
}

pub fn pick_image_files_dialog() -> Vec<PathBuf> {
    let Ok(out) = Command::new("zenity")
        .args([
            "--file-selection",
            "--multiple",
            "--separator=\n",
            "--title=Прикрепить изображения (можно выбрать несколько)",
            "--file-filter=Изображения (PNG, JPG, WEBP, GIF, BMP) | *.png *.PNG *.jpg *.JPG *.jpeg *.JPEG *.webp *.WEBP *.gif *.GIF *.bmp *.BMP",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .collect()
}

pub fn pick_folder_dialog(initial_dir: Option<&Path>) -> Option<PathBuf> {
    let mut cmd = Command::new("zenity");
    cmd.args([
        "--file-selection",
        "--directory",
        "--title=Добавить папку проекта в Infimum Code",
    ]);
    if let Some(dir) = initial_dir {
        if dir.is_dir() {
            let mut s = dir.to_string_lossy().to_string();
            if !s.ends_with('/') {
                s.push('/');
            }
            cmd.arg(format!("--filename={s}"));
        }
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    if p.is_dir() { Some(p) } else { None }
}

pub fn load_image_file(path: &Path) -> Option<PastedImage> {
    let raw = std::fs::read(path).ok()?;
    if raw.is_empty() || raw.len() > 25 * 1024 * 1024 {
        return None;
    }
    if raw.starts_with(b"\x89PNG\r\n\x1a\n") {
        return pasted_image_from_png_bytes(&raw);
    }
    // Convert JPEG/WEBP/GIF/BMP into PNG via Python PIL for GPU preview + Gemini compatibility
    let py_code = "import sys, io; from PIL import Image; im = Image.open(sys.argv[1]).convert('RGBA'); im.thumbnail((2048, 2048)); buf = io.BytesIO(); im.save(buf, format='PNG'); sys.stdout.buffer.write(buf.getvalue())";
    let out = Command::new("python3")
        .args(["-c", py_code, &path.to_string_lossy()])
        .output()
        .ok()?;
    if out.status.success() && out.stdout.starts_with(b"\x89PNG\r\n\x1a\n") {
        return pasted_image_from_png_bytes(&out.stdout);
    }
    None
}

fn pasted_image_from_any_bytes(raw: &[u8]) -> Option<PastedImage> {
    if raw.is_empty() || raw.len() > 25 * 1024 * 1024 {
        return None;
    }
    if raw.starts_with(b"\x89PNG\r\n\x1a\n") {
        return pasted_image_from_png_bytes(raw);
    }
    let py_code = "import sys, io; from PIL import Image; im = Image.open(io.BytesIO(sys.stdin.buffer.read())).convert('RGBA'); im.thumbnail((2048, 2048)); buf = io.BytesIO(); im.save(buf, format='PNG'); sys.stdout.buffer.write(buf.getvalue())";
    let mut child = Command::new("python3")
        .args(["-c", py_code])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(raw);
    }
    let out = child.wait_with_output().ok()?;
    if out.status.success() && out.stdout.starts_with(b"\x89PNG\r\n\x1a\n") {
        return pasted_image_from_png_bytes(&out.stdout);
    }
    None
}

fn pasted_image_from_png_bytes(png_bytes: &[u8]) -> Option<PastedImage> {
    let (w, h, rgba) = decode_png_to_rgba(png_bytes)?;
    Some(PastedImage {
        width: w,
        height: h,
        rgba,
        mime_type: "image/png".to_string(),
        base64_data: encode_base64(png_bytes),
    })
}

pub fn decode_png_to_rgba(png_bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let src = &buf[..info.buffer_size()];
    let w = info.width;
    let h = info.height;
    if w == 0 || h == 0 {
        return None;
    }
    let pixel_count = (w as usize) * (h as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => src.to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(pixel_count * 4);
            for chunk in src.as_chunks::<3>().0 {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(pixel_count * 4);
            for chunk in src.as_chunks::<2>().0 {
                out.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(pixel_count * 4);
            for &g in src {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        _ => return None,
    };
    if rgba.len() < pixel_count * 4 {
        return None;
    }
    Some((w, h, rgba))
}

pub fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || rgba.len() < (width as usize) * (height as usize) * 4 {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer
            .write_image_data(&rgba[..(width as usize) * (height as usize) * 4])
            .ok()?;
    }
    Some(out)
}

pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity((input.len() * 3) / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for b in input.bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

pub fn decode_base64_image_rgba(b64: &str) -> Option<(u32, u32, Vec<u8>)> {
    let raw = decode_base64(b64)?;
    let img = pasted_image_from_any_bytes(&raw)?;
    Some((img.width, img.height, img.rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip_and_base64() {
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255,
            0, 0, 255, 255, 255, 255, 255, 128,
        ];
        let png = encode_rgba_to_png(2, 2, &rgba).expect("encode png");
        let (w, h, dec) = decode_png_to_rgba(&png).expect("decode png");
        assert_eq!((w, h), (2, 2));
        assert_eq!(dec, rgba);
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
        assert_eq!(encode_base64(b"a"), "YQ==");
        assert_eq!(encode_base64(b"ab"), "YWI=");
    }
}
