//! cce-grid — the desktop grid, drawn by cce-ui.
//!
//! The compositor world-anchors this surface to the virtual desktop and
//! pans/zooms it per frame exactly like window content, so this app is never
//! in the camera loop. It renders only when the compositor hands it a patch
//! (`Application::grid_patch`): a virtual-desktop rectangle plus a
//! px-per-virtual-unit scale. Everything here is therefore a pure function
//! of (patch, style config) — no camera state, no timers.
//!
//! The look comes from the same config keys the compositor's fallback grid
//! reads (`style.surface.desktop.*`, root plate corner radius): flat
//! rounded cells, with the relief on the LINES — the gap rails read as
//! raised grout (per-cell half-gap-expanded `Recess` rings that abut at
//! the rail centerlines), while every cell floor stays flat.


use cce_ui::engine::{Application, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{Cap, DisplayList, PaintCtx};
use cce_ui::widget::markdown::{self, Layout as CardLayout, ShapingMeasure, Theme};
use cce_ui::widget::{ElementState, KeyEvent, MouseButton, MouseScrollDelta};

mod board;
mod items;

use board::Edge;
use items::Kind;

/// A note or text card's content box inset, and the label above a card, in
/// virtual units (logical px at zoom 1).
const CARD_PAD: f64 = 14.0;
const CARD_RADIUS: f64 = 10.0;
const CARD_TEXT: f32 = 15.0;
const CARD_LABEL: f64 = 13.0;
/// A new note or text card's size.
const CARD_W: f64 = 400.0;
const CARD_H: f64 = 300.0;
/// Smallest a card may be resized to.
const MIN_CARD: f64 = 80.0;
const EDGE_WIDTH: f64 = 2.0;
const ARROW_LEN: f64 = 14.0;
/// Two presses on one item within this read as a double-click.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// JSON Canvas's six preset colours (Obsidian's), else a `#rrggbb` value.
fn canvas_color(c: Option<&str>) -> Option<[f32; 4]> {
    let srgb = match c? {
        "1" => [0.91, 0.30, 0.33, 1.0],
        "2" => [0.93, 0.56, 0.24, 1.0],
        "3" => [0.92, 0.79, 0.27, 1.0],
        "4" => [0.27, 0.75, 0.42, 1.0],
        "5" => [0.31, 0.72, 0.82, 1.0],
        "6" => [0.62, 0.48, 0.91, 1.0],
        hex => cce_ui::color::parse_hex_rgba(hex)?,
    };
    Some(cce_ui::colors::to_linear(srgb))
}

/// A laid-out card, and what it was laid out from.
struct CardCache {
    source: String,
    width: f64,
    layout: CardLayout,
}

#[derive(Debug, Clone)]
enum Message {
    /// A dropped image finished fetching, saving and decoding on its worker
    /// thread. Carried as pixels rather than an image id because the GPU
    /// upload has to happen on the main loop.
    ItemReady {
        item: items::DesktopItem,
        pixels: Vec<u8>,
        px_w: u32,
        px_h: u32,
    },
    /// An item's context menu closed on an action. Carries the canvas node
    /// id rather than an index: the menu is modal on its own thread, and the
    /// list can be reordered by a drag (or grown by a drop) while it is open.
    MenuAction { node: String, action: String },
    /// A drop that became a card without pixels to upload (a note, text, a
    /// link): ready to pin.
    AddItem(items::DesktopItem),
    /// Files changed in the vault: the board itself (Obsidian, a sync) or a
    /// note a card shows.
    VaultChanged(Vec<std::path::PathBuf>),
    /// The compositor's window-adjust mode (overview, or Super held) came
    /// on or went off — the `adjust` status topic. While it is on every
    /// pinned image shows its four corner handles.
    AdjustMode(bool),
    /// The compositor's overview drag-selection is carrying some of the
    /// pinned images (`selection` status topic, `move <id>:<x>:<y> ...`):
    /// where each is now, in virtual units. The compositor moves the rects
    /// it was reported and draws the selection; this side just follows.
    SelectionMove(Vec<(u64, f64, f64)>),
    /// The group move released (`drop`): the positions stand, so save the
    /// board and report the list afresh.
    SelectionDrop,
    /// A pinned image (restored, or arrived by an outside edit) finished
    /// decoding on a worker thread (`upload_missing`); `None` when it did
    /// not decode. Matched back by node and kind: the list may have changed
    /// meanwhile.
    ImageDecoded { node: String, kind: Kind, decoded: Option<items::Decoded> },
}

/// Which corner handle of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    /// Which way the corner faces: +1 on the right/bottom edge, -1 on the
    /// left/top. Dragging the corner by (dx, dy) grows the item by
    /// (sx*dx, sy*dy).
    fn sign(self) -> (f64, f64) {
        match self {
            Corner::TopLeft => (-1.0, -1.0),
            Corner::TopRight => (1.0, -1.0),
            Corner::BottomLeft => (-1.0, 1.0),
            Corner::BottomRight => (1.0, 1.0),
        }
    }
}

/// An item's corner handles in VIRTUAL units: the disc radius, and each
/// corner's centre. A disc is tangent to both of its edges — the same
/// placement the compositor gives a window's corner handles when the
/// silhouette has no corner radius — and never wider than a quarter of the
/// image, so a thumbnail is not all handle.
fn handle_discs(item: &items::DesktopItem, diameter: f64) -> (f64, [(Corner, f64, f64); 4]) {
    let r = (diameter / 2.0).min(item.w / 4.0).min(item.h / 4.0).max(1.0);
    let (x0, y0, x1, y1) = (item.x + r, item.y + r, item.x + item.w - r, item.y + item.h - r);
    (
        r,
        [
            (Corner::TopLeft, x0, y0),
            (Corner::TopRight, x1, y0),
            (Corner::BottomLeft, x0, y1),
            (Corner::BottomRight, x1, y1),
        ],
    )
}

/// Smallest an image may be resized to, in virtual units.
const MIN_ITEM_SIZE: f64 = 16.0;

/// Subscribe to one of the compositor's status topics and forward every
/// push through `parse` as a message, reconnecting with backoff until the
/// socket is there (the compositor may come up after this service, and
/// restarts at login). `adjust` is a state topic: a new subscriber is sent
/// the current state at once, so the very first line settles whether the
/// handles should already be up. `selection` is one-shot: its lines come
/// only while a group move is carrying this client's images.
fn spawn_topic_listener(
    topic: &'static str,
    sender: calloop::channel::Sender<Message>,
    parse: fn(&str) -> Option<Message>,
) {
    use std::io::{BufRead, Write};
    std::thread::spawn(move || {
        let mut retry_s = 1u64;
        loop {
            let path = cce_ui::ipc::socket_path("cce-status-interface");
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&path) {
                if stream.write_all(format!("{topic}\n").as_bytes()).is_ok() {
                    let mut reader = std::io::BufReader::new(stream);
                    let mut line = String::new();
                    while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                        retry_s = 1;
                        if let Some(msg) = parse(line.trim()) {
                            if sender.send(msg).is_err() {
                                return;
                            }
                        }
                        line.clear();
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(retry_s));
            retry_s = (retry_s * 2).min(30);
        }
    });
}

/// A `selection` topic line: `move <id>:<x>:<y> ...` or `drop`. Anything
/// else — or a `move` with nothing parseable on it — is ignored.
fn parse_selection_line(line: &str) -> Option<Message> {
    let mut words = line.split_whitespace();
    match words.next()? {
        "drop" => Some(Message::SelectionDrop),
        "move" => {
            let moves: Vec<(u64, f64, f64)> = words
                .filter_map(|tok| {
                    let mut f = tok.split(':');
                    let id = f.next()?.parse::<u64>().ok()?;
                    let x = f.next()?.parse::<f64>().ok()?;
                    let y = f.next()?.parse::<f64>().ok()?;
                    Some((id, x, y))
                })
                .collect();
            if moves.is_empty() {
                None
            } else {
                Some(Message::SelectionMove(moves))
            }
        }
        _ => None,
    }
}

/// The thread that tells the compositor what is pinned (`grid-items`, the
/// whole list on every change — see `GridApp::report_items`). Its own
/// thread because the control socket is request/reply and the reply waits
/// on the compositor's main loop, which must never stall the paint loop
/// here; a channel because reports must land in order. A report that
/// cannot be delivered (the compositor not up yet) is retried with backoff,
/// and only the LATEST pending report is ever sent — a stale list is worse
/// than a late one.
fn spawn_reporter() -> std::sync::mpsc::Sender<String> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut retry_s = 1u64;
        let mut pending: Option<String> = None;
        loop {
            // Block for the next report unless one is waiting to be
            // retried, in which case just drain whatever is newer.
            if pending.is_none() {
                match rx.recv() {
                    Ok(line) => pending = Some(line),
                    Err(_) => return,
                }
            }
            while let Ok(newer) = rx.try_recv() {
                pending = Some(newer);
            }
            let line = pending.take().unwrap_or_default();
            match cce_ui::ipc::send_command("cce", &line) {
                Ok(_) => retry_s = 1,
                Err(e) => {
                    log::debug!("[items] report not delivered ({e}); retrying in {retry_s}s");
                    pending = Some(line);
                    std::thread::sleep(std::time::Duration::from_secs(retry_s));
                    retry_s = (retry_s * 2).min(30);
                }
            }
        }
    });
    tx
}

/// The world region the current buffer must cover, as told by the
/// compositor: virtual origin/size and surface px per virtual unit.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Patch {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    scale: f64,
}

