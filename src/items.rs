// Desktop items: images pinned to the world canvas.
//
// A drop on the desktop background lands here (the compositor routes drags
// over the background onto the grid client — see its `Scene::at`, which hit-tests
// the grid layer through its input region since cce-compositor@b82a0ee).
// Each item is saved (into the vault's attachment folder with a vault, else
// the desktop folder) AND recorded on the board with the virtual-canvas
// position it was dropped at, so it reappears in the same world spot next
// session. Nothing here touches the GPU: the caller uploads the
// decoded pixels, because that must happen on the main loop.
//
// Fetching shells out to curl rather than linking an HTTP stack. This process
// is a background renderer that otherwise needs no network at all, and a
// dropped web image is a once-in-a-while user action where process startup is
// far below the notice threshold — an async runtime and a TLS stack would be
// the largest thing in the binary, for that.

use std::path::{Path, PathBuf};

/// What a desktop item is — one JSON Canvas node (see `board.rs`).
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// An image file, drawn as its pixels.
    Image(PathBuf),
    /// A Markdown note, drawn as a card of its content (MarkdownView).
    Note(PathBuf),
    /// Any other file: a card with its name.
    File(PathBuf),
    /// A text card: Markdown kept in the canvas itself.
    Text(String),
    Link(String),
    /// A labelled frame behind other items; not interactive.
    Group(String),
    /// A node type this client does not know, kept so a save returns it.
    Other(String),
}

/// A `file` node's kind, by extension.
pub fn kind_for_file(path: PathBuf) -> Kind {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Kind::Image(path),
        "md" => Kind::Note(path),
        _ => Kind::File(path),
    }
}

/// One pinned item, in virtual-surface coordinates (the same space windows
/// and grid squares live in), so items pan and zoom with the desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopItem {
    /// The canvas node id: the identity across sessions and in edges.
    pub node: String,
    pub kind: Kind,
    /// This process's name for the item in its reports to the compositor
    /// (`grid-items`), which addresses a group move's `move` lines by it.
    /// Assigned when the item enters the list (`GridApp::assign_id`).
    pub id: u64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// JSON Canvas colour (`"1"`–`"6"` or `#rrggbb`), kept as written.
    pub color: Option<String>,
}

impl DesktopItem {
    pub fn new(node: String, kind: Kind, x: f64, y: f64, w: f64, h: f64) -> DesktopItem {
        DesktopItem { node, kind, id: 0, x, y, w, h, color: None }
    }

    pub fn is_image(&self) -> bool {
        matches!(self.kind, Kind::Image(_))
    }

    /// Groups and unknown nodes take no input and are not reported to the
    /// compositor: a group spans a region the background must keep.
    pub fn interactive(&self) -> bool {
        !matches!(self.kind, Kind::Group(_) | Kind::Other(_))
    }

