# CLAUDE.md — working notes for zdb

Guidance for future Claude sessions working in this repo. Keep it current.

## What zdb is

A fast, Zed-style desktop PostgreSQL client built in Rust on **gpui** (Zed's UI
framework). Single active connection at a time; inline-editable result grids;
SQL editor; embedded terminal; command palette; light/dark theme.

## Workspace layout

```
crates/
  zdb-db/      data layer: async DbHandle (own Tokio runtime thread + channels),
               streaming query exec, schema introspection, inline-edit engine
               (edit.rs), query editability describe (actor.rs run_describe),
               rustls(ring) TLS. Auto-reconnect: worker's `ensure_conn` redials
               (same ConnId) when the client is closed; a query whose SUBMIT hit
               a silently-dead socket retries once (safe — nothing reached the
               server; mid-stream failures never retry). 10s connect_timeout +
               120s keepalive in config.rs.
  zdb-config/  JSON settings (connections, theme, keymap) + OS-keychain passwords
               (secret.rs, keyring; per-OS backend).
  zdb-app/     gpui app (bin `zdb`): workspace/ (the UI — ONE `Workspace` gpui
               entity, split by concern into child modules; each file holds an
               `impl Workspace` block + its own tests):
                 mod.rs     struct def + new(), actions!, palette/terminal/
                            selftest, and `spawn_db` (the one async-result
                            helper every DB call funnels through)
                 conn.rs    connect/switch/save + `ConnForm` (add-form inputs)
                 tabs.rs    tab accessors/lifecycle + table_query/apply_where
                 tree.rs    schema sidebar: `SchemaTree` (all tree state, incl.
                            reset()), model types, pure build/sync fns, loaders
                 query.rs   run/explain/sort/format, `QueryLog`, `drain_query`
                 session.rs per-connection saved tabs: snapshot/restore/debounced
                            save + the `store` disk shim (in-memory under test)
                 edit.rs    inline-edit staging/apply, row add/delete
                 autoscroll.rs  middle-click (wheel-press) autoscroll of the grid
                 keynav.rs  keyboard navigation: cell cursor, tree keys, pane
                            focus, tab keys + `bind_nav_keys` (ALL nav bindings)
                 modal.rs   dialogs: one-at-a-time `Modal` state, focus trap,
                            the shared occluding backdrop (`modal_layer`)
                 export.rs  CSV/JSON/INSERTs/TSV export
                 view.rs    all render_* + impl Render
                 grid.rs    Tab/TabKind + ResultDelegate
                 colors.rs / util.rs   palette / pure SQL+config helpers
                 test_support.rs       shared #[cfg(test)] fixtures
               Child modules see Workspace's private fields (defined in the
               parent module) — methods moved out of mod.rs are `pub(super)`
               so siblings/tests can call them. No visibility bumps beyond that.
               Also terminal.rs, main.rs (entry, WSL X11 guard, keybindings, theme).
```

The UI talks to the DB only through `DbHandle` channels; it never touches Tokio
(`zdb_db::QueryStream` names the query-event receiver so the UI needn't).

## Build / run / test

- Source the env every shell: `. "$HOME/.cargo/env"` (cargo not on PATH otherwise).
- Build: `cargo build -p zdb-app`. Run: `./target/debug/zdb`.
- On WSL the app forces the X11 backend (gpui Wayland panics under WSLg) — handled
  in `main.rs::is_wsl`.
- Tests: `cargo test`. Postgres integration tests are gated on env:
  `ZDB_TEST_HOST=127.0.0.1 ZDB_TEST_USER=zdb ZDB_TEST_DB=zdb ZDB_TEST_PASSWORD=zdb cargo test`.
  A native Postgres 16 runs locally (no docker): `sudo pg_ctlcluster 16 main start`
  (role/db/pw all `zdb`).
- gpui UI tests use `#[gpui::test]` and need gpui's `test-support` dev-feature
  (already set in zdb-app `[dev-dependencies]`).
- Run dev with a connection via env: `ZDB_HOST/USER/DB/PASSWORD`, `ZDB_SSL_DISABLE=1`.
  `ZDB_LOG=1` prints milestones; `ZDB_SELFTEST=1` runs a demo query after connect.

## Windows builds

Target is **Windows on ARM64** (the dev box is WSL2 aarch64; the host is ARM64).

- **Build natively on Windows only** (locally with MSVC tools, or CI:
  `.github/workflows/build.yml` builds every OS × arch on native runners).
  gpui's build script precompiles its HLSL shaders with `fxc` (Windows SDK) and
  embeds them — but only when building **on** Windows. Cross-compiling from
  Linux leaves gpui without shaders → `Error creating DirectWriteTextSystem`
  on the user's machine. (We used to work around this with a vendored/patched
  gpui + `debug-assertions = true` + a bundled `shaders/` folder; that whole
  path was removed — see BUILDING-WINDOWS.md.)
- Manifest: `crates/zdb-app/build.rs` (embed-resource) compiles `resources/zdb.rc`
  embedding `zdb.manifest` (Common-Controls v6 → fixes `TaskDialogIndirect`) and
  `zdb.ico` (app icon). GUI subsystem via `#![cfg_attr(target_os="windows", ...)]`.
  MSVC's `link.exe` also auto-embeds a default manifest which collides with ours
  → `/MANIFEST:NO` in `.cargo/config.toml` for the msvc targets.
- CI's Package step drops `zdb.exe` + `sqls.exe` into the artifact; no extra
  runtime files needed.

## macOS builds

**Always ship the `.app` bundle, never the bare `zdb` executable.** Finder cannot
launch a Unix executable — double-clicking one makes it open **Terminal** and run
the app from there, and it shows the generic exec icon (no `CFBundleIconFile`).
That was the whole "it starts a terminal first / has no icon" symptom.

- `scripts/package-macos.sh [arch]` assembles `dist/zdb.app` (must run ON a Mac —
  no macOS SDK on the Linux dev box). Env: `ZDB_SKIP_BUILD=1` reuses an existing
  release binary (CI already built one into `target/release`), `ZDB_ZIP=1` also
  writes `dist/zdb-macos-<arch>.zip` via `ditto`.
- The script ad-hoc **codesigns** the bundle (`codesign --force --deep --sign -`):
  Apple Silicon refuses unsigned code, and adding `sqls`/Info.plist invalidates the
  linker's own ad-hoc signature on the bare binary. It then `lsregister -f`s the
  path — macOS caches bundle icons **per path** and otherwise keeps showing the old
  or generic one (same class of trap as the Windows Explorer icon cache).
