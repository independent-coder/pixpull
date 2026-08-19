//! Image format detection and integrity validation via magic bytes.
//!
//! We don't shell out to `file` or pull in a full decode library — magic-byte
//! sniffing is fast, dependency-free, and covers every format a scraper is
//! likely to hit (JPEG/PNG/GIF/WebP/BMP/TIFF/AVIF/HEIC/SVG/ICO).

use std::fmt;

/// A recognised image format with its canonical file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
    Gif,
    Webp,
    Bmp,
    Tiff,
    Avif,
    Heic,
    Svg,
    Ico,
}

impl Format {
    /// Canonical extension, without the leading dot.
    pub fn ext(&self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Png => "png",
            Format::Gif => "gif",
            Format::Webp => "webp",
            Format::Bmp => "bmp",
            Format::Tiff => "tif",
            Format::Avif => "avif",
            Format::Heic => "heic",
            Format::Svg => "svg",
            Format::Ico => "ico",
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ext())
    }
}

/// Sniff the image format from a byte buffer. Returns `None` if the bytes do
/// not look like any supported image format.
pub fn detect(buf: &[u8]) -> Option<Format> {
    if buf.len() < 4 {
        return None;
    }

    // JPEG: FF D8 FF
    if buf[0] == 0xFF && buf[1] == 0xD8 && buf[2] == 0xFF {
        return Some(Format::Jpeg);
    }

    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if buf.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(Format::Png);
    }

    // GIF: "GIF87a" / "GIF89a"
    if buf.starts_with(b"GIF8") {
        return Some(Format::Gif);
    }

    // BMP: "BM"
    if buf.starts_with(b"BM") {
        return Some(Format::Bmp);
    }

    // WebP: RIFF....WEBP
    if buf.starts_with(b"RIFF") && buf.len() >= 12 && &buf[8..12] == b"WEBP" {
        return Some(Format::Webp);
    }

    // TIFF: little (II) or big (MM) endian
    if buf.starts_with(b"II*\x00") || buf.starts_with(b"MM\x00*") {
        return Some(Format::Tiff);
    }

    // ICO: 00 00 01 00
    if buf.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some(Format::Ico);
    }

    // ISO BMFF containers (AVIF / HEIC / HEIF): "....ftyp<brand>"
    if buf.len() >= 12 && &buf[4..8] == b"ftyp" {
        let brand = &buf[8..12];
        const AVIF: &[&[u8]] = &[b"avif", b"avis"];
        const HEIC: &[&[u8]] = &[b"heic", b"heix", b"hevc", b"hevx", b"mif1", b"msf1"];
        if AVIF.contains(&brand) {
            return Some(Format::Avif);
        }
        if HEIC.contains(&brand) {
            return Some(Format::Heic);
        }
    }

    // SVG: optional BOM/whitespace, then '<', with "<svg" appearing early.
    if is_svg(buf) {
        return Some(Format::Svg);
    }

    None
}

fn is_svg(buf: &[u8]) -> bool {
    let head = &buf[..buf.len().min(512)];
    let mut i = 0;
    // Skip UTF-8 BOM and leading whitespace.
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        i += 3;
    }
    while i < head.len()
        && (head[i] == b' ' || head[i] == b'\t' || head[i] == b'\r' || head[i] == b'\n')
    {
        i += 1;
    }
    if i >= head.len() || head[i] != b'<' {
        return false;
    }
    // Look for "<svg" (case-insensitive) within the first chunk.
    let lower = head[i..].to_ascii_lowercase();
    lower.windows(4).any(|w| w == b"<svg")
}

/// Validate that a fully downloaded file is a real image. Returns the detected
/// format, or an error describing why the file is not a valid image.
pub fn validate(buf: &[u8]) -> Result<Format, String> {
    match detect(buf) {
        Some(f) => Ok(f),
        None => {
            let preview = buf
                .iter()
                .take(32)
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            Err(format!(
                "bytes do not match any known image format (first 32 bytes: {preview})"
            ))
        }
    }
}

/// Whether a file extension already present in a URL/filename reasonably
/// matches the sniffed format. Used to avoid appending a duplicate extension.
pub fn extension_matches(filename: &str, format: Format) -> bool {
    let Some((_, ext)) = filename.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    let accepted: &[&str] = match format {
        Format::Jpeg => &["jpg", "jpeg", "jpe", "jfif"],
        Format::Png => &["png"],
        Format::Gif => &["gif"],
        Format::Webp => &["webp"],
        Format::Bmp => &["bmp", "dib"],
        Format::Tiff => &["tif", "tiff"],
        Format::Avif => &["avif", "avifs"],
        Format::Heic => &["heic", "heif"],
        Format::Svg => &["svg", "svgz"],
        Format::Ico => &["ico", "cur"],
    };
    accepted.contains(&ext.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_formats() {
        assert_eq!(detect(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]), Some(Format::Jpeg));
        assert_eq!(detect(b"\x89PNG\r\n\x1a\n\x00\x00\x00"), Some(Format::Png));
        assert_eq!(detect(b"GIF89a...."), Some(Format::Gif));
        assert_eq!(detect(b"BM\x36\x00\x00\x00"), Some(Format::Bmp));
        assert_eq!(detect(b"RIFF\x00\x00\x00\x00WEBP"), Some(Format::Webp));
        assert_eq!(detect(b"II*\x00\x08\x00\x00\x00"), Some(Format::Tiff));
        assert_eq!(detect(b"\x00\x00\x01\x00\x01\x00"), Some(Format::Ico));
        assert_eq!(detect(b"\x00\x00\x00\x18ftypavif\x00"), Some(Format::Avif));
        assert_eq!(detect(b"\x00\x00\x00\x18ftypheic\x00"), Some(Format::Heic));
        assert_eq!(
            detect(b"<?xml version=\"1.0\"?><svg xmlns=..."),
            Some(Format::Svg)
        );
        assert_eq!(
            detect(b"\xEF\xBB\xBF<svg viewBox=\"0 0 1 1\">"),
            Some(Format::Svg)
        );
        assert_eq!(detect(b"<html><body>nope</body></html>"), None);
        assert_eq!(detect(b"hello world this is not an image"), None);
    }

    #[test]
    fn extension_matching() {
        assert!(extension_matches("photo.jpg", Format::Jpeg));
        assert!(extension_matches("photo.jpeg", Format::Jpeg));
        assert!(!extension_matches("photo.png", Format::Jpeg));
        // No extension at all => caller is expected to append one.
        assert!(!extension_matches("pic", Format::Png));
    }
}