    /// A short name for menus and logs.
    pub fn name(&self) -> String {
        match &self.kind {
            Kind::Image(p) | Kind::Note(p) | Kind::File(p) => {
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            }
            Kind::Text(t) => {
                let first = t.lines().find(|l| !l.trim().is_empty()).unwrap_or("Text").trim();
                first.chars().take(40).collect()
            }
            Kind::Link(u) => u.clone(),
            Kind::Group(l) => l.clone(),
            Kind::Other(t) => t.clone(),
        }
    }
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

/// The URI (or raw image bytes) a drop payload actually carries.
pub enum Payload {
    Uri(String),
    Bytes(Vec<u8>),
}

/// The `src` of the first `<img>` in a fragment of HTML. Browsers offer
/// `text/html` alongside the URL flavours, and it is the only one that names
/// the IMAGE when the image is wrapped in a link — which is exactly how a
/// Google Images thumbnail is marked up, so `text/uri-list` there is the
/// result page, not the picture.
fn img_src(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(tag) = lower[from..].find("<img") {
        let tag = from + tag;
        let rest = &lower[tag..];
        let end = rest.find('>').map(|e| tag + e).unwrap_or(lower.len());
        if let Some(src) = lower[tag..end].find("src") {
            let after = tag + src + 3;
            let seg = &html[after..end.min(html.len())];
            // src = "..." | '...' | bare
            let seg = seg.trim_start().strip_prefix('=')?.trim_start();
            let value = match seg.chars().next() {
                Some('"') => seg[1..].split('"').next(),
                Some('\'') => seg[1..].split('\'').next(),
                _ => seg.split_whitespace().next(),
            };
            if let Some(v) = value {
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        from = end.max(tag + 4);
    }
    None
}

/// Interpret a drop by mime type. Browsers hand over a *link* for an image on
/// a page — the pixels only travel directly when the source made them itself
/// (a canvas, an image editor), which is why both shapes are handled.
pub fn parse_payload(mime: &str, data: &[u8]) -> Option<Payload> {
    if mime.starts_with("image/") {
        return Some(Payload::Bytes(data.to_vec()));
    }
    if mime.starts_with("text/html") {
        let html = String::from_utf8_lossy(data);
        return img_src(&html).map(Payload::Uri);
    }
    let text = if mime == "text/x-moz-url" {
        // Firefox's own flavour is UTF-16LE, "url\ntitle".
        let units: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[lo, hi]| u16::from_le_bytes([lo, hi]))
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

/// A `file://` URI's path part, percent-decoded.
pub fn percent_decode_path(s: &str) -> PathBuf {
    PathBuf::from(percent_decode(s))
}

/// A filename for the saved copy: the URI's last path segment when it looks
/// like a filename, else a generic name. Query strings and fragments are
/// stripped — plenty of image URLs end in `?w=800`.
fn file_name_for(uri: &str, fallback_ext: &str) -> String {
    let trimmed = uri.split(['?', '#']).next().unwrap_or(uri);
    let last = trimmed.rsplit('/').next().unwrap_or("");
    // Decoded, `%2F` is a slash again: `..%2F..%2Fx` named a path out of
    // the save folder. Only the plain name it ends in is kept.
    safe_file_name(&percent_decode(last))
        .filter(|n| n.contains('.') && n.len() <= 128)
        .unwrap_or_else(|| format!("dropped-image.{fallback_ext}"))
}

/// `name` as one plain file name: its last component (split at `/` and
/// `\\`), or `None` when that is empty, `.` or `..`. A name joined onto the
/// save folder must not leave it — a dropped web image chose its own name
/// from its URL, and a crafted one (`..%2F`, `%2Fhome%2F…`) wrote a file of
/// the page's choosing anywhere in the home folder.
pub fn safe_file_name(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("").replace('\0', "");
    let last = last.trim();
    (!last.is_empty() && last != "." && last != "..").then(|| last.to_string())
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
        // Inline data, the way Google Images serves its thumbnails. No fetch
        // to do — the bytes are in the URI.
        Payload::Uri(uri) if uri.starts_with("data:") => {
            let rest = &uri["data:".len()..];
            let (meta, body) = rest.split_once(',').ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed data: URI")
            })?;
            let bytes = if meta.ends_with(";base64") {
                decode_base64(body).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "bad base64 in data: URI")
                })?
            } else {
                percent_decode(body).into_bytes()
            };
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

/// Standard base64 (RFC 4648) to bytes, tolerating whitespace and padding.
/// Hand-rolled rather than pulled in: it is twenty lines, and the only user
/// is `data:` URIs.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        if c.is_ascii_whitespace() || c == b'=' {
            continue;
        }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Where a dropped image is kept. With the board in a vault: inside it, in
/// the folder Obsidian puts new attachments for `board_file` in
/// (`cce_vault::attachments`), so the picture syncs with the board and
/// Obsidian shows the node instead of a missing file. Without a vault: the
/// desktop folder, as before.
pub fn save_image(bytes: &[u8], name: &str, vault: Option<&Path>, board_file: &str) -> std::io::Result<PathBuf> {
    let Some(vault) = vault else { return save_to_desktop(bytes, name) };
    let name = safe_file_name(name).unwrap_or_else(|| "dropped-image".to_string());
    let dir = vault.join(cce_vault::attachments::folder(vault, board_file));
    std::fs::create_dir_all(&dir)?;
    let path = cce_vault::attachments::unique_path(&dir, &name);
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Copy an image pinned from outside the vault into it (see [`save_image`]),
/// leaving the original where it was. The new path, or `None` when it is
/// already inside, or cannot be read.
pub fn adopt_into_vault(path: &Path, vault: &Path, board_file: &str) -> Option<PathBuf> {
    if path.starts_with(vault) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    save_image(&bytes, &name, Some(vault), board_file)
        .map_err(|e| log::warn!("[board] copying {} into the vault: {e}", path.display()))
        .ok()
}

/// Save bytes into the desktop folder under a non-colliding name.
pub fn save_to_desktop(bytes: &[u8], name: &str) -> std::io::Result<PathBuf> {
    let name = safe_file_name(name).unwrap_or_else(|| "dropped-image".to_string());
    let dir = desktop_dir();
    std::fs::create_dir_all(&dir)?;
    let path = unique_path(&dir, &name);
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Resolve a cce binary that is installed beside this one.
///
/// This process runs as a systemd user service, whose PATH is
/// `/usr/local/bin:/usr/bin` — `~/.local/bin`, where every cce binary is
/// installed, is NOT on it. Spawning one by bare name therefore fails with
/// ENOENT under systemd while working perfectly from a shell or when the
/// compositor spawns it, which is exactly how the context menu shipped
/// broken: it worked in every test and never once on the real desktop.
fn de_bin(name: &str) -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join(name);
            if beside.exists() {
                return beside;
            }
        }
    }
    // Fall back to PATH: a dev build run straight out of target/ has no cce
    // binaries beside it, but does have them on PATH.
    std::path::PathBuf::from(name)
}

