# CLAUDE.md

`cce-grid` is the desktop-grid client of the cce desktop: a cce-ui app the
compositor world-anchors to the virtual desktop. It renders grid *patches* —
virtual-rect regions at a compositor-chosen resolution — and is never in the
pan/zoom loop: the compositor transforms the committed buffer per frame like
any window content.

The contract (cce window-management protocol, manager v6 / toplevel v4):
`Application::grid() -> true` declares the role; `grid_patch` events say what
to render; cce-ui's runner resizes, forwards to `Application::grid_patch`,
and acks so the next commit latches at the new anchor. The compositor keeps
its own rect grid as the fallback whenever this client is absent or has not
latched a patch yet, and its gap-colored backdrop always draws beneath as
the safety net beyond patch edges.

Rendering is a pure function of (patch, style config): flat rounded cells,
with the relief on the LINES — per-cell `Recess` rings expanded by the
half-gap, so the walls occupy exactly the half-rail around each cell
(crest at the rail centerline, inner edge concentric with the cell arc)
and neighboring rings abut without double-shading. Cell floors carry NO
relief (user decision); the rails read as raised grout. The compositor
fallback draws the same expanded-ring chamfer so the client latch never
swaps the grid's material. `style.surface.desktop.line_relief` overrides
the lip for the grid alone: a plain integer is the lip width in logical
px (0 = no lip; unset = follow the DE-wide relief material), and a
`(relief)` value carries a full custom material — width, depth, and wall
profile (`cce_ui::relief_spec::ReliefSpec`), installed process-wide by
this client (the grid and the desktop items below are all it draws) and
edited in place with
`cce-relief --key style.surface.desktop.line_relief`. The compositor
fallback honors the integer form and a `(relief)` value's width (its
scenefx chamfer has no custom profile to install). It reads the same `style.surface.desktop.*` /
backplate-radius keys as the fallback. Keep it that way — no camera
state, no timers (`tick` is empty). Input is the one exception, and only
over the desktop items below; the surface is transparent to the pointer
everywhere else.

## Desktop items (`src/items.rs`)

Images pinned to the world canvas. The compositor routes a drag over the
desktop background onto this client (its `Scene::at_including_grid`), so a
drop arrives at `handle_drop`; `drop_mimes()` declares the accepted flavors in
preference order. Pixels win whenever they are offered (`image/png`,
`image/jpeg`, `image/gif`, `image/webp`) — no fetch, no ambiguity. Below them
`text/html` is preferred over `text/uri-list` because it names the IMAGE: a
thumbnail wrapped in a link (Google Images' exact markup) puts the result page
in uri-list, and fetching that yields HTML rather than a picture. For an
unwrapped image the two agree, so the preference never does worse.

A dropped item is saved into the desktop folder (`$XDG_DESKTOP_DIR` when
user-dirs exports one, else `~/Desktop`) AND recorded in a sidecar,
`$XDG_DATA_HOME/cce/desktop-items.json`, with the VIRTUAL-canvas position it
landed at — so it comes back in the same world spot next session. Fetching
shells out to `curl` rather than linking an HTTP stack: this process is a
background renderer that otherwise needs no network at all, and for a
once-in-a-while user action an async runtime plus a TLS stack would be the
largest thing in the binary.

Items draw as GPU-textured quads. Decode happens on a worker thread and the
pixels return through `Message::ItemReady`, because the upload
(`cce_ui::vk::upload_rgba`) has to happen on the main loop — which is also why
`renderer_init` re-uploads everything restored from the sidecar.

They are the only reason this client takes input at all. `input_regions()`
returns exactly the item rects (and an empty list when there is no patch), so
the pointer passes straight through everywhere else. Over an item, left-drag
moves it — the grab offset is held in VIRTUAL units, so the gesture survives a
pan or zoom mid-drag — and right-click opens a one-button `cce-cloud --json`
popup at `ccectl pointer-location`, whose reply comes back as
`Message::RemoveItem(path)`. It carries the path rather than an index because
that menu blocks on its own thread, and the list can be reordered by a drag or
grown by a drop while it is open.

One trap, spelled out on `Patch::surface_per_virtual`: pointer events and
input regions are surface-local px, and for THIS surface that means BUFFER
px at every output scale — the grid surface is pinned at buffer_scale 1
(cce-ui ignores scale events for grid apps; patch.scale is the sole
resolution authority), so `Patch::scale` is the one conversion for paint,
regions, and pointer math alike. This replaced a `/ui` division that had
been calibrated against the compositor's old hit-test, which handed out raw
layout offsets: numerically buffer/ui only at camera zoom 1 on the pow2
patch quantization, and at any other camera state it displaced the input
region off the items (presses read as background — in overview they EXITED
it) and tore the press position apart from the drag deltas, flinging the
grabbed item thousands of virtual units. The compositor's hit-test speaks
true surface coordinates since cce-compositor@feab593; do not reintroduce
output-scale terms here.

This directory is its own git repository (gitsite-published, fetch-only
origin; committing locally is publishing). `cce-grid.service` autostarts it
with the session (WantedBy=cce-session.target); ccebuild installs both.

Corner radii follow the DE-wide convention: the caller widens the nominal
radius by `cce_ui::layout::corner_span_factor()` (superellipse span
compensation) before handing it to the primitives, clamped to a quarter
sweep — same as the compositor's `widen_corner_radius`. An unwidened radius
reads nearly square at corner_shape > 2 and misses the window corners.