struct GridApp {
    patch: Option<Patch>,
    /// Everything pinned to the canvas, in z-order, each image paired with
    /// its uploaded texture id (`None` until the renderer exists — see
    /// `renderer_init` — and always for cards).
    items: Vec<(items::DesktopItem, Option<u32>)>,
    /// The links between items (JSON Canvas edges).
    edges: Vec<Edge>,
    /// Where items and edges are kept: `Desktop.canvas` in the vault.
    board: board::Board,
    _watcher: Option<cce_vault::VaultWatcher>,
    /// Laid-out note and text cards, by node id.
    cards: std::collections::HashMap<String, CardCache>,
    /// Images being decoded for a texture, by node id (`upload_missing`).
    decoding: std::collections::HashSet<String>,
    measure: Option<ShapingMeasure>,
    /// "Connect to…" was picked on this node: the next press on another
    /// item draws an edge to it.
    connecting: Option<String>,
    /// The last press, for double-click: (node, when).
    last_press: Option<(String, std::time::Instant)>,
    /// Worker threads post finished drops back through this.
    sender: calloop::channel::Sender<Message>,
    /// The `grid-items` reports go out through this (`spawn_reporter`).
    reporter: std::sync::mpsc::Sender<String>,
    /// The next `DesktopItem::id` to hand out.
    next_id: u64,
    /// The item being dragged, and where inside it the pointer grabbed —
    /// held in VIRTUAL units so the drag survives a pan or zoom mid-gesture.
    dragging: Option<Drag>,
    /// The compositor's window-adjust mode (`adjust` status topic): while
    /// on, the item under the pointer shows its corner handles and a press
    /// on one resizes.
    adjust: bool,
    /// The item under the pointer (body or handle) — the one that shows its
    /// handles, like the compositor's ring on the hovered window. Cleared by
    /// the off-screen move cce-ui synthesizes on pointer leave, so it drops
    /// the moment the pointer is on the background or a window.
    hover_item: Option<usize>,
    /// The corner handle under the pointer, drawn in the hover colour.
    hover: Option<(usize, Corner)>,
    /// An in-flight corner resize, delta-driven like `Drag`.
    resizing: Option<Resize>,
    /// What changed since the last painted frame — see [`Damage`].
    damage: Damage,
    /// Everything the last painted frame was a function of besides the
    /// items: a frame whose inputs differ is repainted in full, whatever
    /// `damage` says.
    painted: Option<(Patch, (f64, f64), String)>,
    /// The raw `(relief)` string currently installed process-wide (depth +
    /// wall profile LUT) — a change detector, so the registry is only
    /// touched when the config value actually changes.
    applied_relief: Option<String>,
    /// The DE-wide `bevel_depth` captured before the first spec override,
    /// restored if the key later reverts to a plain width or is removed.
    base_depth: Option<f32>,
    /// The DE-wide pinned carve height (0 = follow) while a `(relief)` value's
    /// `h=` is installed, restored when the key reverts.
    base_height: Option<f32>,
}

impl Patch {
    /// Surface-local px per virtual unit — `Patch::scale` itself.
    ///
    /// The grid surface is PINNED at buffer_scale 1 (cce-ui ignores scale
    /// events for grid apps; patch.scale is the sole resolution authority),
    /// so surface-local coordinates ARE buffer px at every output scale:
    /// pointer events arrive in that space and input regions are interpreted
    /// in it. The /ui division that used to live here calibrated against the
    /// compositor's old hit-test, which handed out raw layout offsets —
    /// numerically buffer/ui only at zoom 1 on the pow2 patch quantization —
    /// and at any other camera state it displaced the input region off the
    /// items (presses read as background) and tore the press position apart
    /// from the drag deltas (the flung-item bug). The compositor now speaks
    /// true surface coordinates, so the patch scale is used unmodified.
    fn surface_per_virtual(&self) -> f64 {
        self.scale
    }
}

/// What changed on the canvas since the last painted frame, in VIRTUAL
/// units. The surface is the whole patch — 7552x8160 px on a HiDPI laptop —
/// and an image dragged across it changes a few hundred pixels a frame;
/// repainting and re-compositing all sixty million for each pointer event is
/// what made the drag stutter. So item edits record the rects they touched
/// (`GridApp::touch`), and `take_damage` hands cce-ui their union.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Damage {
    /// Nothing the items did.
    Nothing,
    /// The union of the item rects touched: (x0, y0, x1, y1).
    Rect(f64, f64, f64, f64),
    /// Anything else: repaint it all.
    Full,
}

/// An in-flight item drag.
///
/// The item follows pointer DELTAS, not `patch + position`. The compositor can
/// re-issue the grid patch at any moment — it did so on the very first motion
/// event of a drag during testing, moving the patch origin by half a screen —
/// and the pointer events in flight are still in the OLD surface's coordinate
/// space, so an absolute mapping teleports the item by the origin delta. A
/// delta is the same number in either space.
struct Drag {
    index: usize,
    /// Previous pointer position, surface-local.
    last_pos: (f32, f32),
    /// Patch origin the previous position was measured against. When this
    /// changes, the incoming position is in a different space than the last
    /// one, so that step is used only to re-baseline.
    last_origin: (f64, f64),
    /// Set once the pointer actually travels, so a plain click does not
    /// rewrite the sidecar.
    moved: bool,
}

/// An in-flight corner resize. Delta-driven for the same reason `Drag` is:
/// the patch can be re-issued under the pointer mid-gesture.
struct Resize {
    index: usize,
    corner: Corner,
    last_pos: (f32, f32),
    last_origin: (f64, f64),
    moved: bool,
}

/// The `line_relief` key's three states — see [`Style::line_relief`].
enum LineRelief {
    /// Key absent: follow the DE-wide relief material.
    Material,
    /// Plain integer: lip width in virtual units, 0 = no lip.
    Width(f64),
    /// A `(relief)` value: its own width/depth/profile, editable in place
    /// with `cce-relief --key style.surface.desktop.line_relief`. The raw
    /// string rides along as the change detector.
    Spec(String, cce_ui::relief_spec::ReliefSpec),
}

/// Style knobs, re-read per frame from the shared config (cheap: cce-ui
/// caches the parse on mtime), with the same defaults the compositor uses.
struct Style {
    cell_w: f64,
    cell_h: f64,
    gap_width: f64,
    cell_inset: f64,
    corner_radius: f64,
    gap_color: [f32; 4],
    cell_color: [f32; 4],
    /// Grid-line lip material: a plain integer width (0 = no lip), a full
    /// `(relief)` value, or absent = the DE-wide material. Negative integers
    /// mean unset.
    line_relief: LineRelief,
    /// The image resize handles, from the same `border` keys the windows'
    /// handles use: diameter in virtual units (logical px at zoom 1), and
    /// the resting and hovered colours.
    handle_width: f64,
    handle_color: [f32; 4],
    handle_hover_color: [f32; 4],
}

fn style() -> Style {
    use cce_ui::config::{get_color, get_i64};
    // get_color returns raw sRGB; the render pipeline (like every cce-ui
    // widget color) expects linear.
    let linear = |c: [f32; 4]| {
        let f = cce_ui::color::srgb_to_linear;
        [f(c[0]), f(c[1]), f(c[2]), c[3]]
    };
    Style {
        // Per-axis sizes; the legacy square grid_cell_size is the fallback
        // for both, mirroring the compositor's config resolution.
        cell_w: {
            let legacy = get_i64("/style/surface/desktop/grid_cell_size", 512);
            get_i64("/style/surface/desktop/grid_cell_width", legacy) as f64
        },
        cell_h: {
            let legacy = get_i64("/style/surface/desktop/grid_cell_size", 512);
            get_i64("/style/surface/desktop/grid_cell_height", legacy) as f64
        },
        gap_width: (get_i64("/style/surface/desktop/gap_width", 16).max(0)) as f64,
        cell_inset: get_i64("/style/surface/desktop/cell_fade_inset", 0) as f64,
        // The silhouette radius (cce-ui RFC Phase 7a spelling).
        corner_radius: get_i64("/style/surface/plate/root/corner_radius", 12) as f64,
        gap_color: linear(
            get_color("/style/surface/desktop/gap_color")
                .unwrap_or([0.686, 0.796, 0.867, 1.0]),
        ),
        cell_color: linear(
            get_color("/style/surface/desktop/cell_color")
                .unwrap_or([0.0, 0.0, 0.0, 1.0]),
        ),
        line_relief: match cce_ui::config::get_string("/style/surface/desktop/line_relief") {
            // A string value is a (relief) spec; an unparseable one reads
            // as unset rather than as some accidental width.
            Some(s) => match cce_ui::relief_spec::ReliefSpec::parse(&s) {
                Some(spec) => LineRelief::Spec(s, spec),
                None => LineRelief::Material,
            },
            None => match get_i64("/style/surface/desktop/line_relief", -1) {
                v if v < 0 => LineRelief::Material,
                v => LineRelief::Width(v as f64),
            },
        },
        handle_width: cce_ui::config::get_f32("/style/surface/border/handle_width", 32.0).max(4.0) as f64,
        handle_color: linear(
            get_color("/style/surface/border/color_focused").unwrap_or([0.478, 0.635, 0.969, 1.0]),
        ),
        handle_hover_color: linear(
            get_color("/style/surface/border/color_hover").unwrap_or([0.659, 0.780, 0.980, 1.0]),
        ),
    }
}

/// Never emit more cells than this per frame, whatever the patch/config says
/// (a degenerate period must not turn into an unbounded display list).
const MAX_CELLS: usize = 8192;

