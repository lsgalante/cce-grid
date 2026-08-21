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
//! reads (`style.surface.desktop.*`, backplate corner radius): flat
//! rounded cells, with the relief on the LINES — the gap rails read as
//! raised grout (per-cell half-gap-expanded `Recess` rings that abut at
//! the rail centerlines), while every cell floor stays flat.

use wayland_client::QueueHandle;

use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::{ElementState, KeyEvent, MouseButton, MouseScrollDelta};

#[derive(Debug, Clone)]
enum Message {}

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
    /// The raw `(relief)` string currently installed process-wide (depth +
    /// wall profile LUT) — a change detector, so the registry is only
    /// touched when the config value actually changes.
    applied_relief: Option<String>,
    /// The DE-wide `bevel_depth` captured before the first spec override,
    /// restored if the key later reverts to a plain width or is removed.
    base_depth: Option<f32>,
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
        corner_radius: get_i64("/style/surface/backplate/corner_radius", 12) as f64,
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
        // is the BACKPLATE-EDGE treatment — `layout::bevel_width` clamped to
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
        // backplate clamp.
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
    }
}

impl Application for GridApp {
    type Message = Message;

    fn new(
        _qh: &QueueHandle<EngineState<Self>>,
        _sender: calloop::channel::Sender<Self::Message>,
    ) -> Self {
        Self { patch: None, applied_relief: None, base_depth: None }
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

    fn update(&mut self, _msg: Self::Message, _needs_rebuild: &mut bool, _exit: &mut bool) {}

    fn tick(&mut self, _dt: f32, _needs_rebuild: &mut bool) {}

    // The grid layer is input-transparent compositor-side; nothing ever
    // reaches these.
    fn handle_pointer_move(&mut self, _pos: LogicalPosition, _needs_rebuild: &mut bool) {}

    fn handle_mouse_input(
        &mut self,
        _button: MouseButton,
        _state: ElementState,
        _pos: LogicalPosition,
        _needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
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
    env_logger::init();
    cce_ui::engine::run::<GridApp>();
}
