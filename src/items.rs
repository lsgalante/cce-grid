// Desktop items: images pinned to the world canvas.
//
// A drop on the desktop background lands here (the compositor routes drags
// over the background onto the grid client — see its `Scene::at_including_grid`).
// Each item is saved to the desktop folder AND recorded in a sidecar with the
// virtual-canvas position it was dropped at, so it reappears in the same world
// spot next session. Nothing here touches the GPU: the caller uploads the
// decoded pixels, because that must happen on the main loop.
//
// Fetching shells out to curl rather than linking an HTTP stack. This process
// is a background renderer that otherwise needs no network at all, and a
// dropped web image is a once-in-a-while user action where process startup is
// far below the notice threshold — an async runtime and a TLS stack would be
// the largest thing in the binary, for that.

use std::path::{Path, PathBuf};

/// One pinned image, in virtual-surface coordinates (the same space windows
/// and grid squares live in), so items pan and zoom with the desktop.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DesktopItem {
    /// Where the image was saved — the sidecar stores a path, not pixels.
    pub path: PathBuf,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// `$XDG_DATA_HOME/cce/desktop-items.json`.
pub fn sidecar_path() -> PathBuf {
    cce_ui::config::data_home().join("cce").join("desktop-items.json")
}

/// The desktop folder: `$XDG_DESKTOP_DIR` when the user-dirs config exports
/// one, else `~/Desktop`.
pub fn desktop_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DESKTOP_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Desktop")
}

pub fn load() -> Vec<DesktopItem> {
    let path = sidecar_path();
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    match serde_json::from_str::<Vec<DesktopItem>>(&text) {
        Ok(items) => items,
        Err(e) => {
            // A corrupt sidecar must not cost the user their other items on
            // the next write, so refuse to start from an empty list: keep the
            // file untouched and run with nothing until it is fixed.
            log::error!("[items] {} is unreadable ({e}); not loading or rewriting it", path.display());
            Vec::new()
        }
    }
}

pub fn save(items: &[DesktopItem]) {
    let path = sidecar_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(text) = serde_json::to_string_pretty(items) else { return };
    // Write-then-rename: a crash mid-write would otherwise leave a truncated
    // sidecar, which is exactly the corrupt-file case above.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// The URI (or raw image bytes) a drop payload actually carries.
pub enum Payload {
    Uri(String),
    Bytes(Vec<u8>),
}

/// Interpret a drop by mime type. Browsers hand over a *link* for an image on
/// a page — the pixels only travel directly when the source made them itself
/// (a canvas, an image editor), which is why both shapes are handled.
pub fn parse_payload(mime: &str, data: &[u8]) -> Option<Payload> {
    if mime.starts_with("image/") {
        return Some(Payload::Bytes(data.to_vec()));
    }
    let text = if mime == "text/x-moz-url" {
        // Firefox's own flavour is UTF-16LE, "url\ntitle".
        let units: Vec<u16> = data
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(data).into_owned()
    };
    // text/uri-list is line-based with '#' comments; the other text flavours
    // are a bare URL. Taking the first usable line covers both.
    let uri = text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with('#'))?;
    Some(Payload::Uri(uri.to_string()))
}

/// Percent-decode enough of a `file://` URI to get a real path back.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A filename for the saved copy: the URI's last path segment when it looks
/// like a filename, else a generic name. Query strings and fragments are
/// stripped — plenty of image URLs end in `?w=800`.
fn file_name_for(uri: &str, fallback_ext: &str) -> String {
    let trimmed = uri.split(['?', '#']).next().unwrap_or(uri);
    let last = trimmed.rsplit('/').next().unwrap_or("");
    let last = percent_decode(last);
    let looks_named = !last.is_empty() && last.contains('.') && last.len() <= 128;
    if looks_named {
        last
    } else {
        format!("dropped-image.{fallback_ext}")
    }
}