impl GridApp {
    /// Install (or roll back) the process-wide material a `(relief)` value
    /// carries — depth into the style registry, profile into the wall LUT.
    /// This app draws nothing but the grid, so process-global IS
    /// per-feature; a change detector keeps it idempotent per frame.
    fn sync_relief_material(&mut self, line_relief: &LineRelief) {
        match line_relief {
            LineRelief::Spec(raw, spec) => {
                if self.applied_relief.as_deref() == Some(raw.as_str()) {
                    return;
                }
                if self.base_depth.is_none() {
                    self.base_depth = Some(cce_ui::layout::bevel_depth());
                }
                if self.base_height.is_none() {
                    self.base_height = Some(cce_ui::layout::bevel_height().unwrap_or(0.0));
                }
                if let Ok(mut reg) = cce_ui::layout::get_style_registry().write() {
                    if let Some(d) = spec.light {
                        reg.set_float("bevel_depth", d);
                    }
                    // A pinned drop (`h=0.5mm`) is a length: stored as one,
                    // so it re-resolves if the display metric changes.
                    match spec.height {
                        Some(h) => reg.set_len("bevel_height", h),
                        None => {
                            if let Some(b) = self.base_height {
                                reg.set_float("bevel_height", b);
                            }
                        }
                    }
                }
                cce_ui::layout::install_wall_profile_spec(spec.profile.as_deref());
                self.applied_relief = Some(raw.clone());
            }
            _ if self.applied_relief.is_some() => {
                // The key reverted to a plain width or vanished: back to
                // the DE-wide material the registry still carries.
                if let Ok(mut reg) = cce_ui::layout::get_style_registry().write() {
                    if let Some(d) = self.base_depth.take() {
                        reg.set_float("bevel_depth", d);
                    }
                    if let Some(h) = self.base_height.take() {
                        reg.set_float("bevel_height", h);
                    }
                }
                let global = cce_ui::layout::get_style_registry()
                    .read()
                    .ok()
                    .and_then(|reg| reg.get_string("bevel_profile_spec"));
                cce_ui::layout::install_wall_profile_spec(global.as_deref());
                self.applied_relief = None;
            }
            _ => {}
        }
    }

    fn paint(&mut self, pc: &mut PaintCtx, size: LogicalSize) {
        let Some(p) = self.patch else { return };
        if p.scale <= 0.0 {
            return;
        }
        let st = style();
        let period_x = st.cell_w + st.gap_width;
        let period_y = st.cell_h + st.gap_width;
        if period_x < 1.0 || period_y < 1.0 {
            return;
        }

        // The rail surface: the whole patch in gap color.
        pc.quad(
            Rect { x: 0.0, y: 0.0, width: size.width, height: size.height },
            st.gap_color,
        );

        // Visible cell box within its period slot, in virtual units.
        let inset_x = st.cell_inset.clamp(0.0, (st.cell_w / 2.0 - 1.0).max(0.0));
        let inset_y = st.cell_inset.clamp(0.0, (st.cell_h / 2.0 - 1.0).max(0.0));
        let len_w = st.cell_w - 2.0 * inset_x;
        let len_h = st.cell_h - 2.0 * inset_y;
        let s = p.scale;
        // Span-widened like every other corner in the DE (window clips,
        // fallback cells, cce-ui plates): at corner_shape > 2 a raw-radius
        // superellipse hugs the corner and reads nearly square, and a tiled
        // window's widened arc must land exactly on its cell's. Clamped to a
        // quarter sweep like the compositor's widen_corner_radius.
        let cell_px = len_w.min(len_h) * s;
        let radius = ((st.corner_radius * s)
            * cce_ui::layout::corner_span_factor() as f64)
            .min(cell_px / 2.0) as f32;
        // The relief lives on the LINES, never the cells: each cell's recess
        // rect is expanded past the cell edge so the wall sits in the rail
        // band, rolling down from the rail face to the cell floor. The roll
        // is the ROOT_PLATE-EDGE treatment — `layout::bevel_width` clamped to
        // a fraction of the rail, the widget convention — so the rail reads
        // as a flat plate face with a narrow lip where it meets each sunken
        // cell. (A half-gap-wide wall turned the whole rail into a ramp and
        // read far heavier than any plate edge in the toolkit.) Rings stay
        // inside their own half-rail, so neighbors never overlap; the outer
        // radius offsets by the roll to stay concentric with the cell arc.
        // style.surface.desktop.line_relief overrides the roll: a plain
        // width (0 = no lip), or a full (relief) value carrying its own
        // width/depth/profile. Explicit widths clamp to the half-rail (the
        // rings' geometric budget); the material default keeps the tighter
        // root plate clamp.
        self.sync_relief_material(&st.line_relief);
        let roll = match &st.line_relief {
            LineRelief::Material => {
                (cce_ui::layout::bevel_width() as f64).min(st.gap_width * 0.25)
            }
            LineRelief::Width(w) => w.min(st.gap_width / 2.0),
            LineRelief::Spec(_, spec) => (spec.width as f64).min(st.gap_width / 2.0),
        }
        .max(0.0)
            * s;
        let lip = roll >= 0.5;

        // One extra ring of cells beyond the patch: a border cell outside the
        // patch still owns the inner half of the boundary rail's shading.
        let col0 = (p.x / period_x).floor() as i64 - 1;
        let col1 = ((p.x + p.w) / period_x).ceil() as i64 + 1;
        let row0 = (p.y / period_y).floor() as i64 - 1;
        let row1 = ((p.y + p.h) / period_y).ceil() as i64 + 1;
        let mut cells = 0usize;
        for col in col0..col1 {
            for row in row0..row1 {
                if cells >= MAX_CELLS {
                    return;
                }
                cells += 1;
                let vx = col as f64 * period_x + inset_x;
                let vy = row as f64 * period_y + inset_y;
                let rect = Rect {
                    x: ((vx - p.x) * s) as f32,
                    y: ((vy - p.y) * s) as f32,
                    width: (len_w * s) as f32,
                    height: (len_h * s) as f32,
                };
                pc.rounded_rect(rect, radius, (true, true, true, true), st.cell_color);
            }
        }
        // The relief on the LINES is ONE primitive for the whole patch: a
        // lattice carve, folded per pixel to the nearest cell, so the rail
        // between two cells and the crossing where four meet are a single
        // profile evaluation — true mitres. This replaced one recess ring per
        // cell: N free overlays whose rounded corners stacked in colour space
        // at every crossing and read as overlapping effects, and whose ring
        // walls (straddling a boundary inflated by the roll) overlapped each
        // other down the rail centre whenever the roll exceeded a quarter
        // gap. The wall runs from each cell's edge outward over `roll`.
        if lip {
            let origin = (
                ((inset_x + len_w * 0.5 - p.x) * s) as f32,
                ((inset_y + len_h * 0.5 - p.y) * s) as f32,
            );
            pc.lattice(
                Rect { x: 0.0, y: 0.0, width: size.width, height: size.height },
                ((period_x * s) as f32, (period_y * s) as f32),
                origin,
                ((len_w * s) as f32, (len_h * s) as f32),
                radius,
                (roll as f32).max(1.0),
            );
        }

        // Pinned items sit ON the canvas, so they are placed by the same
        // world->patch mapping as the cells and drawn after them. The whole
        // grid surface is below every window, so an item never covers an app.
        self.layout_cards(&p, size);
        let screen = |x: f64, y: f64, w: f64, h: f64| Rect {
            x: ((x - p.x) * s) as f32,
            y: ((y - p.y) * s) as f32,
            width: (w * s) as f32,
            height: (h * s) as f32,
        };
        // Cull off-patch items: at a far zoom-out the patch can hold
        // hundreds of squares, and an item that is not on it costs a draw
        // for nothing. A margin keeps a card's label (drawn above it).
        let on_patch = |r: &Rect| {
            let m = (CARD_LABEL * 2.0 * s) as f32;
            !(r.x + r.width < -m || r.y + r.height < -m || r.x > size.width + m || r.y > size.height + m)
        };
        // Groups are frames behind everything else.
        for (item, _) in &self.items {
            let Kind::Group(label) = &item.kind else { continue };
            let r = screen(item.x, item.y, item.w, item.h);
            if !on_patch(&r) {
                continue;
            }
            let mut c = canvas_color(item.color.as_deref()).unwrap_or([0.55, 0.55, 0.6, 1.0]);
            let rad = (CARD_RADIUS * s) as f32;
            let mut fill = c;
            fill[3] = 0.07;
            c[3] = 0.6;
            pc.border(r, (rad, rad, rad, rad), fill, c, (2.0 * s).max(1.0) as f32);
            if !label.is_empty() {
                let size = (CARD_LABEL * 1.4 * s) as f32;
                pc.text_with(label.clone(), r.x, r.y - size * 1.5, size, srgb_u8(cce_ui::colors::TEXT_FG), Some("sans-serif".into()), None);
            }
        }
        // Edges under the cards, as Obsidian draws them.
        self.paint_edges(pc, &p);
        for (index, (item, id)) in self.items.iter().enumerate() {
            let rect = screen(item.x, item.y, item.w, item.h);
            if !on_patch(&rect) {
                continue;
            }
            match &item.kind {
                Kind::Image(_) => {
                    if let Some(id) = *id {
                        pc.image(id, rect, 1.0);
                    }
                }
                Kind::Group(_) | Kind::Other(_) => continue,
                _ => {
                    // Text draws after every shape, so an item above cannot
                    // hide this card's text by covering it: clip the text to
                    // what no higher item covers instead.
                    let above: Vec<Rect> = self.items[index + 1..]
                        .iter()
                        .filter(|(o, _)| o.interactive())
                        .map(|(o, _)| screen(o.x, o.y, o.w, o.h))
                        .collect();
                    self.paint_card(pc, &p, item, rect, &above)
                }
            }
            // "Connect to…" is waiting for its far end: ring the near one.
            if self.connecting.as_deref() == Some(item.node.as_str()) {
                let rad = (CARD_RADIUS * s) as f32;
                pc.border(rect, (rad, rad, rad, rad), [0.0, 0.0, 0.0, 0.0], st.handle_hover_color, (3.0 * s).max(1.5) as f32);
            }
            // Window-adjust mode: the hovered item's four corner handles, in
            // the same colours as the windows' handles, the hovered one lit.
            // Sized in virtual units, so they scale with the canvas rather
            // than holding a screen size the way the compositor's do — this
            // client never learns the camera zoom.
            if self.adjust && self.hover_item == Some(index) {
                let (r, discs) = handle_discs(item, st.handle_width);
                for (corner, cx, cy) in discs {
                    let color = if self.hover == Some((index, corner)) {
                        st.handle_hover_color
                    } else {
                        st.handle_color
                    };
                    pc.circle(((cx - p.x) * s) as f32, ((cy - p.y) * s) as f32, (r * s) as f32, color);
                }
            }
        }
    }