/// Show a context menu titled `title` with `entries` (action id, label) and
/// return the chosen action id, if any. Runs on a worker thread: it blocks
/// until the menu closes.
///
/// The menu is a `cce-cloud --json` popup, the same mechanism the desktop and
/// app context menus use, so it looks and behaves like every other menu in the
/// DE rather than something this client drew for itself. The pointer's screen
/// position has to be asked for — a client knows where its own surface was
/// touched, never where that is on the screen.
pub fn item_menu(title: &str, entries: &[(&str, &str)]) -> Option<String> {
    use std::io::Write;

    let loc = std::process::Command::new(de_bin("ccectl")).arg("pointer-location").output().ok()?;
    let loc = String::from_utf8_lossy(&loc.stdout);
    let coord = |key: &str| -> Option<i32> {
        loc.split_whitespace()
            .find_map(|t| t.strip_prefix(key))
            .and_then(|v| v.trim().parse::<f64>().ok())
            .map(|v| v.round() as i32)
    };
    let (x, y) = (coord("x=")?, coord("y=")?);

    let widgets: Vec<serde_json::Value> = entries
        .iter()
        .map(|(id, label)| serde_json::json!({"type": "button", "text": label, "id": id}))
        .collect();
    let layout = serde_json::json!({"pages": [{"title": title, "justify": "left", "widgets": widgets}]}).to_string();

    let mut child = std::process::Command::new(de_bin("cce-cloud"))
        .args(["--json", "-x", &x.to_string(), "-y", &y.to_string()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(layout.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    reply.get("button")?.as_str().map(|s| s.to_string())
}

/// Spawn `cmd` and reap it on a background thread, so the child never lingers
/// as a zombie once it exits. The same helper cce-mail, cce-files, cce-terminal
/// and cce-system-interface each keep; cce-ui's shared `process::spawn_detached`
/// went away in cce-ui 4e94236.
fn spawn_detached(mut cmd: std::process::Command) -> std::io::Result<()> {
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Hand a path or URL to its default app (`xdg-open`), detached.
pub fn open_externally(target: &str) {
    let mut open = std::process::Command::new("xdg-open");
    open.arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let _ = spawn_detached(open);
}

/// How long cce-notes may take to answer an `open`.
const NOTES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Show a note in cce-notes: hand it to the running instance over its
/// socket, or start one. On a thread of its own: callers are on the loop
/// that draws the desktop, and a cce-notes that took the connection but
/// did not answer froze it (the reply was awaited with no deadline).
pub fn open_in_notes(path: &Path) {
    let socket = cce_ui::ipc::socket_path("cce-notes");
    let path = path.to_path_buf();
    std::thread::spawn(move || open_in_notes_at(&socket, &path));
}

fn open_in_notes_at(socket: &str, path: &Path) {
    use std::io::{BufRead, BufReader, Write};
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(socket) {
        let _ = s.set_read_timeout(Some(NOTES_TIMEOUT));
        let _ = s.set_write_timeout(Some(NOTES_TIMEOUT));
        if s.write_all(format!("open {}\n", path.display()).as_bytes()).is_ok() {
            let mut reply = String::new();
            let _ = BufReader::new(s).read_line(&mut reply);
            return;
        }
    }
    let mut notes = std::process::Command::new(de_bin("cce-notes"));
    notes.arg("open").arg(path);
    let _ = spawn_detached(notes);
}

/// Tell the user something about the desktop board, as a notification.
pub fn notify(title: &str, body: &str) {
    let _ = std::process::Command::new(de_bin("ccectl"))
        .args(["notify", title, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Tell the user a drop failed, and why. A drop that silently does nothing
/// is indistinguishable from one the desktop never received, so every failure
/// path goes through here rather than only into the log.
pub fn report_failure(reason: &str) {
    log::warn!("[items] drop failed: {reason}");
    let _ = std::process::Command::new(de_bin("ccectl"))
        .args(["notify", "Image not added to the desktop", reason])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The largest texture side kept. An item is drawn about a grid cell big
/// (512 px by default); a full-size phone photo is ~48 MB of texture each,
/// and a side past the GPU's limit (often 16384) may not draw at all.
pub const MAX_TEX: u32 = 2048;
/// Image decodes running at once, at most: each decodes at full size first.
const MAX_DECODES: usize = 3;

/// A decoded image: straight RGBA8 at `tex` (for `cce_ui::vk::upload_rgba`),
/// and the image's own size, which places and sizes the item.
#[derive(Clone)]
pub struct Decoded {
    pub pixels: Vec<u8>,
    pub tex: (u32, u32),
    pub natural: (u32, u32),
}

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Decoded {{ tex: {:?}, natural: {:?} }}", self.tex, self.natural)
    }
}

/// Decode for a texture, shrunk to fit `MAX_TEX`. Off the UI thread: it
/// takes one of a few decode slots for its duration.
pub fn decode_texture(bytes: &[u8]) -> Option<Decoded> {
    let _slot = decode_slots().take();
    let img = image::load_from_memory(bytes).ok()?;
    let natural = (img.width(), img.height());
    let img = if natural.0.max(natural.1) > MAX_TEX {
        img.resize(MAX_TEX, MAX_TEX, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.into_rgba8();
    let tex = (rgba.width(), rgba.height());
    Some(Decoded { pixels: rgba.into_raw(), tex, natural })
}

/// A counting semaphore over decodes (`MAX_DECODES` slots).
struct Slots {
    free: std::sync::Mutex<usize>,
    freed: std::sync::Condvar,
}

struct SlotGuard<'a>(&'a Slots);

impl Slots {
    const fn new(n: usize) -> Slots {
        Slots { free: std::sync::Mutex::new(n), freed: std::sync::Condvar::new() }
    }

    fn take(&self) -> SlotGuard<'_> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            free = self.freed.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= 1;
        SlotGuard(self)
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.0.freed.notify_one();
    }
}

fn decode_slots() -> &'static Slots {
    static SLOTS: Slots = Slots::new(MAX_DECODES);
    &SLOTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_images_land_in_the_vault_attachment_folder() {
        let vault = tempfile::tempdir().unwrap();
        let v = vault.path();
        let p = save_image(b"png", "pic.png", Some(v), "Desktop.canvas").unwrap();
        assert_eq!(p, v.join("pic.png"));
        // A clash is numbered the way Obsidian numbers it.
        assert_eq!(save_image(b"png", "pic.png", Some(v), "Desktop.canvas").unwrap(), v.join("pic 1.png"));
        // Obsidian's attachment folder setting is followed.
        std::fs::create_dir_all(v.join(".obsidian")).unwrap();
        std::fs::write(v.join(".obsidian/app.json"), r#"{"attachmentFolderPath":"Attachments"}"#).unwrap();
        assert_eq!(save_image(b"png", "pic.png", Some(v), "Desktop.canvas").unwrap(), v.join("Attachments/pic.png"));
    }

    #[test]
    fn outside_images_are_copied_in_and_inside_ones_left() {
        let (vault, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let src = outside.path().join("shot.jpg");
        std::fs::write(&src, b"jpg").unwrap();
        let new = adopt_into_vault(&src, vault.path(), "Desktop.canvas").unwrap();
        assert_eq!(new, vault.path().join("shot.jpg"));
        assert_eq!(std::fs::read(&new).unwrap(), b"jpg");
        assert!(src.exists(), "the original stays");
        assert_eq!(adopt_into_vault(&new, vault.path(), "Desktop.canvas"), None);
        assert_eq!(adopt_into_vault(&outside.path().join("gone.jpg"), vault.path(), "Desktop.canvas"), None);
    }

    #[test]
    fn a_dropped_url_cannot_name_a_path_out_of_the_folder() {
        // `%2F` decodes to a slash: these named paths up and out.
        assert_eq!(file_name_for("https://evil.example/..%2F..%2F.config%2Fautostart%2Fx.desktop", "png"), "x.desktop");
        assert_eq!(file_name_for("https://evil.example/%2Fhome%2Fu%2F.local%2Flib%2Fevil.pth", "png"), "evil.pth");
        assert_eq!(file_name_for("https://evil.example/..%2F..", "png"), "dropped-image.png");
        assert_eq!(file_name_for("https://x.com/a%5C..%5Cb.png", "png"), "b.png");
        assert_eq!(safe_file_name("../../x.png").as_deref(), Some("x.png"));
        assert_eq!(safe_file_name(".."), None);
        assert_eq!(safe_file_name("/"), None);

        // And whatever a caller passes, a save stays in its folder.
        let vault = tempfile::tempdir().unwrap();
        let p = save_image(b"png", "../../escape.png", Some(vault.path()), "Desktop.canvas").unwrap();
        assert_eq!(p, vault.path().join("escape.png"));
        let p = save_image(b"png", "/tmp/abs.png", Some(vault.path()), "Desktop.canvas").unwrap();
        assert_eq!(p, vault.path().join("abs.png"));
    }

    #[test]
    fn opening_a_note_never_waits_on_a_silent_cce_notes() {
        // A listener that accepts and never answers.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("notes.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let started = std::time::Instant::now();
        let s = sock.to_string_lossy().into_owned();
        let t = std::thread::spawn(move || open_in_notes_at(&s, Path::new("/v/N.md")));
        let (_conn, _) = listener.accept().unwrap();
        t.join().unwrap();
        let took = started.elapsed();
        assert!(took >= NOTES_TIMEOUT && took < NOTES_TIMEOUT * 3, "gave up after {took:?}");
    }

    #[test]
    fn big_images_decode_to_a_capped_texture() {
        let mut png = Vec::new();
        image::RgbaImage::new(5000, 100)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let d = decode_texture(&png).unwrap();
        assert_eq!(d.natural, (5000, 100));
        assert_eq!(d.tex.0, MAX_TEX);
        assert_eq!(d.pixels.len(), (d.tex.0 * d.tex.1 * 4) as usize);
        let mut small = Vec::new();
        image::RgbaImage::new(30, 20).write_to(&mut std::io::Cursor::new(&mut small), image::ImageFormat::Png).unwrap();
        assert_eq!(decode_texture(&small).unwrap().tex, (30, 20));
    }

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
    fn html_flavour_names_the_image_not_the_link() {
        // A Google-Images-shaped fragment: the <img> is inside an <a>, so the
        // link URL is useless and only the img src names the picture.
        let html = br#"<a href="/imgres?q=cat"><img src="https://x.com/cat.png" alt="c"></a>"#;
        let Some(Payload::Uri(u)) = parse_payload("text/html", html) else {
            panic!("expected a URI")
        };
        assert_eq!(u, "https://x.com/cat.png");
        // Single quotes and no quotes both parse.
        let Some(Payload::Uri(u)) = parse_payload("text/html", b"<img src='/a.png'>") else {
            panic!()
        };
        assert_eq!(u, "/a.png");
        // Markup with no image at all is not a drop we can use.
        assert!(parse_payload("text/html", b"<p>hello</p>").is_none());
    }

    #[test]
    fn data_uris_carry_their_own_bytes() {
        // "PNG" magic, base64'd, as an inline thumbnail arrives.
        let png = [0x89u8, b'P', b'N', b'G', 0x0d];
        let b64 = {
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut o = String::new();
            for c in png.chunks(3) {
                let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                for i in 0..4 {
                    if i <= c.len() {
                        o.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                    } else {
                        o.push('=');
                    }
                }
            }
            o
        };
        let uri = format!("data:image/png;base64,{b64}");
        let (bytes, name) = fetch(Payload::Uri(uri)).expect("data: URI decodes");
        assert_eq!(&bytes[..5], &png[..]);
        assert_eq!(name, "dropped-image.png");
    }

    #[test]
    fn base64_roundtrips_known_vectors() {
        assert_eq!(decode_base64("TWFu").unwrap(), b"Man".to_vec());
        assert_eq!(decode_base64("TWE=").unwrap(), b"Ma".to_vec());
        assert_eq!(decode_base64("TW E =\n").unwrap(), b"Ma".to_vec());
    }

    #[test]
    fn extension_is_sniffed_from_content() {
        assert_eq!(extension_for(&[0x89, b'P', b'N', b'G', 0]), "png");
        assert_eq!(extension_for(&[0xFF, 0xD8, 0xFF, 0]), "jpg");
        assert_eq!(extension_for(b"not an image"), "bin");
    }
}