- Icon: `scripts/gen-icon.py` emits BOTH `resources/zdb.ico` and `resources/zdb.icns`.
  The `.icns` is written by a small pure-Python packer (`icns` header + `type/len/PNG`
  entries) because Pillow's own ICNS *writer* shells out to macOS-only `iconutil`,
  and we generate here on Linux. Art is inset to 824/1024 (Apple's icon grid) or it
  looks oversized in the Dock.
- CI (`build.yml` Package step) branches on `goos == darwin`: runs the script and
  uploads `zdb.app.zip`. Upload-artifact does NOT preserve the exec bit or bundle
  layout, so the inner `ditto` zip is required, not cosmetic.
- Distributing it: ad-hoc signing ≠ notarized, so a downloaded copy needs
  `xattr -dr com.apple.quarantine /Applications/zdb.app` once (documented in README).

## Testing Windows builds HERE (WSL interop)

The WSL host can run Windows exes directly:
- Copy build to a Windows path: `cp ... /mnt/c/zdbtest/` (interop dislikes UNC cwd).
- Run + capture stderr (panics show even for GUI subsystem):
  `cmd.exe /c "C:\\zdbtest\\zdb.exe & echo EXITCODE=%errorlevel%"`.
- Screenshot a specific window (works when occluded) with PowerShell + Win32
  `PrintWindow(hwnd, hdc, 2)`. This is how UI changes are verified visually.
- **DPI SCREENSHOT TRAP (cost hours once):** the host is 125% DPI; the window is
  ~1536×864 *logical* = ~1920×1080 *physical*. A DPI-**unaware** PowerShell calls
  `GetWindowRect` and gets VIRTUALIZED (logical-ish) coords, makes an undersized
  bitmap, and `PrintWindow` then captures only the LEFT ~80% of the window — the
  right edge (e.g. title-bar window controls) is silently cropped out, looking
  like it "didn't render". FIX: call `[shcore]::SetProcessDpiAwareness(2)` at the
  top of the capture script so `GetWindowRect` returns full physical size and the
  bitmap covers the whole window. Don't debug "missing" right-edge UI until the
  capture is DPI-aware. (`/mnt/c/zdbtest/capdpi.ps1` is the DPI-aware version.)
- Windows settings path: `%APPDATA%\zdb\config\settings.json` (directories crate
  adds the `config` subdir). Keychain = Windows Credential Manager (service `zdb`).

## Commits

- **Title only. No body, no description, no bullet list of changes.**
- **NEVER add a `Co-Authored-By:` trailer** (no Claude co-author line, ever).

## Gotchas

- Background-command "exit 0" from `… | tee | tail` is the tail's exit — always
  grep the log for `error:` to confirm a build actually passed.
- Driving the app on WSL X11: stop it with `pkill -x zdb` — `pkill -f target/debug/zdb`
  matches (and kills) the shell running the command. `import -window` silently crops a
  window that extends past the screen (bottom-panel UI "missing"): `xdotool windowmove`
  it on-screen first. Synthetic hover (tooltips, diagnostic popovers) only registers
  after a synthetic click in the window. Running the app writes the REAL
  `~/.config/zdb` (env connection's tabs → `tabs/env.json`, scratch → `scratch/env.sql`,
  settings toggled in the UI) — clean up after testing.
- **Custom (Zed-style) title bar** lives in `workspace.rs::render_titlebar` (not
  the gpui-component `TitleBar`). Window opens borderless via
  `gpui_component::TitleBar::title_bar_options()` in `main.rs` (`appears_transparent`
  → gpui sets `hide_title_bar`, killing the native frame on Win/Linux).
  Key lessons baked in:
  - We DON'T use gpui-component's `TitleBar` widget: its min/max/close glyphs take
    color from the ambient `window.text_style()` (no explicit color), which an
    ancestor `.text_color` does NOT reach → they render invisible. Our buttons set
    an explicit `.text_color` on each `Icon::empty().path(...)` (window-{minimize,
    maximize,restore,close}.svg, added to `assets.rs`).
  - Window controls work natively on Windows via `.window_control_area(Min/Max/
    Close/Drag)` hit-test hints (events.rs maps them to HTMINBUTTON/HTCLOSE/
    HTCAPTION); Linux gets `on_click` → `window.{minimize,zoom,remove}_window()`.
  - **The `Drag` area must NOT wrap the control buttons** (cost a debugging round).
    gpui's `on_hit_test_window_control` callback (window.rs) returns the FIRST
    `window_control_hitbox` under the cursor *in paint order*, and a parent paints
    before its children. So tagging the whole title bar `Drag` makes EVERY button
    resolve to `Drag` (the bar wins) → min/max/close clicks do nothing (you can
    only drag). FIX (mirrors gpui-component's own `TitleBar`): put `Drag` on a
    `flex_1` wrapper around the LEFT content only; keep `controls` a sibling
    OUTSIDE any drag area. The flex_1 drag region also pushes controls to the
    right edge (no `justify_between` needed). Symptom is invisible in a screenshot
    (buttons render fine); verify by actually clicking — `/mnt/c/zdbtest/ctl.ps1`
    clicks a control then reports whether the window closed/minimized/resized.
  - The bar width is set EXPLICITLY to `window.viewport_size().width`. `w_full` +
    `justify_between` FAILS: the results table's min-content width inflates the
    column's *layout* width past the window, so `justify_between`/`flex-grow`/
    `absolute right_0` distribute over that inflated width and shove the controls
    off-screen right (the bar still *paints* full-width, so it's invisible unless
    you DPI-correctly screenshot the right edge — see the DPI trap above). A
    definite width pins the right edge. Also gave `content` + its main wrapper
    `min_w(0)`+`overflow_hidden` so the table can't inflate the column.
- gpui-component element is `Input` (not TextInput); `.primary()/.danger()` need
  `ButtonVariants`; `cx.theme()` needs `ActiveTheme`; weak self-handle =
  `cx.weak_entity()`.
- Inline editing couples the table delegate to the workspace: `ResultDelegate`
  holds `WeakEntity<Workspace>`, reads editing state in `render_td`, and calls back
  via `weak.update(...)`. Edits accumulate in `pending: Vec<RowEdit>` (optimistically
  shown in the grid); the review pane renders the combined `build_batch` SQL
  (`BEGIN; … COMMIT;`); Apply runs the whole batch in one transaction (`apply_edits`),
  Cancel discards and reloads. Edits survive a failed Apply so they can be retried.
- gpui-component `Input` element defaults to CONTENT height (one line). For a
  full-height multi-line editor call `Input::new(state).h_full()` — the wrapping
  `div().size_full()`/`flex_1` is not enough on its own.
- App icon: regen with `python3 scripts/gen-icon.py` (black rounded square + white DB
  cylinder, no letter) → `resources/zdb.ico`, embedded via build.rs. build.rs has
  `rerun-if-changed` on the .ico so it re-embeds. Windows Explorer may still show the
  OLD icon from its per-path icon cache even when the taskbar/window icon is correct —
  that's host-side staleness (clear with `ie4uinit.exe -show` / delete iconcache), not
  a build problem.
- **Window geometry is persisted** (`Settings.window: Option<WindowState>` in zdb-config;
  `zdb_config::save_window_state` re-reads the file before writing so it never clobbers
  other settings). `main.rs` turns it into `WindowOptions.window_bounds`
  (`Maximized` when the flag is set — gpui stores the RESTORE bounds in that variant),
  dropping bounds that intersect no current display (unplugged monitor → off-screen window).
  Saving has two paths because neither alone is enough: `Workspace::new` registers
  `window.on_window_should_close` (Windows close button / Alt+F4 → WM_CLOSE) and the Linux
  title-bar close button calls `save_window_geometry` before `remove_window()` (that call
  bypasses the OS close path). PLUS `render` calls `track_window_geometry`, which debounces
  a write 500ms after the geometry stops changing — resize renders every frame, and a hard
  exit never reaches any close hook. The disk write is `#[cfg(not(test))]` (tests would
  otherwise stomp the dev machine's real settings.json).
  Verifying on WSL X11: `xdotool windowsize/windowmove` DO work, but synthetic
  `xdotool click`/`key alt+F4` are ignored by the app — don't try to test close paths by
  clicking here; resize + read `~/.config/zdb/settings.json` instead.
- Resizable panels: gpui-component seeds any `resizable_panel()` WITHOUT an explicit
  `.size()` to `PANEL_MIN_SIZE` (state.rs `sync_panels_count`), then ratio-scales to
  the container. A group where the first panel had no size squeezed the last panel
  off-screen (the bottom query-log / review pane was clipped below the window). Give
  EVERY panel an explicit `.size()` (the ratios are scaled to fit) — don't leave one
  unsized expecting it to flex-fill.
- **Wide results must be able to scroll horizontally**: a flex item's automatic
  minimum size is its MIN-CONTENT width, and for the grid that's the sum of every
  column width (180px × N). Every ancestor — including gpui-component's
  `resizable_panel` (`flex_grow` + `size_full`, no min-width of its own) — then grows
  to that width, so the Table element believes it fits, its built-in horizontal
  scrollbar never engages, and the off-window columns are simply clipped by the root
  `overflow_hidden` (the "can't scroll right on wide tables" bug). FIX: `min_w(px(0.))`
  + `overflow_hidden` on the WHOLE chain down to the Table — `render_center`'s root,
  `render_tab_body`'s root, the `results` v_flex, and the div wrapping
  `Table::new(...)`. One missing link re-breaks it. Same clamp is what keeps the
  title-bar controls on-screen (see the title-bar notes above). Not unit-testable
  (layout lives in gpui's render); verify by running with a 25-column table and
  wheel-scrolling — vertical scroll is unaffected by the clamp.
  The clamp alone still LOOKED broken on Windows: gpui-component defaults to
  `ScrollbarShow::Scrolling`, so the bar appears only *while* scrolling and fades
  after 2s — and horizontal scroll is shift+wheel (gpui's Windows `WM_MOUSEWHEEL`
  handler maps shift→`delta.x`) or a tilt wheel, neither of which anyone tries
  without a visible bar. `main.rs` therefore sets
  `Theme::global_mut(cx).scrollbar_show = ScrollbarShow::Always` right after
  `Theme::change` (`Theme::change` only rewrites colors, so the setting survives
  the settings modal's live light/dark switch).
- `text_color` on an icon: `Icon::empty().path("icons/<name>.svg").text_color(rgba(..))`
  tints by alpha mask (gpui rasterizes the SVG → coverage → fills with the color), so
  Lucide stroke OR solid-fill SVGs both work. Don't tint an icon the same color as a
  `.danger()`/`.primary()` button background (red-on-red = invisible); use a neutral
  button bg when the glyph itself carries the color (e.g. green play, red minus).
- Icons: gpui-component ships NO SVG files; `IconName` resolves to `icons/<name>.svg`
  loaded via the app's `AssetSource`. Without one, icon buttons render as blank
  squares. We embed the (Lucide, MIT) SVGs we use in `crates/zdb-app/assets/icons/`
  via `include_bytes!` in `src/assets.rs` and register `.with_assets(Assets)` in
  `main.rs`. Add a new `IconName` → add its SVG there too. (App `.ico` is separate:
  a Win32 resource via build.rs, unrelated to gpui icon rendering.)
- **Seamless inline cell editor** (`render_td`): `Input::new(state).appearance(false)`
  removes the border/background but STILL applies `input_px/py/h` from the input's
  `Size` (Medium → 12px horizontal padding + a min-height), so the edit text shifted
  right + down vs the static `td_text` (which is `px_2` = 8px). FIX: `.small()` sets
  `input_px` to 8px (== `px_2`) and a tiny min-height; wrap with NO extra padding or
  border (those re-introduce the shift), just a subtle `bg(c.active)` to signal edit mode.
  **Cell highlights must fill the CELL, and the table pads the td itself.** The widget's
  `render_cell` (state.rs) applies `table_cell_size(size)` → `table_cell_padding()`
  (Medium = 8px h / 4px v) to the box that wraps our `render_td` element, so a `size_full()`
  highlight paints INSIDE that padding and reads visibly smaller than the cell (short of
  the row height, narrow of the column). FIX: build columns with `Column::p_0()` (a
  per-column `paddings` overrides the size padding) and re-add the padding in OUR elements
  — `td_text` and `render_th` are `size_full().flex().items_center().px_2()`, which also
  vertically centers text in the 32px row and lines the display text up with the input.
  A staged-edit cell is flagged with a blue fill (`EDITED_BG`) + a solid blue left "dirty
  gutter" bar that stays visible over the row-selection tint. That bar is an ABSOLUTE
  overlay child (`absolute left_0 top_0 bottom_0 w(px(2.))`), **not `border_l_2`**: a real
  border participates in layout and shifted the cell's text 2px right, obvious next to
  unedited rows. Hover keeps the blue identity (`EDITED_HOVER`, darker) instead of the
  neutral `c.hover`. **Column separators** are ours too (the widget draws only row
  lines; `TableOptions` has no column-border flag): `col_sep(col_ix, c)` — a 1px
  absolute overlay in `c.grid` (= `theme.table_row_border`, matching the row lines) on
  each cell's LEFT edge, skipped for column 0, added in `render_th` and all three
  `render_td` branches (before the dirty gutter, so that paints over it). Left edge,
  not right: the widget pads the header's inner flex on the right (`offset_pr`), so a
  right-edge rule would sit 8px off from the body's. `edited_cells:
  HashSet<(row,col)>` tracks them, cleared whenever `pending` is. `orig_rows` snapshots
  the loaded values: editing a cell back to its original drops the staged `Update`
  (`remove_pending_update`, which also dedups re-edits of the same cell) and unmarks it.
  **Commit rules**: Enter, Tab, blur, moving to another cell, and switching tabs
  (`activate_tab` commits before `active` moves) all STAGE the edit; only Esc
  discards — unless `Settings.esc_saves_edit` is on, then Esc stages too
  (`keynav.rs::escape_cell_edit`). `cell_input` is ONE shared entity, so a cell→cell move never changes
  the focus handle and emits NO `InputEvent::Blur` — `begin_edit`/`set_current_row`
  therefore call `commit_cell_edit` themselves before repointing `editing`. Esc rides
  the global `escape`→`ClosePalette` action — which runs BEFORE the input's own escape
  (see "binding with no context" under keyboard navigation) — and clears `editing`
  first so the blur from the unmounting input finds nothing to commit.
  When browsing a table from the tree (`table_view.is_some()`) the generated
  `SELECT * FROM …` editor is hidden — the rows fill the center pane.
- **Middle-click autoscroll** (`workspace/autoscroll.rs`): wheel-press over a grid drops
  an anchor and latches the mode; a 60Hz task then scrolls BOTH axes at a speed ∝ the
  cursor's distance from the anchor (dead zone 12px, capped), Windows-style. Any click
  exits, Esc exits, and releasing the wheel after >8px of movement exits (press-drag-
  release reads as a one-shot pan). Key points:
  - It drives the widget's own handles: `TableState::{horizontal,vertical}_scroll_handle`
    are pub; the horizontal one (`VirtualListScrollHandle`) derefs to gpui's `ScrollHandle`,
    the vertical (`UniformListScrollHandle`) keeps it at `.0.borrow().base_handle`. Offsets
    are shared `Rc`s (mutating them + notifying the TableState entity is enough to repaint),
    run from `-max_offset` (scrolled to the end) to `0`, and **`set_offset` does NOT clamp** —
    `clamp_offset` does it, or the grid scrolls into empty space.
  - The press handler sits on the div wrapping `Table` and `stop_propagation()`s; the cursor
    tracking + exits are bound on the WORKSPACE ROOT and only while the mode runs. Root-level
    is required (the pointer leaves the grid), and "only while running" is what keeps the root
    from swallowing the very press that starts it.
  - Verified live on WSL X11: synthetic `xdotool mousemove/click 2` DO reach the app (the
    older "clicks are ignored" note applies to `key`/window-manager paths, not pointer input),
    so screenshot + `compare -metric AE` between frames is a usable check for "is it still
    scrolling". Effective tick rate is ~30Hz, not 60 (timer + render coalescing) — factor that
    in when tuning `SPEED`.
- **Row-number gutter + our own column resize** (`grid.rs`). Grid column 0 is a
  row-number column, so **grid column `i+1` renders result column `i`** — everything the
  `Workspace` stores (`sort_state`, `edit_cols`, `editing`, `edited_cells`) stays in
  RESULT-column space and `render_th`/`render_td` are the only places that map. The gutter
  is `Column::fixed_left()` (stays pinned while scrolling horizontally) and resizable like
  any column. Column widths live in the delegate's `Column`s; `TableState::refresh(cx)`
  rebuilds the widget's cached `col_groups` from them. `set()` carries widths over per
  column NAME so a sort/refresh/edit-reload doesn't undo a drag.
  - The widget's built-in resize handle is **2px** wide and effectively un-grabbable, and its
    `MIN_WIDTH` is 10px, so `make_grid` sets `col_resizable(false)` and we render our own
    `HANDLE_W`(9px) grab zone in `render_th`. Minimum width = the column's DEFAULT width
    (180px data / 56px gutter) — a drag can only widen.
  - `on_drag_move` fires for the whole window while a drag of that payload type is live
    (gpui checks the type, NOT the bounds), and `DragMoveEvent::bounds` is the handle's own
    box, so `new_width = old_width + (cursor.x - handle.right())` self-corrects each frame.
    A `resize_grab` offset recorded on the first move stops the column jumping by wherever
    inside the 9px zone the user grabbed.
  - **The handle needs `right(-8px)`, not `right_0`**: the widget wraps our `render_th` in a
    flex with `pr(cell_padding.right - column.paddings.right)` = 8px (room for its sort
    icon), so our element's right edge is 8px SHORT of the column's real edge — a `right_0`
    handle sits 8px left of the line the user aims at and the drag never starts. It still
    lands inside the cell, so the cell's `overflow_hidden` doesn't clip the hitbox.
  - `col_movable(false)` too: a column move reorders only the widget's `col_groups`, which
    would desync the grid→result index mapping.
  - Verifying with xdotool: `xdotool getwindowgeometry` reports the FRAME origin — the WM
    adds ~(38,59) here. Use `xwininfo -id <win>`'s "Absolute upper-left X/Y" or every
    synthetic click lands ~60px low (looks like "the handle doesn't work").
- **Multi-line / huge cell values** (`grid.rs::display_text`, `view.rs::render_value_editor`).
  Rows are a fixed 32px, so a value with newlines used to WRAP inside the row: a 3-line
  value showed only its middle line and a 20-line one bled over its neighbours. `td_text`
  is now `whitespace_nowrap` + `overflow_hidden` + `text_ellipsis`, and `display_text`
  flattens the value (`\n`→`⏎`, tab→space) and caps it at `CELL_MAX_CHARS` (300).
  **That cap is DISPLAY-only** — `begin_edit` reads the raw string from `tab.rows`, and
  export/copy read the rows too, so nothing is ever truncated for editing or saving.
  - **Editing a multi-line value in the inline input PANICS gpui**: `text_system.rs`
    asserts "text argument should not contain newlines" when it shapes a single line.
    So `Workspace` has a SECOND editor, `cell_multi` (`InputState::multi_line(true)`),
    and `Tab.editing_multi` says which one holds the value. `begin_edit` picks by
    `text.contains('\n')`, `commit_cell_edit` reads back from the same one.
  - That editor renders as a 160px strip under the grid (`render_value_editor`), NOT in
    the cell: the cell is one row tall and `render_cell` is `overflow_hidden`, so an
    expanded in-cell editor would just be clipped. In the grid the cell only shows the
    `c.active` highlight while its value is being edited below.
  - Enter inserts a newline there, so commit is the Save button or **Ctrl/Cmd+Enter** —
    and `run_active_tab` returns early (committing instead) when `editing_multi` is set,
    because that chord is also `RunQuery` and re-running reloaded the rows mid-edit.
    It must NOT commit on Blur either (clicking its own Cancel button blurs it first);
    Esc discards like any other cell edit.
- **Row selection follows the cell cursor.** The blue full-row highlight is the
  Table widget's OWN state (`TableState.selected_row`); ours is `Tab.cursor`
  (`(row, result col)`) with `Tab.current_row` always equal to its row (drives the
  `-`/delete button). `edit.rs::set_cursor` is the ONE place the cursor moves: it
  clamps, sets all three, and scrolls the cell into view. Don't write
  `current_row`/`cursor` directly or call the widget's `set_selected_row` alone.
  When a result lands (`run_sql`'s handler, the single point every result passes
  through) the cursor is KEPT and clamped to the new rows — none on an empty
  result — so refresh / re-sort / edit-reload keep your place. (It used to be
  cleared there; per-tab + clamped is what stops a stale highlight now.)
- The per-tab toolbar (`render_tab_body`): Run toggles to a red Stop while the tab is
  `running`; `+`/`-`/refresh are ALWAYS shown and disabled when N/A (`+` needs an
  editable result; `-` needs a selected row — its glyph is red only when enabled,
  else neutral `c.fg`; refresh needs the tab's `base_sql`). `reload_data(tab_id)` re-runs
  it keeping filter + sort: a Table tab REBUILDS from its WHERE / ORDER BY boxes (as Run
  does — `base_sql` predates a header sort, which only `run_sql`s), a Query tab re-runs
  `base_sql` under its header sort. (`render_center` = tab strip + active tab body |
  welcome pane.)
- **Table-tab filter bar** (`view.rs::render_filter_bar`) is styled after Zed's
  "Search threads…" box: one full-width strip, muted icon + SQL keyword, BORDERLESS
  `Input` (`appearance(false)`), a clear `×` once non-empty (clears + re-runs), mono font.
  Focus shows as the half's keyword/icon brightening (no border); red = the last run
  failed on that box's text (`Tab.clause_error`, see error marks below).
- **Live theme switch / Settings modal** (`render_settings`, gear in the sidebar
  header or Ctrl/Cmd+,): `gpui_component::Theme::change(mode, Some(window), cx)` swaps
  the theme at runtime; persist with `self.settings.save()`. List/row UIs (schema
  tree, connection list) use `.hover(|s| s.bg(c.hover))` where `c.hover = theme
  .list_hover` and `.bg(c.active)` (`theme.list_active`) for the selected row; the
  schema tree is Zed-explorer-style (chevron-right/down + database/table/eye icons +
  a `border_l_1` indent guide on each expanded schema's children).

- **Center is TABBED (Zed-style)** — `Workspace.tabs: Vec<Tab>` + `active: Option<usize>`
  (`None` = welcome pane). Each `Tab` (id, `TabKind::{Query,Scratch,Table}`, title) OWNS
  its `editor`/`where_input`/`order_input`/`table` Entities + ALL result+edit state (headers/rows/
  orig_rows/base_sql/last_sql/sort_state/edit_target/edit_cols/editing/current_row/
  new_row_idx/pending/edited_cells/running) so switching tabs preserves each one's grid,
  selection, and staged edits. Key rules baked in:
  - **Table-tab sorting rebuilds, never rewrites.** A Table tab's SQL is generated by
    `tabs.rs::table_query` from its own parts: `SELECT * FROM "s"."t" [WHERE <where_input>]
    [ORDER BY <order_input>] LIMIT 500`. A header click (`query.rs::toggle_sort`) writes
    `"col" ASC|DESC` into `order_input` and regenerates — so the ORDER BY text field (next
    to WHERE in the table toolbar, Enter re-runs) is the single source of truth, and typing
    in it clears `sort_state` (header arrows off). Do NOT re-parse the previous SQL. Query /
    Scratch tabs sort arbitrary user SQL, so they still use `util::order_by_sql`'s
    `SELECT * FROM (<base>) AS _zdb ORDER BY …` wrapper (LIMIT hoisted out).
  - The flat result fields were REMOVED from `Workspace`; every result/edit method takes a
    `tab_id` and looks up `tab_mut(id)`. Toolbar/action callers pass `active_id()`.
    `cell_input` stays SHARED on `Workspace` (only the active tab edits one cell at a time;
    `commit/cancel_cell_edit` act on the active tab).
  - **Async query handlers re-look-up the tab by id** — `run_sql`/`describe_async`/
    `apply_pending` capture `tab_id` and do `this.tab_mut(tab_id)` inside the result closure,
    bailing if the tab was closed mid-query. DON'T assume the active tab is still the one
    that issued the query. The per-result selection clear lives here, scoped to that tab's
    widget.
  - `ResultDelegate` carries a `tab_id`; `render_th`/`render_td` resolve `ws.tab(tab_id)`
    for editing/sort/edited-cell state (NOT global fields).
  - Tables open focus-if-open-else-new (`open_table_tab`); scratch is a SINGLETON
    auto-saved tab (`focus_scratch_tab`, also `ToggleScratch`/Ctrl+Shift+E); `+` →
    `open_query_tab` ("Query N"). The tab-strip close `x` is a SEPARATE clickable sibling
    of the activate region (nesting two `on_click`s would fire both → activate a
    just-removed tab).
  - **Right-click menu on a tab chip** (Close / Close Others / Close to the Right /
    Close All → `close_tab`/`close_other_tabs`/`close_tabs_right`/`close_all_tabs`).
    `ContextMenuExt::context_menu` hardcodes its element id (`"context-menu"`), and
    gpui keys element state by the id PATH — so N sibling chips with no id'd ancestor
    share ONE `ContextMenuState`: every chip renders the popup + its full-window
    `occlude()` overlay, the same `PopupMenu` entity is rendered N times, and its
    items' interactive state collides → the menu LOOKS fine but clicks do nothing.
    FIX: wrap each chip in a uniquely id'd div (`div().id(format!("tabchip-{id}"))`)
    so each context menu gets its own state. (Tree rows escape this because
    `uniform_list` already pushes a per-item id.)
  - `activate_tab` focuses the tab's input only when `window.root::<gpui_component::Root>()`
    exists — headless `#[gpui::test]` windows have no `Root`, and focusing a code-editor
    input there panics (`root.rs` "window first layer should be a Root").
  - Opening a tab needs `&mut Window`; from windowless async contexts (the selftest) set
    `pending_open` and let `render()` consume it next frame.

- **Saved tabs per connection** (`workspace/session.rs`, `zdb-config/src/tabs.rs`).
  Each connection's tab set — order, active index, and per tab: title + kind +
  (Query: editor SQL) / (Table: schema, relation, WHERE, ORDER BY) — is written to
  `<config>/zdb/tabs/<conn id>.json` (`"version": 1`, temp file + rename) and
  reopened on the next connect to it, so it survives both a restart and an
  A→B→A switch. Rows, cursor, widths and staged edits are NOT stored.
  - **Identity is `ConnectionEntry.id`** (uuid), not the name. `Settings::load`
    backfills entries from older files and writes them back once; the add form
    re-saving an existing NAME keeps that entry's id (`conn::conn_id_for`). The
    keychain is still keyed by name. The env (`ZDB_HOST`) connection has no entry
    and uses the fixed key `session::ENV_CONN_KEY`. `Workspace.conn_key` holds
    whichever applies; ids are checked to be a plain file stem before they are
    joined into a path (settings.json is hand-editable).
  - **`session_ready` is the clobber guard.** It turns true only in
    `restore_session`, and `flush_session`/`mark_session_dirty` do nothing before
    that — so a failed connect (tabs closed, nothing restored) can't overwrite the
    saved set with an empty one. `switch_connection` flushes the old connection
    FIRST, then clears the flag, then closes the tabs. Same reason the checkbox
    turned on mid-session doesn't start saving: restore never ran for this connect.
  - Restore needs `&mut Window`: a successful connect sets `pending_restore`,
    `render` consumes it. It replaces any tabs opened while not connected (no merge)
    and is skipped under `ZDB_SELFTEST`.
  - **Restored tabs don't query on connect.** A Table tab is built with
    `needs_load`; `render` → `load_restored_tab` runs it when it is the ACTIVE tab
    (render, not `activate_tab`, because `close_tab` & co. move `active` directly).
    A Query tab gets its text back and is never auto-run (it may be a `DELETE`).
  - Saves: `mark_session_dirty` (tab open/close/activate + `InputEvent::Change` on
    the Query editor / WHERE / ORDER BY) debounces one write 500ms out; explicit
    flushes on connection switch and both window-close paths. `last_saved` skips
    writes that would change nothing.
  - **Scratch is per connection** (`<config>/zdb/scratch/<conn id>.sql`), always
    saved regardless of the checkbox. The global `scratch.sql` remains the scratch
    for the not-connected state and seeds a connection that has none yet.
    `Tab::scratch` captures its key at creation — safe because every tab is closed
    on a connection switch.
  - "Query N" = highest N in use + 1 (`next_query_n`), not the tab count, so a
    restored "Query 3" never gets a twin.
  - Settings → "Restore tabs per connection" (`Settings.restore_tabs`, default ON —
    hence the manual `Default` impl; struct-level `#[serde(default)]` takes missing
    fields from it). Off = no restore, no save, files left alone.
  - All disk I/O goes through `session::store`, which is a thread-local map under
    `#[cfg(test)]` — tests must never touch the dev machine's config dir.

- **Keyboard navigation** (`workspace/keynav.rs`; bindings in `bind_nav_keys`, called
  from `main.rs` AFTER `gpui_component::init`; every handler is on the workspace
  root via `view.rs::nav_actions`). Grid: arrows / Tab / Home / End / Ctrl+Home/End /
  PgUp/PgDn move the cell cursor, Enter / F2 / typing edits, Ctrl+C copies the cell.
  Panes: Ctrl+1 tree, Ctrl+2 editor (WHERE on Table tabs), Ctrl+3 grid; Ctrl+Tab /
  Ctrl+Shift+Tab / Ctrl+W tabs; Ctrl+S applies the staged batch. Settings has two
  live checkboxes: "Esc saves cell edit" and "Vim navigation". Mechanics that cost
  time:
  - **Where a binding lives decides whether it fires.** gpui takes the DEEPEST
    matching key context and, at one depth, the LAST-registered binding — and a
    predicate is tested at every depth of the focus path, so `"Table && !Input"`
    still matches at the Table's depth while an Input inside it has focus. Hence:
    arrows + `escape` are bound in the widget's own `"Table"` context (ours win by
    being registered later; its `escape` would clear the row highlight); every other
    grid key is on `"Grid"`, a context the grid's wrapper div carries ONLY while no
    cell is being edited (`"GridEdit"` while one is, `"Grid vim"` with vim on). A
    bare-letter binding on `"Table"` would eat characters typed into the inline input.
  - **Unhandled actions fall through to the next binding.** The single-line inline
    input binds `tab`/`up`/`down` but doesn't handle them, so `tab` reaches
    `"GridEdit"` (stage + move) and `down` in the filter box reaches `"TreeFilter"`
    (focus tree). Same reason `grid_motion` is a no-op mid-edit: up/down fall out of
    the input into the `"Table"` arrows and must not walk off the cell.
  - **Type-to-edit** is an `on_key_down` on the grid wrapper (bindings run first, so
    it only sees unclaimed keys); it reads `keystroke.key_char`. Off under vim.
  - **Focus after an edit ends**: the inline input unmounts with the edit and focus
    would fall off the grid. The paths without a `&mut Window` (the input's own
    events) set `pending_grid_focus`, consumed in `render`. A Blur commit must NOT
    set it — clicking into the WHERE box would bounce focus back to the grid.
  - **`root_focus` fallback**: gpui dispatches keys along the focus path. With
    nothing focused (no tab open, last tab closed) no binding reached our handlers
    and the keyboard was dead; `render` focuses `root_focus` when `window.focused`
    is `None`. That handle is tracked by a ZERO-SIZE child of the root, NOT the root
    div: a `track_focus` element claims every mouse-down inside it (focus +
    `prevent_default`), and gpui's Windows `handle_nc_mouse_down_msg` treats a
    default-prevented press as handled — it returns before arming
    `nc_button_pressed`, so min/max/close went dead (the window couldn't be
    closed). Never put `track_focus` on anything that contains the title bar.
  - **Keep the header in step when scrolling horizontally.** The widget's
    `scroll_to_col` defers the scroll to the body's prepaint — a frame after the
    header was laid out — so the header kept showing the old columns. `set_cursor`
    computes the offset itself (`reveal_offset` + `ResultDelegate::col_span`) and
    sets the shared horizontal handle directly.
  - **Tree focus**: `TreeState`'s focus handle is private, so the sidebar body div
    has its own (`SchemaTree.focus`, context `"SchemaTree"`), and our handlers drive
    the selection through `set_selected_index` + a pure `flatten` of the retained
    items; expand/collapse edits `tree.expanded` and calls `sync_tree`. Arrows +
    Enter are bound in BOTH `"SchemaTree"` and the widget's `"Tree"` (focused by a
    click), where its own arrows would otherwise win.
  - **Keyboard context menu** (Shift+F10): `ContextMenuExt` only opens on a right
    click, so we build a `PopupMenu` ourselves (`open_tree_menu`), render it
    `deferred(anchored())` from the root, and anchor it to the selected row's
    bounds, captured by a `canvas` in `schema_tree_row`. `view.rs::node_menu` builds
    the items for both menus.
  - **A binding with NO context outranks every context binding** (cost a round):
    gpui gives a context-less binding the DEEPEST depth (`keymap.rs::binding_enabled`
    → `contexts.len()`). So `main.rs`'s global `escape` → `ClosePalette` fires before
    the focused Input's own `escape` — which is why Esc could never close a completion
    menu. Handlers of global keys must hand the key on themselves when it isn't theirs
    (`completion_menu_action` offers Esc / Tab to an open menu first).
  - **Tab is ours, not gpui-component's.** Its `Root` binds `tab` → `window.focus_next()`
    over EVERY tab stop in the window — it walked into toolbar buttons (invisible focus)
    and behind open dialogs. We bind `tab`/`shift-tab` → `FocusNextField`/`PrevField` on
    `"zdb"` (`modal.rs::step_focus`), opt the panes' Buttons out (`.tab_stop(false)`) and
    make the tree + grid handles tab stops, so Tab walks filter box → tree → editor |
    WHERE → ORDER BY → grid (landing on the grid puts the cursor on a cell). Panes that use
    Tab win by depth: grid cells, code-editor indent, `AcceptCompletion` on `"Input"`
    (Tab takes the highlighted completion, Zed-style). `FocusHandle::tab_stop` writes the
    SHARED focus entry and every `track_focus` clones from it, so setting it on our clone
    of a widget's private handle (TableState) takes effect.
  - **Terminal**: the pane/tab/apply chords are shell keys there, so their handlers
    `cx.propagate()` when the terminal has focus — an unhandled action lets the
    keystroke through to the terminal's `on_key_down`. `tab`/`shift-tab`/`escape` are
    bound to `NoAction` in the `"Terminal"` context (its wrapper div): `NoAction` masks
    every lower binding, so they reach the shell (completion, vi) instead of moving focus.
  - **Dialogs own mouse + keyboard** (`modal.rs`): one `Modal` at a time (opening one
    replaces another); the backdrop `occlude()`s the panes (a press on it closes) and
    starts BELOW the title bar so the window stays movable/closable; the panel tracks a
    focus handle that `sync_modal_focus` (render) pulls focus into and hands back on close
    (unless a palette command moved it); Tab cycles only the dialog's controls (steps
    `focus_next` until focus is inside again); the pane / tab / query chords are swallowed
    while one is up.
  - **Shortcut reference** (F1, keyboard button in the sidebar header, palette,
    Settings → "Show"): `render_shortcuts` draws `keynav::SHORTCUTS`, a hand-kept
    table — add a row there whenever a binding is added or changed.
  - Verifying: synthetic `xdotool key` DOES reach the app on WSL X11 once the window
    is activated + focused (`xdotool windowactivate/windowfocus`); the whole feature
    was checked that way with `import -window` screenshots.

- **Schema tree = the `gpui_component::tree` widget** (Zed-project-panel style: virtualized
  `uniform_list`, selection highlight, arrow-key nav, per-depth indent guides). Hierarchy:
  database root → schemas → relations → COLUMNS/INDEXES/CONSTRAINTS groups → leaves, plus
  per-schema SEQUENCES/FUNCTIONS groups. Model stays `workspace/mod.rs` `SchemaNode`/`RelNode`
  (lazy `relations`/`detail` via `DbHandle::{relations,relation_detail,schema_objects}`);
  render is `view.rs::render_sidebar` + the `schema_tree_row` free fn (returns a `ListItem`).
  Key mechanics (all source-verified against gpui-component 0.5.1 `tree.rs`):
  - **Expansion source of truth = `Workspace.expanded_ids`** (`HashSet<SharedString>` of node
    ids, `'\u{1}'`-joined: `db`, `sch␁s`, `rel␁s␁t`, `cols␁s␁t`, `col␁s␁t␁c`, `load␁<parent>`…).
    `build_tree` (pure fn) rebuilds `Vec<TreeItem>` + `NodeMeta` map from model+expanded+filter;
    `sync_tree` is the single sink (data arrival / filter / refresh / reveal): kicks lazy loads
    (`ensure_loads`, in-flight guard `tree_loads`), `set_items`, restores selection by id
    (`set_items` WIPES `selected_ix`; `flat_index_of` mirrors the widget's private flatten).
  - **Widget toggles flow back via shared state**: `TreeItem`'s expanded flag is an
    `Rc<RefCell<…>>` and `Clone` SHARES it, so the retained `tree_items` see widget-side
    toggles. There is no toggle callback; `cx.observe(&tree_state, on_tree_notify)` fires on
    every select/toggle (they all notify) and `sync_expansion` diffs retained items into
    `expanded_ids`, kicking loads. The observer NEVER calls `set_items` (no rebuild loop).
  - **Unloaded expandables get a disabled "loading…" placeholder child** (`is_folder()` ==
    `children.len()>0`, so without one the widget won't expand them).
  - **Two click targets kept**: the widget's row handler is `on_mouse_down` (not click) —
    the relation name area has its own `on_mouse_down` that `cx.stop_propagation()`s, selects
    manually, and opens via `click_tree_relation`: like Zed's project panel, one click opens
    and KEEPS the keyboard in the tree (it refocuses `tree.focus` — the stopped propagation
    skips the widget's focus-on-click, and `activate_tab` would leave focus in WHERE, so the
    arrows "stopped working"); a double-click / Enter (`TreeOpenSelected`) goes into the
    tab's grid. All tree keys, and
    how the tree gets keyboard focus despite its private `FocusHandle`, are in the
    keyboard-navigation notes above.
  - **Filter box** (`filter_input` under the sidebar header): non-empty `tree_filter` keeps
    only matching relations, force-expands db+schemas TRANSIENTLY (`sync_expansion` skips
    db/`sch␁` ids while filtering so `expanded_ids` stays clean), loads all unloaded schemas
    (global search). **Auto-reveal**: `reveal_relation` (from `activate_tab` for Table tabs)
    expands ancestors + sets `pending_reveal`; `sync_tree` consumes it only once the node is
    visible → select + `scroll_to_item(Center)`. **Context menus**: `ContextMenuExt`
    (`gpui_component::menu`) is blanket-impl'd for `ParentElement+Styled` divs; closure-based
    `PopupMenuItem::new(..).on_click(..)` (Open / Copy name / schema Refresh).
  - Rows are uniform `h(px(24.))` (uniform_list measures the first row); `ListItem` hover/
    selected styling uses the same theme tokens as `Colors.hover/active`. pg_catalog queries +
    types (`RelationDetail`/`SchemaObjects`/…) live in `zdb-db/src/introspect.rs` via `actor.rs`.
  - Tests: pure `flat_index_of`/`build_tree`/`sync_expansion` units + `#[gpui::test]` reveal &
    widget-toggle-kicks-load (simulate a toggle by mutating a retained item's shared Rc then
    notifying the `TreeState`).

- **Result export** (`zdb-db/src/export.rs`, pure fns `to_csv`/`to_json`/`to_inserts`/`to_tsv`
  + `ExportFormat`): CSV/JSON/SQL-INSERTs to a file via the native save dialog
  (`cx.prompt_for_new_path`), TSV to the clipboard (`cx.write_to_clipboard`). Toolbar
  Export(=CSV)/Copy buttons + all formats in the palette. `export_active`/`copy_active_tsv`
  read the ACTIVE tab; build the bytes while the tab is borrowed, then drop the borrow before
  touching `self.status` (borrow-conflict trap). INSERT target = the Table tab's relation, else
  `edit_target`, else `result` (`export_table_name`, a free fn).

- **EXPLAIN** (`explain_active`): prefixes the active tab's SQL with `EXPLAIN ` /
  `EXPLAIN (ANALYZE, BUFFERS) ` and runs via `run_sql` (NOT `run_new_query`, so it doesn't
  clobber `base_sql`/sort). Clears `edit_target`/`edit_cols` first so plan rows aren't editable.
  Toolbar "Explain" + palette (plain + analyze). ANALYZE actually executes the query.

- **SQL editor extras**: `Format` button/palette → `sqlformat` crate (`util::format_sql`,
  2-space, uppercased keywords) rewrites the editor via `InputState::set_value` (needs
  `&mut Window`). Run / Ctrl+Enter runs the **statement under the cursor**, not the whole
  editor: `util::statement_range(sql, cursor_byte_offset)` splits on top-level `;` honoring
  `'`/`"` quotes, `--`/`/* */` comments, and `$tag$` dollar-quoting (`InputState::cursor()`
  gives the byte offset). **Syntax highlight is NOT automatic**: `code_editor("sql")` alone
  does nothing — gpui-component gates every grammar except JSON behind its all-or-nothing
  `tree-sitter-languages` feature (28 C grammar crates), and an unknown language silently
  falls back to the JSON grammar (no error, no highlights). We register just SQL ourselves:
  direct dep `tree-sitter-sequel` + `main.rs::register_sql_language()` (called right after
  `gpui_component::init`) puts it in `LanguageRegistry::singleton()`; registered names win
  over the fallback. Don't enable the `tree-sitter-languages` feature.

- **Schema-aware completion = the bundled `sqls` LSP server, driven in-process** (`src/lsp.rs`).
  NOT a homegrown completer and NOT a hand-rolled protocol server — it's a real LSP client to
  the real `sqls` (Go) subprocess. Key points:
  - gpui-component's `Input` has a built-in `CompletionProvider` trait (lsp_types-typed) + a
    completion popup. We implement `SqlCompletion` and set it on every SQL editor's
    `InputState` via `state.lsp.completion_provider = Some(Rc::new(...))` (a **pub** field).
  - `LspHandle` (mirrors `DbHandle`): spawns `sqls -config <tmpfile>` with a **blocking reader
    thread** (no tokio in zdb-app) that parses `Content-Length`-framed JSON-RPC and completes a
    `futures_channel::oneshot` per request; the provider `.await`s that in `cx.background_spawn`.
    Reader holds a `Weak<Inner>` so it never keeps the process alive (Drop kills the child →
    stdout EOF → reader exits — avoids a kill/EOF deadlock). Handshake is **pipelined** (don't
    block on the `initialize` reply; sqls processes in order). Full-text `didOpen`/`didChange`
    sync, one URI per tab.
  - `LspSlot = Rc<RefCell<Option<LspHandle>>>` on `Workspace`, shared into every editor's
    provider; `start_lsp` fills it on connect (and switch), `None` = no completion (degrades
    silently). sqls opens its **own** Postgres connection to introspect (a 2nd connection).
  - **Hide the console**: sqls is a console subprocess; spawn sets `CREATE_NO_WINDOW`
    (`0x0800_0000`) via `creation_flags` on Windows, else a console window pops next to the app.
  - **Insertion fix (dup/eaten chars)**: sqls returns bare `label`s (no `insertText`/`textEdit`),
    so gpui-component's menu falls back to its own trigger-offset range and duplicates/eats the
    typed prefix. `with_word_range` rewrites every item with an explicit `textEdit` over
    `word_start(text,cursor)..cursor`, so the editor replaces the typed word deterministically.
    Tested by `word_start`/`with_word_range` units + the `completion_end_to_end` gate.
  - **Password is NOT written to disk**: the temp config has host/port/user/db only; the
    password goes to the subprocess via `PGPASSWORD` env (verified pgx honors it). Matches the
    keychain policy.
  - **Binary**: `scripts/build-sqls.sh` clones sqls and cross-builds all arches with Go
    (`CGO_ENABLED=0`) — must first drop the two CGO-only blank imports (`godror` Oracle,
    `mattn/go-sqlite3`); we only speak Postgres. Official releases are x86-64 only, hence
    build-from-source (native Windows ARM64, the target). zdb finds `sqls[.exe]` next to its
    own exe, else on PATH; CI's Package step bundles `sqls.exe`.
  - **Version trap**: gpui-component 0.5.1 pins `ropey = "=2.0.0-beta.1"` (its text engine —
    already in the tree for every input). Do NOT add a direct `ropey` dep (it's a beta): name
    the type via the re-export `gpui_component::Rope` in the `CompletionProvider` impl.
    `lsp-types = "=0.97.0"` (stable) must match gpui-component's exactly. `background_spawn`
    needs `use gpui::AppContext`.
  - End-to-end test: `lsp::tests::completion_end_to_end`, gated on `ZDB_TEST_SQLS` (path to the
    binary) + the `ZDB_TEST_*` Postgres env; polls the oneshot with `try_recv` (no executor).
  - **WHERE / ORDER BY complete columns too** (`SqlCompletion::clause`): alone, `id > 3 AND na`
    names no table, so sqls is shown `SELECT * FROM s.t <KW> ` + only the comma-separated item
    being typed (`list_item_start`: sqls loses the columns after `col DESC,`). Names stay
    UNQUOTED when plain (`clause_head`) — sqls resolves nothing for `"s"."t"`. Table names
    are dropped there.
  - **Menu presentation** (`tidy_items`): sqls' markdown docs go (gpui-component shows them in
    a side panel — for a table, a column listing taller than the window); a column's type
    becomes its inline detail (`integer PK`). Each item's `filter_text` is set to the label
    prefix the typed word matches: the menu highlights the first `filter_text.len()` bytes of
    the label, else `query.len()` — and its query runs on across spaces while the menu stays
    open (`WHERE c` vs label `c1`), which PANICKED gpui (`is_char_boundary(run.end)`).
  - **sqls can die** — v0.2.47 overflowed its stack on any `CASE` without `END` (i.e. while
    typing one), after which every request hung and a stale menu froze on screen. Pinned
    v0.2.48 (upstream fix). zdb also survives it: on stdout EOF the reader fails all pending
    requests (dropped senders = no completions) and marks the handle dead; the next request
    `restart()`s it (not within `RESTART_BACKOFF` of its start), and editors re-`didOpen`
    per server generation.
- **Error marks** (`query.rs::mark_failure`): a failed run is put back where it failed using
  Postgres' 1-based character `position` (`DbError::Postgres`). Editor runs remember the
  statement + its byte offset (`Tab.editor_run`); the token at the position
  (`util::error_span`) gets a diagnostic (red squiggle, hover = message + DETAIL/HINT) —
  code-editor inputs only (`InputState::diagnostics_mut`). Table tabs map the position onto
  their generated SQL (`tabs.rs::TableSql`) and redden WHERE or ORDER BY. Diagnostics are
  byte offsets into the text they were made for: `grid::clear_marks` drops them on EVERY
  editor change (stale offsets past the end would hit the same gpui text-run panic), and
  each run clears the previous marks. Deliberately NOT checked as-you-type: that means
  server round-trips (an error per pause in the server log, and a parse error inside the
  user's open transaction aborts it — needs a separate connection), and local parsers
  aren't exact Postgres.

See the auto-memory under the Claude projects dir for the running phase log.
