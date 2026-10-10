// The desktop board: everything pinned to the canvas, kept as a JSON Canvas
// file (https://jsoncanvas.org, Obsidian's `.canvas`) — `Desktop.canvas` at
// the root of the notes vault when one is configured, so Obsidian and the
// user's other devices see the desktop as a canvas, else
// `$XDG_DATA_HOME/cce/desktop.canvas`.
//
// Nodes map to desktop items one to one (JSON Canvas node order is z-order,
// as the items list's is): `file` nodes are images, note cards or plain
// file cards by extension; `text`, `link` and `group` nodes are their own
// kinds; any type this client does not know is kept, undrawn, so a newer
// Obsidian's nodes survive a save. Edges are kept with their ids, sides,
// labels and colours.
//
// A save edits the canvas it read (cce_vault::canvas keeps every field as
// the raw text it came as) rather than writing a fresh one: fields and
// keys this client does not model — a node's colour Obsidian set, a key a
// newer version adds — go back exactly as they were.
//
// File paths: vault-relative for a file inside the vault (what Obsidian
// reads), absolute for one outside it — the desktop folder's images, which
// Obsidian then shows as missing while cce draws them.
//
// The first run with a board migrates the old sidecar
// (`desktop-items.json`, images only) into it and renames the sidecar to
// `desktop-items.json.migrated`.

use std::path::{Path, PathBuf};

use cce_vault::canvas::{self, Canvas, Object};

use crate::items::{kind_for_file, DesktopItem, Kind};

#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub id: String,
    pub from: String,
    pub to: String,
    pub from_side: Option<String>,
    pub to_side: Option<String>,
    pub label: Option<String>,
    /// An arrowhead at the `to` end (JSON Canvas's default `toEnd`).
    pub arrow: bool,
}

pub struct Board {
    pub path: PathBuf,
    /// The vault root, when the board lives in one.
    pub vault: Option<PathBuf>,
    canvas: Canvas,
    /// The board file's mtime after this client's last write or read, so
    /// a change it did not make itself can be told apart.
    pub seen: Option<std::time::SystemTime>,
    /// The file is there but did not parse (a sync caught mid-write, a bad
    /// hand edit): never written over, and read again when it changes —
    /// until then saves do nothing. It used to be dropped for the session:
    /// an empty desktop until the next login, and every pin since lost.
    pub unreadable: bool,
}

/// What a save found and did when the file had changed since it was read:
/// the board as merged (this client's edits over the outside ones), for the
/// caller to show.
pub struct Merged {
    pub items: Vec<DesktopItem>,
    pub edges: Vec<Edge>,
}

/// Where the board lives: the vault's `Desktop.canvas`, else the data dir.
pub fn location() -> (PathBuf, Option<PathBuf>) {
    match cce_vault::config::vault_root(None) {
        Ok(vault) => (vault.join("Desktop.canvas"), Some(vault)),
        Err(_) => (cce_ui::config::data_home().join("cce").join("desktop.canvas"), None),
    }
}

/// The pre-board sidecar: images only.
pub fn legacy_path() -> PathBuf {
    cce_ui::config::data_home().join("cce").join("desktop-items.json")
}