    /// Lay out every on-patch note and text card that has no layout yet, or
    /// one for another width or text. A note's text is read here, once;
    /// a change to it on disk drops the cache (`vault_changed`).
    fn layout_cards(&mut self, p: &Patch, size: LogicalSize) {
        let s = p.scale;
        let mut jobs: Vec<(String, String, f64)> = Vec::new();
        for (item, _) in &self.items {
            let lead = if matches!(item.kind, Kind::Link(_)) { link_glyph_room() as f64 } else { 0.0 };
            let width = (item.w - 2.0 * CARD_PAD - lead).max(20.0);
            let source = match &item.kind {
                Kind::Note(path) => {
                    if self.cards.get(&item.node).is_some_and(|c| (c.width - width).abs() < 0.5) {
                        continue;
                    }
                    std::fs::read_to_string(path).unwrap_or_else(|_| format!("*{} is missing*", item.name()))
                }
                Kind::Text(t) => t.clone(),
                // The URL alone; `paint_card` leads it with the `link` glyph.
                Kind::Link(u) => u.clone(),
                Kind::File(_) => format!("**{}**", item.name()),
                _ => continue,
            };
            let (rx, ry) = ((item.x - p.x) * s, (item.y - p.y) * s);
            if rx + item.w * s < 0.0 || ry + item.h * s < 0.0 || rx > size.width as f64 || ry > size.height as f64 {
                continue;
            }
            if self.cards.get(&item.node).is_some_and(|c| c.source == source && (c.width - width).abs() < 0.5) {
                continue;
            }
            jobs.push((item.node.clone(), source, width));
        }
        if jobs.is_empty() {
            return;
        }
        let theme = Theme { body_font: "sans-serif".into(), mono_font: "monospace".into(), size: CARD_TEXT };
        let m = self.measure.get_or_insert_with(|| ShapingMeasure::new(true));
        for (node, source, width) in jobs {
            let blocks = markdown::blocks(&source);
            // No index here to resolve links against: they all draw as links.
            let layout = markdown::layout(&blocks, width as f32, &theme, m, &|_| true);
            self.cards.insert(node, CardCache { source, width, layout });
        }
    }

    fn paint_card(&self, pc: &mut PaintCtx, p: &Patch, item: &items::DesktopItem, r: Rect, above: &[Rect]) {
        let s = p.scale;
        let rad = (CARD_RADIUS * s) as f32;
        let border = canvas_color(item.color.as_deref()).unwrap_or([0.42, 0.42, 0.48, 1.0]);
        pc.border(r, (rad, rad, rad, rad), [0.06, 0.06, 0.08, 1.0], border, (1.5 * s).max(1.0) as f32);
        // A file card is labelled with its name above it, as in Obsidian.
        if matches!(item.kind, Kind::Note(_) | Kind::File(_)) {
            let size = (CARD_LABEL * s) as f32;
            let name = item.name();
            let name = name.strip_suffix(".md").unwrap_or(&name).to_string();
            let strip = Rect { x: r.x, y: r.y - size * 1.8, width: r.width, height: size * 1.8 };
            for frag in uncovered(strip, above) {
                pc.clip(frag, |pc| {
                    pc.text_with(name.clone(), r.x + rad * 0.5, r.y - size * 1.6, size, srgb_u8(cce_ui::colors::TEXT_DIM), Some("sans-serif".into()), None)
                });
            }
        }
        if let Some(c) = self.cards.get(&item.node) {
            let pad = (CARD_PAD * s) as f32;
            let inner = Rect { x: r.x + pad * 0.5, y: r.y + pad * 0.5, width: r.width - pad, height: r.height - pad };
            // A link card's URL is led by the `link` glyph, in the colour
            // the URL is drawn in, centred on its first line — the way
            // cce-ui's markdown leads an embed it cannot show inline. The
            // URL was laid out `link_glyph_room` narrower to make the room.
            let glyph = matches!(item.kind, Kind::Link(_)).then(|| {
                let (size, side) = (CARD_TEXT, link_glyph_side());
                let (y, color) = c
                    .layout
                    .draws
                    .iter()
                    .find_map(|d| match d {
                        markdown::Draw::Text { y, color, .. } => Some((*y, *color)),
                        _ => None,
                    })
                    .unwrap_or((0.0, cce_ui::colors::TEXT_FG));
                // A text draw's colour is linear (it is painted through
                // `srgb_u8`); a glyph is tinted in sRGB, as a text colour.
                let color = cce_ui::colors::to_srgb(color);
                let sf = s as f32;
                let rect = Rect {
                    x: r.x + pad,
                    y: r.y + pad + (y + 0.5 * (size * 1.3 - side)) * sf,
                    width: side * sf,
                    height: side * sf,
                };
                (rect, color)
            });
            let lead = if glyph.is_some() { link_glyph_room() * s as f32 } else { 0.0 };
            for frag in uncovered(inner, above) {
                pc.clip(frag, |pc| {
                    if let Some((rect, color)) = glyph {
                        pc.icon("link", rect, color);
                    }
                    c.layout.paint_scaled(pc, (r.x + pad + lead, r.y + pad), s as f32, frag)
                });
            }
        }
    }

    /// Straight edges between side midpoints, an arrowhead at the far end.
    fn paint_edges(&self, pc: &mut PaintCtx, p: &Patch) {
        let s = p.scale;
        let by_node: std::collections::HashMap<&str, &items::DesktopItem> =
            self.items.iter().map(|(i, _)| (i.node.as_str(), i)).collect();
        let to_screen = |(x, y): (f64, f64)| (((x - p.x) * s) as f32, ((y - p.y) * s) as f32);
        let color = [0.62, 0.62, 0.68, 0.9];
        let width = (EDGE_WIDTH * s).max(1.0) as f32;
        // Edges lie under every card, so a label's text is clipped to what
        // the cards leave uncovered (text would otherwise draw over them).
        let cards: Vec<Rect> = self
            .items
            .iter()
            .filter(|(i, _)| i.interactive())
            .map(|(i, _)| Rect {
                x: ((i.x - p.x) * s) as f32,
                y: ((i.y - p.y) * s) as f32,
                width: (i.w * s) as f32,
                height: (i.h * s) as f32,
            })
            .collect();
        for e in &self.edges {
            let (Some(a), Some(b)) = (by_node.get(e.from.as_str()), by_node.get(e.to.as_str())) else { continue };
            let (pa, _) = anchor(a, e.from_side.as_deref(), center(b));
            let (pb, nb) = anchor(b, e.to_side.as_deref(), center(a));
            let (sa, sb) = (to_screen(pa), to_screen(pb));
            pc.vector(sa.0, sa.1, sb.0, sb.1, width, color, Cap::Round);
            if e.arrow {
                // The head points into the far node, along its side's normal.
                let len = ARROW_LEN;
                let base = (pb.0 + nb.0 * len, pb.1 + nb.1 * len);
                let perp = (-nb.1 * len * 0.5, nb.0 * len * 0.5);
                for side in [1.0, -1.0] {
                    let wing = to_screen((base.0 + perp.0 * side, base.1 + perp.1 * side));
                    pc.vector(sb.0, sb.1, wing.0, wing.1, width, color, Cap::Round);
                }
            }
            if let Some(label) = e.label.as_deref().filter(|l| !l.is_empty()) {
                let size = (CARD_LABEL * s) as f32;
                let mid = ((sa.0 + sb.0) / 2.0, (sa.1 + sb.1) / 2.0);
                let w = label.chars().count() as f32 * size * 0.55;
                pc.rounded_rect(
                    Rect { x: mid.0 - w / 2.0 - 4.0, y: mid.1 - size * 0.7, width: w + 8.0, height: size * 1.4 },
                    size * 0.3,
                    (true, true, true, true),
                    [0.06, 0.06, 0.08, 0.9],
                );
                let strip = Rect { x: mid.0 - w / 2.0 - 4.0, y: mid.1 - size * 0.7, width: w + 8.0, height: size * 1.4 };
                for frag in uncovered(strip, &cards) {
                    pc.clip(frag, |pc| {
                        pc.text_with(label.to_string(), mid.0 - w / 2.0, mid.1 - size * 0.5, size, srgb_u8(cce_ui::colors::TEXT_FG), Some("sans-serif".into()), None)
                    });
                }
            }
        }
    }
}

/// The parts of `r` that none of `over` covers, as rectangles: each
/// overlapping rect splits a fragment into up to four bands around it.
fn uncovered(r: Rect, over: &[Rect]) -> Vec<Rect> {
    let mut frags = vec![r];
    for o in over {
        let mut next = Vec::with_capacity(frags.len());
        for f in frags {
            let (fx1, fy1, ox1, oy1) = (f.x + f.width, f.y + f.height, o.x + o.width, o.y + o.height);
            if o.x >= fx1 || ox1 <= f.x || o.y >= fy1 || oy1 <= f.y {
                next.push(f);
                continue;
            }
            let (top, bottom) = (o.y.max(f.y), oy1.min(fy1));
            let bands = [
                Rect { x: f.x, y: f.y, width: f.width, height: top - f.y },
                Rect { x: f.x, y: bottom, width: f.width, height: fy1 - bottom },
                Rect { x: f.x, y: top, width: o.x - f.x, height: bottom - top },
                Rect { x: ox1, y: top, width: fx1 - ox1, height: bottom - top },
            ];
            next.extend(bands.into_iter().filter(|b| b.width > 0.5 && b.height > 0.5));
        }
        frags = next;
    }
    frags
}

fn center(i: &items::DesktopItem) -> (f64, f64) {
    (i.x + i.w / 2.0, i.y + i.h / 2.0)
}

