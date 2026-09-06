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

use wayland_client::QueueHandle;

use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::{ElementState, KeyEvent, MouseButton, MouseScrollDelta};

mod items;

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
    /// The context menu closed on "remove". Carries the path rather than an
    /// index: the menu is modal on its own thread, and the list can be
    /// reordered by a drag (or grown by a drop) while it is open.
    RemoveItem(std::path::PathBuf),
}

/// The world region the current buffer must cover, as told by the
/// compositor: virtual origin/size and surface px per virtual unit.
#[derive(Debug, Clone, Copy)]
struct Patch {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    scale: f64,
}

struct GridApp {
    patch: Option<Patch>,
    /// Images pinned to the canvas, paired with their uploaded texture id
    /// (`None` until the renderer exists — see `renderer_init`).
    items: Vec<(items::DesktopItem, Option<u32>)>,
    /// Worker threads post finished drops back through this.
    sender: calloop::channel::Sender<Message>,
    /// The item being dragged, and where inside it the pointer grabbed —
    /// held in VIRTUAL units so the drag survives a pan or zoom mid-gesture.
    dragging: Option<Drag>,
    /// The raw `(relief)` string currently installed process-wide (depth +
    /// wall profile LUT) — a change detector, so the registry is only
    /// touched when the config value actually changes.
    applied_relief: Option<String>,
    /// The DE-wide `bevel_depth` captured before the first spec override,
    /// restored if the key later reverts to a plain width or is removed.
    base_depth: Option<f32>,
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
                if let Some(d) = spec.depth {
                    if let Ok(mut reg) = cce_ui::layout::get_style_registry().write() {
                        reg.set_float("bevel_depth", d);
                    }
                }
                cce_ui::layout::install_wall_profile_spec(spec.profile.as_deref());
                self.applied_relief = Some(raw.clone());
            }
            _ if self.applied_relief.is_some() => {
                // The key reverted to a plain width or vanished: back to
                // the DE-wide material the registry still carries.
                if let Some(d) = self.base_depth.take() {
                    if let Ok(mut reg) = cce_ui::layout::get_style_registry().write() {
                        reg.set_float("bevel_depth", d);
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
            Rect { x: 0.0, y: 0.0, width: size.width as f32, height: size.height as f32 },
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
        let ring_radius = radius + roll as f32;
        let ring_depth = (roll as f32).max(1.0);

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
                if lip {
                    let ring = Rect {
                        x: rect.x - roll as f32,
                        y: rect.y - roll as f32,
                        width: rect.width + 2.0 * roll as f32,
                        height: rect.height + 2.0 * roll as f32,
                    };
                    pc.recess(
                        ring,
                        (ring_radius, ring_radius, ring_radius, ring_radius),
                        ring_depth,
                    );
                }
            }
        }

        // Pinned images sit ON the canvas, so they are placed by the same
        // world->patch mapping as the cells and drawn after them. The whole
        // grid surface is below every window, so an item never covers an app.
        for (item, id) in self.items.iter() {
            let Some(id) = *id else { continue };
            let rect = Rect {
                x: ((item.x - p.x) * s) as f32,
                y: ((item.y - p.y) * s) as f32,
                width: (item.w * s) as f32,
                height: (item.h * s) as f32,
            };
            // Cull off-patch items: at a far zoom-out the patch can hold
            // hundreds of squares, and an image that is not on it costs a
            // draw for nothing.
            if rect.x + rect.width < 0.0
                || rect.y + rect.height < 0.0
                || rect.x > size.width as f32
                || rect.y > size.height as f32
            {
                continue;
            }
            pc.image(id, rect, 1.0);
        }
    }
}

impl Application for GridApp {
    type Message = Message;

    fn new(
        _qh: &QueueHandle<EngineState<Self>>,
        _sender: calloop::channel::Sender<Self::Message>,
    ) -> Self {
        Self {
            patch: None,
            items: items::load().into_iter().map(|i| (i, None)).collect(),
            sender: _sender,
            dragging: None,
            applied_relief: None,
            base_depth: None,
        }
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
        match msg {
            Message::ItemReady { item, pixels, px_w, px_h } => {
                let id = cce_ui::vk::upload_rgba(pixels, px_w, px_h);
                log::info!(
                    "[items] pinned {} at ({:.0}, {:.0})",
                    item.path.display(),
                    item.x,
                    item.y
                );
                self.items.push((item, Some(id)));
                // Persist only the model — the texture id is per-process.
                let model: Vec<items::DesktopItem> =
                    self.items.iter().map(|(i, _)| i.clone()).collect();
                items::save(&model);
                *needs_rebuild = true;
            }
            Message::RemoveItem(path) => {
                let Some(pos) = self.items.iter().position(|(i, _)| i.path == path) else {
                    return;
                };
                // A drag on the removed item cannot outlive it.
                if self.dragging.is_some() {
                    self.dragging = None;
                }
                let (item, id) = self.items.remove(pos);
                if let Some(id) = id {
                    cce_ui::vk::free_image(id);
                }
                let model: Vec<items::DesktopItem> =
                    self.items.iter().map(|(i, _)| i.clone()).collect();
                items::save(&model);
                // The file itself stays where it was saved: this unpins the
                // image from the desktop, it does not delete the user's file.
                log::info!("[items] removed {} from the desktop", item.path.display());
                *needs_rebuild = true;
            }
        }
    }