#[derive(serde::Deserialize)]
struct LegacyItem {
    path: PathBuf,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// A fresh node id in Obsidian's shape: 16 hex digits.
pub fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u64(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    format!("{:016x}", h.finish())
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Board {
    /// The board at its configured place, migrating the old sidecar into
    /// it the first time.
    pub fn open() -> (Board, Vec<DesktopItem>, Vec<Edge>) {
        let (path, vault) = location();
        Board::open_at(path, vault, &legacy_path())
    }

    pub fn open_at(path: PathBuf, vault: Option<PathBuf>, legacy: &Path) -> (Board, Vec<DesktopItem>, Vec<Edge>) {
        let mut board = Board { path, vault, canvas: Canvas::empty(), seen: None, unreadable: false };
        if board.path.exists() {
            match board.read() {
                Ok((items, edges)) => return (board, items, edges),
                Err(e) => {
                    // A board that does not parse must not be overwritten by
                    // an empty one: run with nothing, leave the file be, and
                    // try it again when it changes.
                    log::error!("[board] {} is unreadable ({e}); not loading or rewriting it", board.path.display());
                    board.unreadable = true;
                    board.seen = mtime(&board.path);
                    return (board, Vec::new(), Vec::new());
                }
            }
        }
        let items = migrate(legacy);
        if !items.is_empty() {
            board.save(&items, &[]);
            if board.seen.is_some() {
                let done = legacy.with_extension("json.migrated");
                if let Err(e) = std::fs::rename(legacy, &done) {
                    log::warn!("[board] could not set the old sidecar aside: {e}");
                }
                log::info!("[board] migrated {} item(s) into {}", items.len(), board.path.display());
            }
        }
        (board, items, Vec::new())
    }

    /// Re-read the board file (it changed under us: Obsidian, a sync).
    pub fn read(&mut self) -> Result<(Vec<DesktopItem>, Vec<Edge>), String> {
        let (canvas, seen) = self.read_canvas()?;
        let (items, edges) = to_model(&canvas, self.vault.as_deref());
        self.canvas = canvas;
        self.seen = seen;
        self.unreadable = false;
        Ok((items, edges))
    }

    fn read_canvas(&self) -> Result<(Canvas, Option<std::time::SystemTime>), String> {
        let seen = mtime(&self.path);
        let text = std::fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        Ok((canvas::from_str(&text).map_err(|e| e.to_string())?, seen))
    }

    /// True when the file changed since this client last read or wrote it.
    pub fn changed_on_disk(&self) -> bool {
        !self.path.as_os_str().is_empty() && mtime(&self.path) != self.seen
    }

    /// Write `items` and `edges`. When the file changed since this client
    /// last read or wrote it (an Obsidian edit or a sync the watcher has not
    /// delivered yet), the outside edit is read and merged first rather
    /// than written over — see [`merge`] — and the merge is returned for
    /// the caller to show.
    pub fn save(&mut self, items: &[DesktopItem], edges: &[Edge]) -> Option<Merged> {
        if self.unreadable {
            log::warn!("[board] {} is unreadable; not saving over it", self.path.display());
            return None;
        }
        let mut merged = None;
        if self.changed_on_disk() {
            match self.read_canvas() {
                Ok((theirs, seen)) => {
                    let vault = self.vault.as_deref();
                    let (base_items, base_edges) = to_model(&self.canvas, vault);
                    let (their_items, their_edges) = to_model(&theirs, vault);
                    let m = merge(&base_items, &base_edges, items, edges, &their_items, &their_edges);
                    log::info!("[board] {} changed outside; merged before saving", self.path.display());
                    self.canvas = theirs;
                    self.seen = seen;
                    merged = Some(m);
                }
                // Gone or half-written: write ours, as before.
                Err(e) => log::warn!("[board] {} changed but does not read ({e}); saving ours", self.path.display()),
            }
        }
        let (items, edges) = match &merged {
            Some(m) => (m.items.as_slice(), m.edges.as_slice()),
            None => (items, edges),
        };
        apply_model(&mut self.canvas, self.vault.as_deref(), items, edges);
        let text = canvas::to_string(&self.canvas);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write-then-rename beside it: a crash mid-write would leave a
        // truncated board, which then refuses to load.
        // Hidden, so vault watchers (which skip dot-files) never see it.
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = self.path.with_file_name(format!(".{name}.cce-tmp"));
        match std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &self.path)) {
            Ok(()) => self.seen = mtime(&self.path),
            Err(e) => log::error!("[board] could not save {}: {e}", self.path.display()),
        }
        merged
    }
}

/// Whether two nodes say the same thing, as a save would write them
/// (positions are whole pixels there).
fn same_node(a: &DesktopItem, b: &DesktopItem) -> bool {
    let r = |v: f64| v.round();
    a.kind == b.kind
        && r(a.x) == r(b.x)
        && r(a.y) == r(b.y)
        && r(a.w).max(1.0) == r(b.w).max(1.0)
        && r(a.h).max(1.0) == r(b.h).max(1.0)
        && a.color == b.color
}