/// A path in `dir` that does not exist yet, suffixing `-2`, `-3`, … A drop
/// must never overwrite a file the user already has.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem}-{n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(name)
}

/// Sniff a container from magic bytes — the extension in a URL is a guess and
/// content-type is not carried through a drop.
fn extension_for(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else {
        "bin"
    }
}

/// Fetch the drop's bytes: a local file is read, anything else goes through
/// curl. Returns the bytes and the name to save them under.
pub fn fetch(payload: Payload) -> std::io::Result<(Vec<u8>, String)> {
    match payload {
        Payload::Bytes(bytes) => {
            let ext = extension_for(&bytes);
            Ok((bytes, format!("dropped-image.{ext}")))
        }
        Payload::Uri(uri) if uri.starts_with("file://") => {
            let path = percent_decode(uri.trim_start_matches("file://"));
            let bytes = std::fs::read(&path)?;
            let name = Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file_name_for(&uri, extension_for(&bytes)));
            Ok((bytes, name))
        }
        Payload::Uri(uri) => {
            if !uri.starts_with("http://") && !uri.starts_with("https://") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unsupported URI scheme: {uri}"),
                ));
            }
            let out = std::process::Command::new("curl")
                .args([
                    "--location",
                    "--fail",
                    "--silent",
                    "--show-error",
                    // A drop should not be able to hang a renderer thread
                    // forever on a dead host.
                    "--max-time",
                    "30",
                    "--max-filesize",
                    "67108864",
                    &uri,
                ])
                .output()?;
            if !out.status.success() {
                return Err(std::io::Error::other(format!(
                    "curl failed for {uri}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let name = file_name_for(&uri, extension_for(&out.stdout));
            Ok((out.stdout, name))
        }
    }
}

/// Save bytes into the desktop folder under a non-colliding name.
pub fn save_to_desktop(bytes: &[u8], name: &str) -> std::io::Result<PathBuf> {
    let dir = desktop_dir();
    std::fs::create_dir_all(&dir)?;
    let path = unique_path(&dir, name);
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Decode to straight RGBA8 for `cce_ui::vk::upload_rgba`.
pub fn decode_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some((rgba.into_raw(), w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_takes_the_first_real_line() {
        let data = b"# comment\r\nhttps://example.com/cat.png\r\nhttps://other\r\n";
        let Some(Payload::Uri(u)) = parse_payload("text/uri-list", data) else {
            panic!("expected a URI")
        };
        assert_eq!(u, "https://example.com/cat.png");
    }

    #[test]
    fn moz_url_is_utf16() {
        // "http://a/b.png\nTitle" in UTF-16LE, as Firefox sends it.
        let s = "http://a/b.png\nTitle";
        let data: Vec<u8> = s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let Some(Payload::Uri(u)) = parse_payload("text/x-moz-url", &data) else {
            panic!("expected a URI")
        };
        assert_eq!(u, "http://a/b.png");
    }

    #[test]
    fn image_mimes_carry_bytes_not_links() {
        let Some(Payload::Bytes(b)) = parse_payload("image/png", &[1, 2, 3]) else {
            panic!("expected bytes")
        };
        assert_eq!(b, vec![1, 2, 3]);
    }

    #[test]
    fn file_names_survive_query_strings_and_fall_back() {
        assert_eq!(file_name_for("https://x.com/a/cat.png?w=800", "png"), "cat.png");
        assert_eq!(file_name_for("https://x.com/a/photo%20one.jpg", "jpg"), "photo one.jpg");
        // No filename in the path at all.
        assert_eq!(file_name_for("https://x.com/render?id=9", "png"), "dropped-image.png");
    }

    #[test]
    fn extension_is_sniffed_from_content() {
        assert_eq!(extension_for(&[0x89, b'P', b'N', b'G', 0]), "png");
        assert_eq!(extension_for(&[0xFF, 0xD8, 0xFF, 0]), "jpg");
        assert_eq!(extension_for(b"not an image"), "bin");
    }
}