/// Where an edge meets an item: the midpoint of the named side, or of the
/// side facing `toward`; with that side's outward normal.
fn anchor(i: &items::DesktopItem, side: Option<&str>, toward: (f64, f64)) -> ((f64, f64), (f64, f64)) {
    let (cx, cy) = center(i);
    let side = side.map(str::to_string).unwrap_or_else(|| {
        let (dx, dy) = (toward.0 - cx, toward.1 - cy);
        if dx.abs() * i.h >= dy.abs() * i.w {
            if dx >= 0.0 { "right" } else { "left" }.to_string()
        } else if dy >= 0.0 {
            "bottom".to_string()
        } else {
            "top".to_string()
        }
    });
    match side.as_str() {
        "left" => ((i.x, cy), (-1.0, 0.0)),
        "top" => ((cx, i.y), (0.0, -1.0)),
        "bottom" => ((cx, i.y + i.h), (0.0, 1.0)),
        _ => ((i.x + i.w, cy), (1.0, 0.0)),
    }
}

/// The `link` glyph's side on a link card, in card units (the size cce-ui's
/// markdown gives the glyph leading an embed).
fn link_glyph_side() -> f32 {
    (CARD_TEXT * 0.85).round()
}

/// What a link card's URL is moved right by: the glyph and a gap.
fn link_glyph_room() -> f32 {
    link_glyph_side() + (CARD_TEXT * 0.4).round()
}

fn srgb_u8(linear: [f32; 4]) -> [u8; 3] {
    let c = cce_ui::colors::to_srgb(linear);
    [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8]
}

impl GridApp {
    /// Record that item `index`'s rect, as it is NOW, differs from the last
    /// painted frame. A move or resize calls it before and after the edit.
    /// The handle discs lie inside the rect, so they are covered too.
    fn touch(&mut self, index: usize) {
        let Some((item, _)) = self.items.get(index) else { return };
        // A card's label sits above it, and an edge's head and label stray
        // a little past the rects it joins.
        let m = CARD_LABEL * 2.5 + ARROW_LEN;
        let (mut x0, mut y0, mut x1, mut y1) = (item.x - m, item.y - m, item.x + item.w + m, item.y + item.h + m);
        // An edge runs between its two items, so moving one end repaints the
        // box spanning both.
        let node = item.node.as_str();
        for e in &self.edges {
            let other = if e.from == node {
                &e.to
            } else if e.to == node {
                &e.from
            } else {
                continue;
            };
            if let Some((o, _)) = self.items.iter().find(|(o, _)| o.node == *other) {
                x0 = x0.min(o.x - m);
                y0 = y0.min(o.y - m);
                x1 = x1.max(o.x + o.w + m);
                y1 = y1.max(o.y + o.h + m);
            }
        }
        self.damage = match self.damage {
            Damage::Nothing => Damage::Rect(x0, y0, x1, y1),
            Damage::Rect(a, b, c, d) => Damage::Rect(a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            Damage::Full => Damage::Full,
        };
    }

    /// The topmost item under a virtual-canvas point.
    fn item_at(&self, vx: f64, vy: f64) -> Option<usize> {
        self.items.iter().rposition(|(i, _)| {
            i.interactive() && vx >= i.x && vx < i.x + i.w && vy >= i.y && vy < i.y + i.h
        })
    }

    /// The corner handle under a virtual-canvas point, with a unit of slack
    /// around the disc's antialiased rim. Only the hovered item's handles
    /// are up, so only its discs can be hit.
    fn corner_at(&self, vx: f64, vy: f64) -> Option<(usize, Corner)> {
        if !self.adjust {
            return None;
        }
        let diameter = style().handle_width;
        for (index, (item, _)) in self.items.iter().enumerate().rev() {
            if self.hover_item != Some(index) {
                continue;
            }
            let (r, discs) = handle_discs(item, diameter);
            let reach = (r + 1.0) * (r + 1.0);
            for (corner, cx, cy) in discs {
                let (dx, dy) = (vx - cx, vy - cy);
                if dx * dx + dy * dy <= reach {
                    return Some((index, corner));
                }
            }
        }
        None
    }

    /// The board file's vault path (`Desktop.canvas`), what an attachment
    /// for it is placed relative to.
    fn board_file(&self) -> String {
        let vault = self.board.vault.as_deref();
        vault
            .and_then(|v| self.board.path.strip_prefix(v).ok())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Desktop.canvas".to_string())
    }

    /// Images pinned before the board kept them in the vault point outside
    /// it (the desktop folder), so they reach no other device and Obsidian
    /// shows them missing. Copy each in — the original stays put — and
    /// repoint its node. Runs at startup; a no-op once all are inside.
    fn adopt_outside_images(&mut self) {
        let Some(vault) = self.board.vault.clone() else { return };
        let board_file = self.board_file();
        let mut moved = 0;
        for (item, _) in &mut self.items {
            if let Kind::Image(p) = &item.kind {
                if let Some(new) = items::adopt_into_vault(p, &vault, &board_file) {
                    log::info!("[board] copied {} into the vault as {}", p.display(), new.display());
                    item.kind = Kind::Image(new);
                    moved += 1;
                }
            }
        }
        if moved > 0 {
            self.save_items();
        }
    }

    fn save_items(&mut self) {
        let model: Vec<items::DesktopItem> = self.items.iter().map(|(i, _)| i.clone()).collect();
        // The file had changed outside (a sync the watcher had not delivered
        // yet): the save merged it in, so show what was written.
        if let Some(merged) = self.board.save(&model, &self.edges) {
            self.replace_items(merged.items, merged.edges);
        }
    }

    /// Show `fresh` and `edges` in place of the current board, keeping the
    /// textures of images that are still there.
    fn replace_items(&mut self, fresh: Vec<items::DesktopItem>, edges: Vec<Edge>) {
        let mut old: Vec<(items::DesktopItem, Option<u32>)> = std::mem::take(&mut self.items);
        for mut item in fresh {
            let tex = old
                .iter()
                .position(|(o, _)| o.node == item.node && o.kind == item.kind)
                .and_then(|i| old.swap_remove(i).1);
            self.assign_id(&mut item);
            self.items.push((item, tex));
        }
        for (_, id) in old {
            if let Some(id) = id {
                cce_ui::vk::free_image(id);
            }
        }
        self.edges = edges;
        self.cards.clear();
        self.dragging = None;
        self.resizing = None;
        self.hover = None;
        self.hover_item = None;
        self.damage = Damage::Full;
        self.upload_missing();
        self.report_items();
    }

    /// Double-click: a note opens in cce-notes, a link or a file in its
    /// default app.
    fn activate(&mut self, index: usize) {
        let Some((item, _)) = self.items.get(index) else { return };
        match &item.kind {
            Kind::Note(p) => items::open_in_notes(p),
            Kind::Image(p) | Kind::File(p) => items::open_externally(&p.to_string_lossy()),
            Kind::Link(u) => items::open_externally(u),
            _ => {}
        }
    }

    /// Pin a card next to item `index` (or at the patch centre).
    fn place_beside(&self, index: Option<usize>) -> (f64, f64) {
        match index.and_then(|i| self.items.get(i)) {
            Some((it, _)) => (it.x + it.w + 40.0, it.y),
            None => self.patch.map(|p| (p.x + p.w / 2.0 - CARD_W / 2.0, p.y + p.h / 2.0 - CARD_H / 2.0)).unwrap_or((0.0, 0.0)),
        }
    }

    /// A new note in the vault ("Desktop note.md", numbered if taken),
    /// pinned as a card and opened in cce-notes for writing.
    fn new_note_card(&mut self, beside: Option<usize>) {
        let Some(vault) = self.board.vault.clone() else {
            items::report_failure("note cards need a notes vault (vault { path } in config.kdl)");
            return;
        };
        let mut path = vault.join("Desktop note.md");
        let mut n = 2;
        while path.exists() {
            path = vault.join(format!("Desktop note {n}.md"));
            n += 1;
        }
        if let Err(e) = std::fs::write(&path, "") {
            items::report_failure(&format!("could not create the note: {e}"));
            return;
        }
        let (x, y) = self.place_beside(beside);
        let mut item = items::DesktopItem::new(board::new_id(), Kind::Note(path.clone()), x, y, CARD_W, CARD_H);
        self.assign_id(&mut item);
        self.items.push((item, None));
        self.save_items();
        self.report_items();
        items::open_in_notes(&path);
    }

    /// A text card becomes a note in the vault, named by its first line,
    /// and the card a note card of it — editable in cce-notes, where a
    /// text card is not editable here at all.
    fn convert_to_note(&mut self, index: usize) {
        let Some(vault) = self.board.vault.clone() else { return };
        let Some((item, _)) = self.items.get(index) else { return };
        let Kind::Text(text) = item.kind.clone() else { return };
        let stem: String = item
            .name()
            .chars()
            .map(|c| if "/\\:*?\"<>|#^[]".contains(c) { ' ' } else { c })
            .collect::<String>()
            .trim()
            .trim_start_matches(['#', ' '])
            .to_string();
        let stem = if stem.is_empty() { "Desktop note".to_string() } else { stem };
        let mut path = vault.join(format!("{stem}.md"));
        let mut n = 2;
        while path.exists() {
            path = vault.join(format!("{stem} {n}.md"));
            n += 1;
        }
        if let Err(e) = std::fs::write(&path, &text) {
            items::report_failure(&format!("could not create the note: {e}"));
            return;
        }
        self.items[index].0.kind = Kind::Note(path);
        self.cards.remove(&self.items[index].0.node);
        self.save_items();
    }

    fn menu_action(&mut self, node: &str, action: &str) {
        let Some(index) = self.items.iter().position(|(i, _)| i.node == node) else { return };
        match action {
            "open" => self.activate(index),
            "connect" => self.connecting = Some(node.to_string()),
            "disconnect" => {
                self.edges.retain(|e| e.from != node && e.to != node);
                self.save_items();
            }
            "new_note" => self.new_note_card(Some(index)),
            "convert" => self.convert_to_note(index),
            "remove" => {
                // A drag or resize on the removed item cannot outlive it.
                self.dragging = None;
                self.resizing = None;
                self.hover = None;
                self.hover_item = None;
                let (item, id) = self.items.remove(index);
                if let Some(id) = id {
                    cce_ui::vk::free_image(id);
                }
                self.edges.retain(|e| e.from != node && e.to != node);
                self.cards.remove(node);
                self.save_items();
                // The file itself stays where it was: this unpins it from the
                // desktop, it does not delete the user's file.
                log::info!("[items] removed {} from the desktop", item.name());
                self.report_items();
            }
            _ => {}
        }
    }

    /// The board or a card's note changed on disk.
    fn vault_changed(&mut self, paths: &[std::path::PathBuf]) -> bool {
        let mut changed = false;
        // A card's note: lay it out again from the new text.
        for (item, _) in &self.items {
            if let Kind::Note(p) = &item.kind {
                if paths.iter().any(|c| c == p) {
                    self.cards.remove(&item.node);
                    changed = true;
                }
            }
        }
        if paths.contains(&self.board.path) && self.board.changed_on_disk() {
            let was_unreadable = self.board.unreadable;
            match self.board.read() {
                Ok((fresh, edges)) => {
                    if was_unreadable {
                        // It reads at last: keep what was pinned meanwhile,
                        // and write it into the board now.
                        let local: Vec<items::DesktopItem> = self.items.iter().map(|(i, _)| i.clone()).collect();
                        let fresh = board::keep_local(fresh, local);
                        self.replace_items(fresh, edges);
                        self.save_items();
                        log::info!("[board] {} reads again; loaded it", self.board.path.display());
                    } else {
                        self.replace_items(fresh, edges);
                        log::info!("[board] reloaded {} after an outside edit", self.board.path.display());
                    }
                    changed = true;
                }
                Err(e) => log::warn!("[board] {} changed but does not read ({e}); keeping what is shown", self.board.path.display()),
            }
        }
        changed
    }

    /// Decode the images that have no texture yet (restored, or arrived by
    /// an outside edit) — on worker threads, a few at a time and shrunk to
    /// `items::MAX_TEX`; each comes back as `Message::ImageDecoded` and is
    /// uploaded there. This ran on the loop that draws the desktop, at full
    /// size: a board of phone photos stalled the desktop at login and kept
    /// ~48 MB of texture per photo shown a grid cell big.
    fn upload_missing(&mut self) {
        for (item, id) in &self.items {
            if id.is_some() || self.decoding.contains(&item.node) {
                continue;
            }
            let Kind::Image(path) = &item.kind else { continue };
            self.decoding.insert(item.node.clone());
            let (node, kind, path, sender) = (item.node.clone(), item.kind.clone(), path.clone(), self.sender.clone());
            std::thread::spawn(move || {
                let decoded = match std::fs::read(&path) {
                    Ok(bytes) => items::decode_texture(&bytes),
                    Err(e) => {
                        log::warn!("[items] {} is gone ({e}); not drawing it", path.display());
                        None
                    }
                };
                let _ = sender.send(Message::ImageDecoded { node, kind, decoded });
            });
        }
    }

    /// Name an item for the compositor's benefit (`DesktopItem::id`).
    fn assign_id(&mut self, item: &mut items::DesktopItem) {
        self.next_id += 1;
        item.id = self.next_id;
    }

    /// Tell the compositor what is pinned and where: `grid-items
    /// <id>:<x>:<y>:<w>:<h> ...`, virtual units, in draw order (last on
    /// top), the whole list every time. It is what lets the overview
    /// drag-selection pick images up beside windows and carry them in a
    /// group move — the compositor otherwise knows the images only as this
    /// surface's input region. Sent on every change to the list or to a
    /// rect this client made itself; the compositor moves its own copy
    /// during a group move and hears the settled list on `drop`.
    fn report_items(&self) {
        let mut line = String::from("grid-items");
        for (item, _) in self.items.iter().filter(|(i, _)| i.interactive()) {
            line.push_str(&format!(
                " {}:{:.2}:{:.2}:{:.2}:{:.2}",
                item.id, item.x, item.y, item.w, item.h
            ));
        }
        let _ = self.reporter.send(line);
    }
}

impl Application for GridApp {
    type Message = Message;