/// A three-way merge by id: `base` as last read, `ours` as this client has
/// it now, `theirs` as the file is now. A node or edge this client changed,
/// added or removed is taken from `ours`; one it left alone follows
/// `theirs` (moved, changed, removed or added outside). Where both changed
/// one node, ours wins. Order: ours, then what was added outside.
pub fn merge(
    base: &[DesktopItem],
    base_edges: &[Edge],
    ours: &[DesktopItem],
    our_edges: &[Edge],
    theirs: &[DesktopItem],
    their_edges: &[Edge],
) -> Merged {
    let find = |list: &[DesktopItem], id: &str| list.iter().find(|i| i.node == id).cloned();
    let mut items = Vec::new();
    for o in ours {
        match (find(base, &o.node), find(theirs, &o.node)) {
            // Untouched here: theirs, or gone if they removed it.
            (Some(b), t) if same_node(&b, o) => {
                if let Some(mut t) = t {
                    t.id = o.id;
                    items.push(t);
                }
            }
            // Changed or added here.
            _ => items.push(o.clone()),
        }
    }
    for t in theirs {
        let added_outside = find(base, &t.node).is_none() && find(ours, &t.node).is_none();
        if added_outside {
            items.push(t.clone());
        }
    }
    let find_edge = |list: &[Edge], id: &str| list.iter().find(|e| e.id == id).cloned();
    let mut edges = Vec::new();
    for o in our_edges {
        match (find_edge(base_edges, &o.id), find_edge(their_edges, &o.id)) {
            (Some(b), t) if b == *o => edges.extend(t),
            _ => edges.push(o.clone()),
        }
    }
    for t in their_edges {
        if find_edge(base_edges, &t.id).is_none() && find_edge(our_edges, &t.id).is_none() {
            edges.push(t.clone());
        }
    }
    // An edge whose end is gone goes with it.
    edges.retain(|e| items.iter().any(|i| i.node == e.from) && items.iter().any(|i| i.node == e.to));
    Merged { items, edges }
}

/// After an unreadable board reads at last: the board as read, plus what was
/// pinned meanwhile (`local`, nodes the file does not have).
pub fn keep_local(fresh: Vec<DesktopItem>, local: Vec<DesktopItem>) -> Vec<DesktopItem> {
    let mut out = fresh;
    for l in local {
        if !out.iter().any(|f| f.node == l.node) {
            out.push(l);
        }
    }
    out
}

fn migrate(legacy: &Path) -> Vec<DesktopItem> {
    let Ok(text) = std::fs::read_to_string(legacy) else { return Vec::new() };
    let Ok(old) = serde_json::from_str::<Vec<LegacyItem>>(&text) else {
        log::warn!("[board] {} does not parse; not migrating it", legacy.display());
        return Vec::new();
    };
    old.into_iter()
        .map(|o| DesktopItem::new(new_id(), kind_for_file(o.path), o.x, o.y, o.w, o.h))
        .collect()
}

/// A `file` value as stored, resolved to a path on disk.
fn resolve(file: &str, vault: Option<&Path>) -> PathBuf {
    let p = Path::new(file);
    match vault {
        Some(v) if !p.is_absolute() => v.join(p),
        _ => p.to_path_buf(),
    }
}

/// A path as a `file` value: vault-relative inside the vault, else absolute.
fn file_value(path: &Path, vault: Option<&Path>) -> String {
    match vault.and_then(|v| path.strip_prefix(v).ok()) {
        Some(rel) => rel.to_string_lossy().into_owned(),
        None => path.to_string_lossy().into_owned(),
    }
}

pub fn to_model(canvas: &Canvas, vault: Option<&Path>) -> (Vec<DesktopItem>, Vec<Edge>) {
    let mut items = Vec::new();
    for n in canvas.nodes() {
        let Some(id) = n.str("id") else { continue };
        let kind = match n.str("type").as_deref() {
            Some("file") => kind_for_file(resolve(&n.str("file").unwrap_or_default(), vault)),
            Some("text") => Kind::Text(n.str("text").unwrap_or_default()),
            Some("link") => Kind::Link(n.str("url").unwrap_or_default()),
            Some("group") => Kind::Group(n.str("label").unwrap_or_default()),
            other => Kind::Other(other.unwrap_or("").to_string()),
        };
        let num = |k: &str| n.num(k).unwrap_or(0.0);
        let mut item = DesktopItem::new(id, kind, num("x"), num("y"), num("width").max(1.0), num("height").max(1.0));
        item.color = n.str("color");
        items.push(item);
    }
    let edges = canvas
        .edges()
        .filter_map(|e| {
            Some(Edge {
                id: e.str("id")?,
                from: e.str("fromNode")?,
                to: e.str("toNode")?,
                from_side: e.str("fromSide"),
                to_side: e.str("toSide"),
                label: e.str("label"),
                arrow: e.str("toEnd").as_deref() != Some("none"),
            })
        })
        .collect();
    (items, edges)
}

/// Set a string field only when its value differs, so an untouched field
/// keeps the exact text (escapes included) it was read as.
fn put(o: &mut Object, key: &str, value: &str) {
    if o.str(key).as_deref() != Some(value) {
        o.set_str(key, value);
    }
}

