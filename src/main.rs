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
//! rounded cells on the gap-colored rail surface — deliberately unlit, so
//! the grid reads as ground under the windows' own relief.

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
}

/// Style knobs, re-read per frame from the shared config (cheap: cce-ui
/// caches the parse on mtime), with the same defaults the compositor uses.
struct Style {
    cell_size: f64,
    gap_width: f64,
    cell_inset: f64,
    corner_radius: f64,
    gap_color: [f32; 4],
    cell_color: [f32; 4],
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
        cell_size: get_i64("/style/surface/desktop/grid_cell_size", 512) as f64,
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
    }
}

/// Never emit more cells than this per frame, whatever the patch/config says
/// (a degenerate period must not turn into an unbounded display list).
const MAX_CELLS: usize = 8192;

impl GridApp {
    fn paint(&self, pc: &mut PaintCtx, size: LogicalSize) {
        let Some(p) = self.patch else { return };
        if p.scale <= 0.0 {
            return;
        }
        let st = style();
        let period = st.cell_size + st.gap_width;
        if period < 1.0 {
            return;
        }

        // The rail surface: the whole patch in gap color.
        pc.quad(
            Rect { x: 0.0, y: 0.0, width: size.width as f32, height: size.height as f32 },
            st.gap_color,
        );

        // Visible cell box within its period slot, in virtual units.
        let inset = st.cell_inset.clamp(0.0, st.cell_size / 2.0 - 1.0);
        let lo = inset;
        let len = st.cell_size - 2.0 * inset;
        let s = p.scale;
        // Span-widened like every other corner in the DE (window clips,
        // fallback cells, cce-ui plates): at corner_shape > 2 a raw-radius
        // superellipse hugs the corner and reads nearly square, and a tiled
        // window's widened arc must land exactly on its cell's. Clamped to a
        // quarter sweep like the compositor's widen_corner_radius.
        let cell_px = len * s;
        let radius = ((st.corner_radius * s)
            * cce_ui::layout::corner_span_factor() as f64)
            .min(cell_px / 2.0) as f32;

        let col0 = (p.x / period).floor() as i64;
        let col1 = ((p.x + p.w) / period).ceil() as i64;
        let row0 = (p.y / period).floor() as i64;
        let row1 = ((p.y + p.h) / period).ceil() as i64;
        let mut cells = 0usize;
        for col in col0..col1 {
            for row in row0..row1 {
                if cells >= MAX_CELLS {
                    return;
                }
                cells += 1;
                let vx = col as f64 * period + lo;
                let vy = row as f64 * period + lo;
                let rect = Rect {
                    x: ((vx - p.x) * s) as f32,
                    y: ((vy - p.y) * s) as f32,
                    width: (len * s) as f32,
                    height: (len * s) as f32,
                };
                // A flat cell — no relief: the grid is ground, not furniture,
                // and a lit well under every window fought the windows' own
                // bevels.
                pc.rounded_rect(rect, radius, (true, true, true, true), st.cell_color);
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
        Self { patch: None }
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