    fn create(_sender: cce_ui::engine::AppSender<Self::Message>) -> Self {
        // The app keeps calloop's sender; `AppSender` converts into it.
        let _sender: calloop::channel::Sender<Self::Message> = _sender.into();
        spawn_topic_listener("adjust", _sender.clone(), |line| {
            Some(Message::AdjustMode(line == "on"))
        });
        spawn_topic_listener("selection", _sender.clone(), parse_selection_line);
        let (board, loaded, edges) = board::Board::open();
        // The board's folder is watched when it is a vault: the board file
        // itself (an Obsidian edit, a sync) and the notes cards show.
        let watcher = board.vault.as_ref().and_then(|v| {
            let tx = _sender.clone();
            cce_vault::VaultWatcher::spawn(v, move |paths| {
                let _ = tx.send(Message::VaultChanged(paths));
            })
            .map_err(|e| log::warn!("[board] vault watcher: {e}"))
            .ok()
        });
        let mut app = Self {
            patch: None,
            items: Vec::new(),
            edges,
            board,
            _watcher: watcher,
            cards: std::collections::HashMap::new(),
            decoding: std::collections::HashSet::new(),
            measure: None,
            connecting: None,
            last_press: None,
            sender: _sender,
            reporter: spawn_reporter(),
            next_id: 0,
            dragging: None,
            adjust: false,
            hover_item: None,
            hover: None,
            resizing: None,
            damage: Damage::Full,
            painted: None,
            applied_relief: None,
            base_depth: None,
            base_height: None,
        };
        for mut item in loaded {
            app.assign_id(&mut item);
            app.items.push((item, None));
        }
        if app.board.unreadable {
            let path = app.board.path.display().to_string();
            std::thread::spawn(move || {
                items::notify(
                    "Desktop board unreadable",
                    &format!("{path} does not parse. The desktop shows nothing and saves nothing until it reads again; it is reloaded when it changes."),
                )
            });
        }
        app.adopt_outside_images();
        app.report_items();
        app
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "Desktop Grid".to_string(),
            app_id: "cce-grid".to_string(),
            // Nominal initial size; every real size comes from a patch.
            width: 640,
            height: 480,
            fullscreen: false,
            min_size: None,
        }
    }

    fn grid(&self) -> bool {
        true
    }