/// Write the model into the canvas it was read from: nodes in the items'
/// order (z-order), each the object it was read as with the modelled
/// fields updated, new ones built fresh; edges likewise.
pub fn apply_model(canvas: &mut Canvas, vault: Option<&Path>, items: &[DesktopItem], edges: &[Edge]) {
    let mut old: Vec<Object> = std::mem::take(canvas.objects_mut("nodes"));
    let mut nodes = Vec::with_capacity(items.len());
    for item in items {
        let mut n = match old.iter().position(|o| o.str("id").as_deref() == Some(item.node.as_str())) {
            Some(i) => old.swap_remove(i),
            None => {
                let mut n = Object::new();
                n.set_str("id", &item.node);
                n
            }
        };
        // A node that changed type (a text card converted to a note) drops
        // the fields of the type it was; one that kept its type keeps every
        // field, known or not.
        let new_type = match &item.kind {
            Kind::Image(_) | Kind::Note(_) | Kind::File(_) => Some("file"),
            Kind::Text(_) => Some("text"),
            Kind::Link(_) => Some("link"),
            Kind::Group(_) => Some("group"),
            Kind::Other(_) => None,
        };
        if let Some(t) = new_type {
            if n.str("type").is_some_and(|old| old != t) {
                for key in ["file", "subpath", "text", "url", "label", "background", "backgroundStyle"] {
                    n.remove(key);
                }
            }
        }
        match &item.kind {
            Kind::Image(p) | Kind::Note(p) | Kind::File(p) => {
                put(&mut n, "type", "file");
                put(&mut n, "file", &file_value(p, vault));
            }
            Kind::Text(t) => {
                put(&mut n, "type", "text");
                put(&mut n, "text", t);
            }
            Kind::Link(u) => {
                put(&mut n, "type", "link");
                put(&mut n, "url", u);
            }
            Kind::Group(l) => {
                put(&mut n, "type", "group");
                put(&mut n, "label", l);
            }
            // Not modelled: written back exactly as read.
            Kind::Other(_) => {}
        }
        // JSON Canvas positions are whole pixels.
        n.set_num("x", item.x.round());
        n.set_num("y", item.y.round());
        n.set_num("width", item.w.round().max(1.0));
        n.set_num("height", item.h.round().max(1.0));
        match &item.color {
            Some(c) => put(&mut n, "color", c),
            None => n.remove("color"),
        }
        nodes.push(n);
    }
    *canvas.objects_mut("nodes") = nodes;

    let mut old: Vec<Object> = std::mem::take(canvas.objects_mut("edges"));
    let mut out = Vec::with_capacity(edges.len());
    for e in edges {
        let mut o = match old.iter().position(|o| o.str("id").as_deref() == Some(e.id.as_str())) {
            Some(i) => old.swap_remove(i),
            None => {
                let mut o = Object::new();
                o.set_str("id", &e.id);
                o
            }
        };
        put(&mut o, "fromNode", &e.from);
        put(&mut o, "toNode", &e.to);
        for (key, v) in [("fromSide", &e.from_side), ("toSide", &e.to_side), ("label", &e.label)] {
            match v {
                Some(s) => put(&mut o, key, s),
                None => o.remove(key),
            }
        }
        match (e.arrow, o.str("toEnd").as_deref()) {
            (true, Some("none")) => o.remove("toEnd"),
            (false, _) => put(&mut o, "toEnd", "none"),
            _ => {}
        }
        out.push(o);
    }
    *canvas.objects_mut("edges") = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBSIDIAN: &str = "{\n\t\"nodes\":[\n\
        \t\t{\"id\":\"g1\",\"type\":\"group\",\"label\":\"Plans\",\"x\":-100,\"y\":-100,\"width\":900,\"height\":600},\n\
        \t\t{\"id\":\"n1\",\"type\":\"file\",\"file\":\"notes/Idea.md\",\"x\":0,\"y\":0,\"width\":400,\"height\":300,\"color\":\"4\"},\n\
        \t\t{\"id\":\"t1\",\"type\":\"text\",\"text\":\"**hi**\",\"x\":450,\"y\":0,\"width\":250,\"height\":60},\n\
        \t\t{\"id\":\"i1\",\"type\":\"file\",\"file\":\"/home/u/Desktop/cat.png\",\"x\":0,\"y\":400,\"width\":200,\"height\":100},\n\
        \t\t{\"id\":\"f1\",\"type\":\"future\",\"x\":1,\"y\":2,\"width\":3,\"height\":4,\"what\":[1,2]}\n\
        \t],\n\t\"edges\":[\n\
        \t\t{\"id\":\"e1\",\"fromNode\":\"n1\",\"fromSide\":\"right\",\"toNode\":\"t1\",\"toSide\":\"left\",\"label\":\"so\"}\n\
        \t]\n}";

    fn vault() -> PathBuf {
        PathBuf::from("/v")
    }

    #[test]
    fn reads_every_kind_and_edge() {
        let canvas = canvas::from_str(OBSIDIAN).unwrap();
        let (items, edges) = to_model(&canvas, Some(&vault()));
        let kinds: Vec<&Kind> = items.iter().map(|i| &i.kind).collect();
        assert_eq!(
            kinds,
            [
                &Kind::Group("Plans".into()),
                &Kind::Note("/v/notes/Idea.md".into()),
                &Kind::Text("**hi**".into()),
                &Kind::Image("/home/u/Desktop/cat.png".into()),
                &Kind::Other("future".into()),
            ]
        );
        assert_eq!(items[1].color.as_deref(), Some("4"));
        assert_eq!(edges[0].label.as_deref(), Some("so"));
        assert!(edges[0].arrow);
    }

    #[test]
    fn an_untouched_board_saves_byte_for_byte() {
        let mut canvas = canvas::from_str(OBSIDIAN).unwrap();
        let (items, edges) = to_model(&canvas, Some(&vault()));
        apply_model(&mut canvas, Some(&vault()), &items, &edges);
        assert_eq!(canvas::to_string(&canvas), OBSIDIAN);
    }

    #[test]
    fn edits_reorder_remove_and_add() {
        let mut canvas = canvas::from_str(OBSIDIAN).unwrap();
        let (mut items, mut edges) = to_model(&canvas, Some(&vault()));
        // Raise the note (z-order), move it, drop the text card and its edge,
        // add a new card for a note outside the vault.
        let note = items.remove(1);
        items.push(note);
        items.last_mut().unwrap().x = 12.4;
        items.retain(|i| i.node != "t1");
        edges.clear();
        items.push(DesktopItem::new("z9".into(), Kind::Note("/elsewhere/X.md".into()), 5.0, 6.0, 400.0, 300.0));
        apply_model(&mut canvas, Some(&vault()), &items, &edges);
        let out = canvas::to_string(&canvas);
        let ids: Vec<String> = canvas.nodes().filter_map(|n| n.str("id")).collect();
        assert_eq!(ids, ["g1", "i1", "f1", "n1", "z9"]);
        assert!(out.contains(r#"{"id":"n1","type":"file","file":"notes/Idea.md","x":12,"y":0,"width":400,"height":300,"color":"4"}"#), "{out}");
        assert!(out.contains(r#""file":"/elsewhere/X.md""#));
        assert!(out.contains(r#""what":[1,2]"#), "unknown node kept");
        assert_eq!(canvas.edges().count(), 0);
    }

    #[test]
    fn a_changed_type_drops_the_old_fields() {
        let mut canvas = canvas::from_str(OBSIDIAN).unwrap();
        let (mut items, edges) = to_model(&canvas, Some(&vault()));
        items[2].kind = Kind::Note("/v/hi.md".into());
        apply_model(&mut canvas, Some(&vault()), &items, &edges);
        let out = canvas::to_string(&canvas);
        assert!(out.contains(r#"{"id":"t1","type":"file","x":450,"y":0,"width":250,"height":60,"file":"hi.md"}"#), "{out}");
    }

    #[test]
    fn migrates_the_old_sidecar_once() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("desktop-items.json");
        std::fs::write(&legacy, r#"[{"path":"/home/u/Desktop/a.jpg","x":-7547.2,"y":-4732.8,"w":512.0,"h":288.0}]"#).unwrap();
        let board_path = dir.path().join("vault/Desktop.canvas");
        let (board, items, _) = Board::open_at(board_path.clone(), Some(dir.path().join("vault")), &legacy);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, Kind::Image("/home/u/Desktop/a.jpg".into()));
        assert!(board_path.exists());
        assert!(!legacy.exists());
        assert!(dir.path().join("desktop-items.json.migrated").exists());
        let text = std::fs::read_to_string(&board_path).unwrap();
        assert!(text.contains(r#""file":"/home/u/Desktop/a.jpg","x":-7547,"y":-4733,"width":512,"height":288"#), "{text}");
        // Reopening reads the board, not the (gone) sidecar.
        let (_, again, _) = Board::open_at(board_path, board.vault.clone(), &legacy);
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn a_broken_board_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Desktop.canvas");
        std::fs::write(&path, "{ not json").unwrap();
        let (mut board, items, _) = Board::open_at(path.clone(), None, &dir.path().join("none.json"));
        assert!(items.is_empty());
        board.save(&[], &[]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    /// Write `text` as the board, making sure its mtime moves.
    fn rewrite(path: &Path, text: &str) {
        let before = mtime(path);
        loop {
            std::fs::write(path, text).unwrap();
            if mtime(path) != before {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_board_unreadable_at_start_is_read_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Desktop.canvas");
        // A sync caught mid-write.
        std::fs::write(&path, &OBSIDIAN[..40]).unwrap();
        let (mut board, items, _) = Board::open_at(path.clone(), None, &dir.path().join("none.json"));
        assert!(items.is_empty() && board.unreadable);
        assert_eq!(board.path, path, "the board must stay watched");
        // Something pinned meanwhile is not written over the file...
        let local = DesktopItem::new("p1".into(), Kind::Text("pinned meanwhile".into()), 0.0, 0.0, 10.0, 10.0);
        board.save(std::slice::from_ref(&local), &[]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), &OBSIDIAN[..40]);
        // ...and when the sync finishes, the board reads and keeps it.
        rewrite(&path, OBSIDIAN);
        assert!(board.changed_on_disk());
        let (fresh, edges) = board.read().unwrap();
        assert!(!board.unreadable);
        let all = keep_local(fresh, vec![local]);
        assert_eq!(all.len(), 6);
        assert_eq!(all.last().unwrap().node, "p1");
        board.save(&all, &edges);
        assert!(std::fs::read_to_string(&path).unwrap().contains("pinned meanwhile"));
    }

    #[test]
    fn a_save_merges_an_outside_edit_it_had_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Desktop.canvas");
        std::fs::write(&path, OBSIDIAN).unwrap();
        let (mut board, mut items, edges) = Board::open_at(path.clone(), Some(vault()), &dir.path().join("none.json"));
        // Elsewhere (a phone, then a sync): the text card moves, the image
        // goes, a node is added.
        let theirs = OBSIDIAN
            .replace(r#""id":"t1","type":"text","text":"**hi**","x":450"#, r#""id":"t1","type":"text","text":"**hi**","x":999"#)
            .replace("\t\t{\"id\":\"i1\",\"type\":\"file\",\"file\":\"/home/u/Desktop/cat.png\",\"x\":0,\"y\":400,\"width\":200,\"height\":100},\n", "")
            .replace(r#"{"id":"f1""#, r#"{"id":"new","type":"text","text":"from the phone","x":5,"y":5,"width":50,"height":50},
		{"id":"f1""#);
        rewrite(&path, &theirs);
        // Here, before the watcher said so: the note card is dragged.
        items[1].x = 123.0;
        let merged = board.save(&items, &edges).expect("the outside edit was merged");
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains(r#""id":"n1","type":"file","file":"notes/Idea.md","x":123"#), "ours: {out}");
        assert!(out.contains(r#""id":"t1","type":"text","text":"**hi**","x":999"#), "theirs kept: {out}");
        assert!(out.contains("from the phone"), "their new node kept: {out}");
        assert!(!out.contains("cat.png"), "their removal kept: {out}");
        let ids: Vec<&str> = merged.items.iter().map(|i| i.node.as_str()).collect();
        assert_eq!(ids, ["g1", "n1", "t1", "f1", "new"]);
        assert_eq!(merged.edges.len(), 1);
        // Nothing new outside: a plain save, no merge.
        assert!(board.save(&merged.items, &merged.edges).is_none());
    }

    #[test]
    fn merge_rules() {
        let n = |id: &str, x: f64| DesktopItem::new(id.into(), Kind::Text(id.into()), x, 0.0, 10.0, 10.0);
        let base = vec![n("a", 0.0), n("b", 0.0), n("c", 0.0)];
        // Ours: moved a, removed b. Theirs: moved a and c, added d.
        let ours = vec![n("a", 5.0), n("c", 0.0)];
        let theirs = vec![n("a", 9.0), n("b", 0.0), n("c", 7.0), n("d", 0.0)];
        let m = merge(&base, &[], &ours, &[], &theirs, &[]);
        let got: Vec<(String, f64)> = m.items.iter().map(|i| (i.node.clone(), i.x)).collect();
        assert_eq!(got, [("a".into(), 5.0), ("c".into(), 7.0), ("d".into(), 0.0)], "ours wins a, our removal of b stands, their c and d kept");
    }
}
