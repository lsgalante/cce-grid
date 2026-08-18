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

Rendering is a pure function of (patch, style config): flat rounded cells
on the gap-colored rail surface — deliberately NO relief, so the grid reads
as ground under the windows' own bevels (the compositor fallback is flat
for the same reason: the client latch must not swap the grid's material).
It reads the same `style.surface.desktop.*` / backplate-radius keys as the
fallback. Keep it that way — no camera state, no timers, no input (the
surface is input-transparent compositor-side).

This directory is its own git repository (gitsite-published, fetch-only
origin; committing locally is publishing). `cce-grid.service` autostarts it
with the session (WantedBy=cce-session.target); ccebuild installs both.

Corner radii follow the DE-wide convention: the caller widens the nominal
radius by `cce_ui::layout::corner_span_factor()` (superellipse span
compensation) before handing it to the primitives, clamped to a quarter
sweep — same as the compositor's `widen_corner_radius`. An unwidened radius
reads nearly square at corner_shape > 2 and misses the window corners.