    fn grid_patch(&mut self, x: f64, y: f64, w: f64, h: f64, scale: f64) {
        self.patch = Some(Patch { x, y, w, h, scale });
    }

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, _exit: &mut bool) {
        // A group move step is one pointer event's worth of motion and
        // records the rects it touched, like a drag of this client's own.
        // The rest are rare, and each changes more than one item's rect (a
        // new texture, a reordered list, every handle): not worth a rect.
        if !matches!(msg, Message::SelectionMove(_)) {
            self.damage = Damage::Full;
        }
        match msg {
            Message::SelectionMove(moves) => {
                for (id, x, y) in moves {
                    let Some(index) = self.items.iter().position(|(i, _)| i.id == id) else {
                        continue;
                    };
                    self.touch(index);
                    let (item, _) = &mut self.items[index];
                    item.x = x;
                    item.y = y;
                    self.touch(index);
                }
                *needs_rebuild = true;
            }
            Message::MenuAction { node, action } => {
                self.menu_action(&node, &action);
                *needs_rebuild = true;
            }
            Message::AddItem(mut item) => {
                self.assign_id(&mut item);
                log::info!("[items] pinned {} at ({:.0}, {:.0})", item.name(), item.x, item.y);
                self.items.push((item, None));
                self.save_items();
                self.report_items();
                *needs_rebuild = true;
            }
            Message::VaultChanged(paths) => {
                if self.vault_changed(&paths) {
                    *needs_rebuild = true;
                } else {
                    // Nothing this client shows: keep the frame as it is.
                    self.damage = Damage::Nothing;
                }
            }
            Message::SelectionDrop => {
                self.save_items();
                self.report_items();
                log::info!("[items] group move dropped; sidecar saved");
                *needs_rebuild = true;
            }
            Message::ItemReady { mut item, pixels, px_w, px_h } => {
                self.assign_id(&mut item);
                let id = cce_ui::vk::upload_rgba(pixels, px_w, px_h);
                log::info!("[items] pinned {} at ({:.0}, {:.0})", item.name(), item.x, item.y);
                self.items.push((item, Some(id)));
                // Persist only the model — the texture id is per-process.
                self.save_items();
                self.report_items();
                *needs_rebuild = true;
            }
            Message::ImageDecoded { node, kind, decoded } => {
                self.decoding.remove(&node);
                let Some(d) = decoded else { return };
                // Still pinned, still that image, still without a texture.
                let Some((_, id)) = self.items.iter_mut().find(|(i, id)| i.node == node && i.kind == kind && id.is_none()) else {
                    return;
                };
                *id = Some(cce_ui::vk::upload_rgba(d.pixels, d.tex.0, d.tex.1));
                *needs_rebuild = true;
            }
            Message::AdjustMode(on) => {
                if self.adjust == on {
                    return;
                }
                self.adjust = on;
                if !on {
                    // A resize in flight finishes on its release; only the
                    // highlight goes with the handles.
                    self.hover = None;
                }
                *needs_rebuild = true;
            }
        }
    }

    fn tick(&mut self, _dt: f32, _needs_rebuild: &mut bool) {}

    /// Uploads happen here, not in `new`: a reconnect builds a fresh renderer
    /// and does not replay earlier uploads, so items restored from the sidecar
    /// (and any pinned before the reconnect) have to be handed over again.
    fn renderer_init(&mut self, _renderer: &mut cce_ui::vk::VkRenderer) {
        for (_, id) in self.items.iter_mut() {
            *id = None;
        }
        self.upload_missing();
    }

    /// Note cards draw text, which this surface never did before: the
    /// glyph pass is on, and system fonts carry the sans's bold and italic
    /// faces (the bundle has only its regular). `ShapingMeasure::new(true)`
    /// loads the same set.
    fn display_list_text(&self) -> bool {
        true
    }

    fn load_system_fonts(&self) -> bool {
        true
    }

    /// What a browser offers for an image on a page, best first: the raw
    /// bytes if the source has them, else a link to fetch.
    fn drop_mimes(&self) -> &'static [&'static str] {
        &[
            // Pixels beat links: no fetch, no ambiguity. Firefox offers these
            // for an image on a page; Chrome usually does not.
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/webp",
            // Preferred over text/uri-list because it names the IMAGE. When a
            // thumbnail is wrapped in a link — Google Images' exact markup —
            // uri-list is the result page and fetching it yields HTML, not a
            // picture. For an unwrapped image the two agree, so this never
            // does worse.
            "text/html",
            "text/uri-list",
            "text/x-moz-url",
            "text/plain;charset=utf-8",
            "text/plain",
        ]
    }

    fn handle_drop(
        &mut self,
        mime: &str,
        data: &[u8],
        pos: LogicalPosition,
        _needs_rebuild: &mut bool,
    ) {
        let Some(patch) = self.patch else { return };
        if patch.scale <= 0.0 {
            return;
        }
        // Plain text that is not a link or a path becomes a text card,
        // whole (the URI parsing below would keep only its first line).
        if mime.starts_with("text/plain") {
            let text = String::from_utf8_lossy(data).trim().to_string();
            let first = text.lines().next().unwrap_or("").trim();
            let linkish = first.contains("://") || first.starts_with('/') || first.starts_with("data:");
            if !text.is_empty() && !linkish {
                let s = patch.surface_per_virtual();
                let (vx, vy) = (patch.x + pos.x as f64 / s, patch.y + pos.y as f64 / s);
                let item = items::DesktopItem::new(board::new_id(), Kind::Text(text), vx - 125.0, vy - 60.0, 250.0, 120.0);
                let _ = self.sender.send(Message::AddItem(item));
                return;
            }
        }
        let Some(payload) = items::parse_payload(mime, data) else {
            items::report_failure(&format!("nothing usable in the dropped {mime}"));
            return;
        };
        // A Markdown file is a note card: nothing to fetch or decode.
        if let items::Payload::Uri(uri) = &payload {
            if let Some(path) = uri.strip_prefix("file://").map(items::percent_decode_path) {
                if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")) {
                    let s = patch.surface_per_virtual();
                    let (vx, vy) = (patch.x + pos.x as f64 / s, patch.y + pos.y as f64 / s);
                    let item = items::DesktopItem::new(
                        board::new_id(),
                        Kind::Note(path),
                        vx - CARD_W / 2.0,
                        vy - CARD_H / 2.0,
                        CARD_W,
                        CARD_H,
                    );
                    let _ = self.sender.send(Message::AddItem(item));
                    return;
                }
            }
        }
        let link = match &payload {
            items::Payload::Uri(u) if u.starts_with("http://") || u.starts_with("https://") => Some(u.clone()),
            _ => None,
        };

        // The drop point in world coordinates — the inverse of the mapping
        // `paint` uses to place cells, so the image lands under the cursor
        // whatever the camera is doing.
        let s = patch.surface_per_virtual();
        let vx = patch.x + pos.x as f64 / s;
        let vy = patch.y + pos.y as f64 / s;

        // Sized to fit inside one grid cell, keeping aspect: a phone
        // screenshot would otherwise land several squares wide.
        let st = style();
        let (cell_w, cell_h) = (st.cell_w.max(16.0), st.cell_h.max(16.0));
        let sender = self.sender.clone();
        let mime = mime.to_string();
        let vault = self.board.vault.clone();
        let board_file = self.board_file();
        std::thread::spawn(move || {
            let (bytes, name) = match items::fetch(payload) {
                Ok(v) => v,
                Err(e) => {
                    items::report_failure(&format!("could not fetch it: {e}"));
                    return;
                }
            };
            let Some(decoded) = items::decode_texture(&bytes) else {
                // A web page rather than a picture: pin it as a link card.
                if let Some(url) = link {
                    let item = items::DesktopItem::new(board::new_id(), Kind::Link(url), vx - 150.0, vy - 40.0, 300.0, 80.0);
                    let _ = sender.send(Message::AddItem(item));
                    return;
                }
                items::report_failure(&format!(
                    "the dropped {mime} is not an image cce can read ({} bytes)",
                    bytes.len()
                ));
                return;
            };
            // Save even though it is already decoded: the board points at a
            // file, and in a vault that file is what syncs with it.
            let path = match items::save_image(&bytes, &name, vault.as_deref(), &board_file) {
                Ok(p) => p,
                Err(e) => {
                    items::report_failure(&format!("could not save the image: {e}"));
                    return;
                }
            };
            // Sized by the image's own pixels; the texture may be smaller.
            let (nat_w, nat_h) = (decoded.natural.0 as f64, decoded.natural.1 as f64);
            let fit = (cell_w / nat_w).min(cell_h / nat_h).min(1.0);
            let w = nat_w * fit;
            let h = nat_h * fit;
            // Centred on the drop point.
            let item = items::DesktopItem::new(board::new_id(), Kind::Image(path), vx - w / 2.0, vy - h / 2.0, w, h);
            let (px_w, px_h) = decoded.tex;
            let _ = sender.send(Message::ItemReady { item, pixels: decoded.pixels, px_w, px_h });
        });
    }

    /// Exactly the pinned items, in surface-local px. Everything else on this
    /// surface stays click-through: the compositor no longer forces the grid
    /// layer transparent, it just misses this region, so the desktop keeps its
    /// background clicks (menu, overview exit, panning) while a press ON an
    /// image reaches this client. An empty region — the no-items case — is
    /// wholly transparent, which is the old behaviour exactly.
    fn input_regions(&self) -> Option<Vec<(i32, i32, i32, i32)>> {
        let Some(p) = self.patch else { return Some(Vec::new()) };
        if p.scale <= 0.0 {
            return Some(Vec::new());
        }
        let s = p.surface_per_virtual();
        let out: Vec<(i32, i32, i32, i32)> = self
            .items
            .iter()
            .filter(|(item, _)| item.interactive())
            .map(|(item, _)| {
                (
                    ((item.x - p.x) * s).round() as i32,
                    ((item.y - p.y) * s).round() as i32,
                    (item.w * s).round().max(1.0) as i32,
                    (item.h * s).round().max(1.0) as i32,
                )
            })
            .collect();
        Some(out)
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let Some(p) = self.patch else { return };
        if p.scale <= 0.0 {
            return;
        }
        let origin = (p.x, p.y);
        let s = p.surface_per_virtual();

        if let Some(rs) = self.resizing.as_mut() {
            let last_pos = rs.last_pos;
            rs.last_pos = (pos.x, pos.y);
            if rs.last_origin != origin {
                rs.last_origin = origin;
                return;
            }
            let dx = (pos.x - last_pos.0) as f64 / s;
            let dy = (pos.y - last_pos.1) as f64 / s;
            if dx == 0.0 && dy == 0.0 {
                return;
            }
            rs.moved = true;
            let (index, corner) = (rs.index, rs.corner);
            self.touch(index);
            if let Some((item, _)) = self.items.get_mut(index) {
                // Corners scale the image PROPORTIONALLY — an image stretched
                // out of its aspect is a different picture — by the mean of
                // the two edge ratios the drag asks for, anchored on the
                // opposite corner.
                let (sx, sy) = corner.sign();
                if !item.is_image() {
                    // A card has no aspect to keep: each edge follows.
                    let (old_w, old_h) = (item.w, item.h);
                    item.w = (old_w + sx * dx).max(MIN_CARD);
                    item.h = (old_h + sy * dy).max(MIN_CARD);
                    if sx < 0.0 {
                        item.x += old_w - item.w;
                    }
                    if sy < 0.0 {
                        item.y += old_h - item.h;
                    }
                    *needs_rebuild = true;
                    self.touch(index);
                    return;
                }
                let kw = (item.w + sx * dx) / item.w.max(1.0);
                let kh = (item.h + sy * dy) / item.h.max(1.0);
                let k = ((kw + kh) / 2.0).max(MIN_ITEM_SIZE / item.w.max(item.h).max(1.0));
                let (old_w, old_h) = (item.w, item.h);
                item.w = (old_w * k).max(MIN_ITEM_SIZE);
                item.h = old_h * (item.w / old_w.max(1.0));
                if sx < 0.0 {
                    item.x += old_w - item.w;
                }
                if sy < 0.0 {
                    item.y += old_h - item.h;
                }
                *needs_rebuild = true;
            }
            self.touch(index);
            return;
        }

        let Some(drag) = self.dragging.as_mut() else {
            // Idle motion: the item under the pointer gets the handles, and
            // the handle under it lights. A leave arrives as an off-screen
            // position and clears both.
            let vx = p.x + pos.x as f64 / s;
            let vy = p.y + pos.y as f64 / s;
            let item = self.item_at(vx, vy);
            if item != self.hover_item {
                // The handles leave one item and appear on another.
                for index in [self.hover_item, item].into_iter().flatten() {
                    self.touch(index);
                }
                self.hover_item = item;
                *needs_rebuild = true;
            }
            let hover = self.corner_at(vx, vy);
            if hover != self.hover {
                for (index, _) in [self.hover, hover].into_iter().flatten() {
                    self.touch(index);
                }
                self.hover = hover;
                *needs_rebuild = true;
            }
            return;
        };
        let last_pos = drag.last_pos;
        drag.last_pos = (pos.x, pos.y);
        if drag.last_origin != origin {
            // The surface moved under the pointer; this position cannot be
            // compared with the previous one. Re-baseline and wait.
            drag.last_origin = origin;
            return;
        }
        let dx = (pos.x - last_pos.0) as f64 / s;
        let dy = (pos.y - last_pos.1) as f64 / s;
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        drag.moved = true;
        let index = drag.index;
        self.touch(index);
        if let Some((item, _)) = self.items.get_mut(index) {
            item.x += dx;
            item.y += dy;
            *needs_rebuild = true;
        }
        self.touch(index);
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        let p = self.patch?;
        if p.scale <= 0.0 {
            return None;
        }
        if button == MouseButton::Right {
            if state != ElementState::Pressed {
                return None;
            }
            let s = p.surface_per_virtual();
            let vx = p.x + pos.x as f64 / s;
            let vy = p.y + pos.y as f64 / s;
            let hit = self.item_at(vx, vy)?;
            let item = &self.items[hit].0;
            let node = item.node.clone();
            let name = item.name();
            let mut entries: Vec<(&'static str, &'static str)> = Vec::new();
            match &item.kind {
                Kind::Note(_) => entries.push(("open", "Open in Notes")),
                Kind::Link(_) => entries.push(("open", "Open link")),
                Kind::Text(_) if self.board.vault.is_some() => entries.push(("convert", "Convert to note")),
                _ => {}
            }
            entries.push(("connect", "Connect to…"));
            if self.edges.iter().any(|e| e.from == node || e.to == node) {
                entries.push(("disconnect", "Disconnect"));
            }
            if self.board.vault.is_some() {
                entries.push(("new_note", "New note card"));
            }
            entries.push(("remove", "Remove from Desktop"));
            // The menu blocks until it is dismissed, so it cannot run on the
            // loop that has to keep drawing the desktop behind it.
            let sender = self.sender.clone();
            std::thread::spawn(move || {
                if let Some(action) = items::item_menu(&name, &entries) {
                    let _ = sender.send(Message::MenuAction { node, action });
                }
            });
            return None;
        }
        if button != MouseButton::Left {
            return None;
        }
        match state {
            ElementState::Pressed => {
                let s = p.surface_per_virtual();
                let vx = p.x + pos.x as f64 / s;
                let vy = p.y + pos.y as f64 / s;
                // Finishing a "Connect to…": this press names the far end.
                if let Some(from) = self.connecting.take() {
                    if let Some(hit) = self.item_at(vx, vy) {
                        let to = self.items[hit].0.node.clone();
                        if to != from {
                            self.edges.push(Edge {
                                id: board::new_id(),
                                from,
                                to,
                                from_side: None,
                                to_side: None,
                                label: None,
                                arrow: true,
                            });
                            self.save_items();
                            log::info!("[board] connected two items");
                        }
                    }
                    self.damage = Damage::Full;
                    *needs_rebuild = true;
                    return None;
                }
                // A second press on the same item, quickly: open it.
                if let Some(hit) = self.item_at(vx, vy) {
                    let node = self.items[hit].0.node.clone();
                    let now = std::time::Instant::now();
                    let double = self
                        .last_press
                        .as_ref()
                        .is_some_and(|(n, at)| *n == node && now.duration_since(*at) < DOUBLE_CLICK);
                    self.last_press = Some((node, now));
                    if double {
                        self.last_press = None;
                        self.activate(hit);
                        return None;
                    }
                }
                // A corner handle (adjust mode only) resizes; the body moves.
                if let Some((index, corner)) = self.corner_at(vx, vy) {
                    self.resizing = Some(Resize {
                        index,
                        corner,
                        last_pos: (pos.x, pos.y),
                        last_origin: (p.x, p.y),
                        moved: false,
                    });
                    return None;
                }
                // Last drawn is on top, so search backwards and take the
                // first hit.
                let hit = self.item_at(vx, vy)?;
                // Raise it: the one you grabbed should be the one you see,
                // and the next press should find it first. The handles
                // follow it to its new index.
                let item = self.items.remove(hit);
                self.items.push(item);
                // Raised over whatever overlapped it, and the handles of
                // the item that had them are gone.
                if let Some(index) = self.hover_item {
                    let index = if index > hit { index - 1 } else { index };
                    if index != hit {
                        self.touch(index);
                    }
                }
                self.touch(self.items.len() - 1);
                self.hover_item = Some(self.items.len() - 1);
                self.dragging = Some(Drag {
                    index: self.items.len() - 1,
                    last_pos: (pos.x, pos.y),
                    last_origin: (p.x, p.y),
                    moved: false,
                });
                *needs_rebuild = true;
            }
            ElementState::Released => {
                if let Some(rs) = self.resizing.take() {
                    if rs.moved {
                        self.save_items();
                        self.report_items();
                        if let Some((item, _)) = self.items.get(rs.index) {
                            log::info!("[items] resized {} to {:.0}x{:.0}", item.name(), item.w, item.h);
                        }
                    }
                    return None;
                }
                if let Some(drag) = self.dragging.take() {
                    if drag.moved {
                        self.save_items();
                        if let Some((item, _)) = self.items.get(drag.index) {
                            log::info!("[items] moved {} to ({:.0}, {:.0})", item.name(), item.x, item.y);
                        }
                    }
                    // The press raised the item even if it never moved,
                    // and the compositor's hit test wants the new order.
                    self.report_items();
                }
            }
        }
        None
    }

    fn handle_mouse_wheel(
        &mut self,
        _delta: &MouseScrollDelta,
        _pos: LogicalPosition,
        _needs_rebuild: &mut bool,
    ) {
    }

    fn handle_key_input(
        &mut self,
        _event: &KeyEvent,
        _needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        None
    }

    // style-audit: opt-out the desktop grid overlay draws the compositor cells, not a window

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<DisplayList> {
        // The runner renders this surface at scale 1 (patch.scale is the
        // resolution) but the toolkit-wide factor follows the output, and
        // text shapes at that factor: pin it to this surface's, or every
        // card's text would shape at twice the size it is placed at.
        cce_ui::scale::set_scale_factor(scale as f32);
        let mut pc = PaintCtx::new();
        self.paint(&mut pc, size);
        Some(pc.finish())
    }

    /// The item rects touched since the last frame, as one surface rect —
    /// or None (everything) when the frame's other inputs moved: the patch,
    /// the surface size, or any style value the paint reads.
    fn take_damage(&mut self, size: LogicalSize, _scale: f64) -> Option<(f32, f32, f32, f32)> {
        let damage = std::mem::replace(&mut self.damage, Damage::Nothing);
        let p = self.patch?;
        let st = style();
        let inputs = (
            p,
            (size.width as f64, size.height as f64),
            format!(
                "{} {} {} {} {} {:?} {:?} {} {:?} {:?} {} {} {:?}",
                st.cell_w,
                st.cell_h,
                st.gap_width,
                st.cell_inset,
                st.corner_radius,
                st.gap_color,
                st.cell_color,
                st.handle_width,
                st.handle_color,
                st.handle_hover_color,
                cce_ui::layout::corner_span_factor(),
                cce_ui::layout::bevel_width(),
                self.applied_relief,
            ),
        );
        let same = self.painted.as_ref() == Some(&inputs);
        self.painted = Some(inputs);
        if !same {
            return None;
        }
        match damage {
            Damage::Full => None,
            Damage::Nothing => Some((0.0, 0.0, 0.0, 0.0)),
            Damage::Rect(x0, y0, x1, y1) => {
                let s = p.scale;
                Some((
                    ((x0 - p.x) * s) as f32,
                    ((y0 - p.y) * s) as f32,
                    ((x1 - x0) * s) as f32,
                    ((y1 - y0) * s) as f32,
                ))
            }
        }
    }

    fn clear_color(&self) -> [f32; 4] {
        // Patch edges the display list somehow misses read as rail surface,
        // matching the compositor's always-on gap backdrop underneath.
        style().gap_color
    }
}