    fn tick(&mut self, _dt: f32, _needs_rebuild: &mut bool) {}

    /// Uploads happen here, not in `new`: a reconnect builds a fresh renderer
    /// and does not replay earlier uploads, so items restored from the sidecar
    /// (and any pinned before the reconnect) have to be handed over again.
    fn renderer_init(&mut self, _renderer: &mut cce_ui::vk::VkRenderer) {
        for (item, id) in self.items.iter_mut() {
            let Ok(bytes) = std::fs::read(&item.path) else {
                log::warn!("[items] {} is gone; not drawing it", item.path.display());
                *id = None;
                continue;
            };
            match items::decode_rgba(&bytes) {
                Some((pixels, w, h)) => *id = Some(cce_ui::vk::upload_rgba(pixels, w, h)),
                None => {
                    log::warn!("[items] {} did not decode", item.path.display());
                    *id = None;
                }
            }
        }
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
        let Some(payload) = items::parse_payload(mime, data) else {
            items::report_failure(&format!("nothing usable in the dropped {mime}"));
            return;
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
        std::thread::spawn(move || {
            let (bytes, name) = match items::fetch(payload) {
                Ok(v) => v,
                Err(e) => {
                    items::report_failure(&format!("could not fetch it: {e}"));
                    return;
                }
            };
            let Some((pixels, px_w, px_h)) = items::decode_rgba(&bytes) else {
                items::report_failure(&format!(
                    "the dropped {mime} is not an image cce can read ({} bytes)",
                    bytes.len()
                ));
                return;
            };
            // Save even though it is already decoded: the user asked for the
            // file on their desktop, not just a picture on the canvas.
            let path = match items::save_to_desktop(&bytes, &name) {
                Ok(p) => p,
                Err(e) => {
                    items::report_failure(&format!("could not save it to the desktop: {e}"));
                    return;
                }
            };
            let fit = (cell_w / px_w as f64).min(cell_h / px_h as f64).min(1.0);
            let w = px_w as f64 * fit;
            let h = px_h as f64 * fit;
            let item = items::DesktopItem {
                path,
                // Centred on the drop point.
                x: vx - w / 2.0,
                y: vy - h / 2.0,
                w,
                h,
            };
            let _ = sender.send(Message::ItemReady { item, pixels, px_w, px_h });
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
        let Some(drag) = self.dragging.as_mut() else { return };
        let Some(p) = self.patch else { return };
        if p.scale <= 0.0 {
            return;
        }
        let origin = (p.x, p.y);
        let last_pos = drag.last_pos;
        drag.last_pos = (pos.x, pos.y);
        if drag.last_origin != origin {
            // The surface moved under the pointer; this position cannot be
            // compared with the previous one. Re-baseline and wait.
            drag.last_origin = origin;
            return;
        }
        let s = p.surface_per_virtual();
        let dx = (pos.x - last_pos.0) as f64 / s;
        let dy = (pos.y - last_pos.1) as f64 / s;
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        drag.moved = true;
        let index = drag.index;
        if let Some((item, _)) = self.items.get_mut(index) {
            item.x += dx;
            item.y += dy;
            *needs_rebuild = true;
        }
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        let Some(p) = self.patch else { return None };
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
            let hit = self.items.iter().rposition(|(i, _)| {
                vx >= i.x && vx < i.x + i.w && vy >= i.y && vy < i.y + i.h
            })?;
            let path = self.items[hit].0.path.clone();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Image".to_string());
            // The menu blocks until it is dismissed, so it cannot run on the
            // loop that has to keep drawing the desktop behind it.
            let sender = self.sender.clone();
            std::thread::spawn(move || {
                if items::item_menu(&name).as_deref() == Some("remove") {
                    let _ = sender.send(Message::RemoveItem(path));
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
                // Last drawn is on top, so search backwards and take the
                // first hit.
                let hit = self.items.iter().rposition(|(i, _)| {
                    vx >= i.x && vx < i.x + i.w && vy >= i.y && vy < i.y + i.h
                })?;
                // Raise it: the one you grabbed should be the one you see,
                // and the next press should find it first.
                let item = self.items.remove(hit);
                self.items.push(item);
                self.dragging = Some(Drag {
                    index: self.items.len() - 1,
                    last_pos: (pos.x, pos.y),
                    last_origin: (p.x, p.y),
                    moved: false,
                });
                *needs_rebuild = true;
            }
            ElementState::Released => {
                if let Some(drag) = self.dragging.take() {
                    if drag.moved {
                        let model: Vec<items::DesktopItem> =
                            self.items.iter().map(|(i, _)| i.clone()).collect();
                        items::save(&model);
                        if let Some((item, _)) = self.items.get(drag.index) {
                            log::info!(
                                "[items] moved {} to ({:.0}, {:.0})",
                                item.path.display(),
                                item.x,
                                item.y
                            );
                        }
                    }
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

    fn display_list(&mut self, size: LogicalSize, _scale: f64) -> Option<DisplayList> {
        let mut pc = PaintCtx::new();
        self.paint(&mut pc, size);
        Some(pc.finish())
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