fn main() {
    // Default to info, not env_logger's error-only: this process runs
    // unattended as a session service, and a drop that quietly fails with
    // nothing in the log is indistinguishable from a drop that never
    // happened — which is exactly how the first Chrome failure presented.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    cce_ui::engine::run::<GridApp>();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(rs: &[Rect]) -> f32 {
        rs.iter().map(|r| r.width * r.height).sum()
    }

    #[test]
    fn uncovered_subtracts_overlaps() {
        let r = Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 };
        assert_eq!(uncovered(r, &[]).len(), 1);
        // A square in the middle leaves the frame around it.
        let hole = Rect { x: 40.0, y: 40.0, width: 20.0, height: 20.0 };
        let frags = uncovered(r, &[hole]);
        assert_eq!(frags.len(), 4);
        assert!((area(&frags) - (10000.0 - 400.0)).abs() < 0.01);
        // Covered whole: nothing left. Missed: untouched.
        assert!(uncovered(r, &[Rect { x: -5.0, y: -5.0, width: 200.0, height: 200.0 }]).is_empty());
        assert_eq!(uncovered(r, &[Rect { x: 200.0, y: 0.0, width: 10.0, height: 10.0 }]).len(), 1);
    }

    #[test]
    fn edges_meet_the_facing_side() {
        let a = items::DesktopItem::new("a".into(), Kind::Text(String::new()), 0.0, 0.0, 100.0, 50.0);
        let b = items::DesktopItem::new("b".into(), Kind::Text(String::new()), 300.0, 0.0, 100.0, 50.0);
        assert_eq!(anchor(&a, None, center(&b)), ((100.0, 25.0), (1.0, 0.0)));
        assert_eq!(anchor(&b, None, center(&a)), ((300.0, 25.0), (-1.0, 0.0)));
        assert_eq!(anchor(&a, Some("bottom"), center(&b)), ((50.0, 50.0), (0.0, 1.0)));
    }
}
