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
               rustls(ring) TLS.
  zdb-config/  JSON settings (connections, theme, keymap) + OS-keychain passwords
               (secret.rs, keyring; per-OS backend).
  zdb-app/     gpui app (bin `zdb`): workspace/ (the UI, split into mod.rs =
               Workspace struct + logic methods + tests; view.rs = all render_*
               + impl Render (child module, reaches Workspace privates, no
               visibility bumps); grid.rs = Tab/TabKind + ResultDelegate;
               colors.rs = Colors/palette; util.rs = pure SQL/config helpers),
               terminal.rs, main.rs (entry, WSL X11 guard, keybindings, theme).
vendor/gpui/   PATCHED copy of gpui 0.2.2 (see Windows notes). [patch.crates-io].
```

The UI talks to the DB only through `DbHandle` channels; it never touches Tokio.

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

## Windows builds (this is the important part)

Target is **Windows on ARM64** (the dev box is WSL2 aarch64; the host is ARM64).

- Cross-compile with the **llvm-mingw** `aarch64-pc-windows-gnullvm` toolchain
  (`~/.local/llvm-mingw`), env `CC_/CXX_/AR_aarch64_pc_windows_gnullvm` + PATH.
  `[profile.release] debug-assertions = true` is REQUIRED (gpui only precompiles
  its HLSL shaders on a Windows host; debug_assertions makes it compile them at
  runtime instead). Set it in the PROFILE, not RUSTFLAGS (RUSTFLAGS doesn't reach
  host proc-macros → `gpui_macros::derive_inspector_reflection` won't compile).
- The runtime-shader path reads `*.hlsl` from gpui's build-time `CARGO_MANIFEST_DIR`,
  which doesn't exist on the user's machine → `Error creating DirectWriteTextSystem`
  (os error 3). FIX: **vendored gpui** (`vendor/gpui`) patches
  `directx_renderer.rs::build_shader_blob` to read `<exe_dir>/shaders/<name>` first;
  the package bundles those `.hlsl` in a `shaders/` folder next to `zdb.exe`.
- Manifest: `crates/zdb-app/build.rs` (embed-resource) compiles `resources/zdb.rc`
  embedding `zdb.manifest` (Common-Controls v6 → fixes `TaskDialogIndirect`) and
  `zdb.ico` (app icon). GUI subsystem via `#![cfg_attr(target_os="windows", ...)]`.
- Package + bundle: `scripts/package-windows.sh aarch64-pc-windows-gnullvm arm64`
  → `dist/zdb-windows-arm64.zip` (exe + libunwind.dll + shaders/ + README).
- Don't bother with MSVC-cross (cargo-xwin): `ring` fails on aarch64-msvc; not needed.

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

## Gotchas

- Background-command "exit 0" from `… | tee | tail` is the tail's exit — always
  grep the log for `error:` to confirm a build actually passed.
- **Custom (Zed-style) title bar** lives in `workspace.rs::render_titlebar` (not
  the gpui-component `TitleBar`). Window opens borderless via
  `gpui_component::TitleBar::title_bar_options()` in `main.rs` (`appears_transparent`
  → vendored gpui sets `hide_title_bar`, killing the native frame on Win/Linux).
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
- Resizable panels: gpui-component seeds any `resizable_panel()` WITHOUT an explicit
  `.size()` to `PANEL_MIN_SIZE` (state.rs `sync_panels_count`), then ratio-scales to
  the container. A group where the first panel had no size squeezed the last panel
  off-screen (the bottom query-log / review pane was clipped below the window). Give
  EVERY panel an explicit `.size()` (the ratios are scaled to fit) — don't leave one
  unsized expecting it to flex-fill.
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
  border (those re-introduce the shift), just a subtle `bg(c.active)` to signal edit
  mode (matched by `pt(px(3.))` to the display cells' top-aligned `py_1`). A staged-edit
  cell is flagged with a blue fill (`EDITED_BG`) + a solid blue `border_l_2` "dirty
  gutter" that stays visible over the row-selection tint; hover keeps the blue identity
  (`EDITED_HOVER`, darker) instead of the neutral `c.hover`. `edited_cells:
  HashSet<(row,col)>` tracks them, cleared whenever `pending` is. `orig_rows` snapshots
  the loaded values: editing a cell back to its original drops the staged `Update`
  (`remove_pending_update`, which also dedups re-edits of the same cell) and unmarks it.
  When browsing a table from the tree (`table_view.is_some()`) the generated
  `SELECT * FROM …` editor is hidden — the rows fill the center pane.
- **Row selection is the gpui-component Table widget's OWN state**, separate from
  our `current_row`. The blue full-row highlight comes from `TableState
  .selected_row` (set by the widget's internal click handler); `set_current_row`
  only tracks our copy (drives the `-`/delete button). So clearing `current_row`
  is NOT enough — a stale highlight stays lit when switching tables (row 5 in
  table A → row 5 lit in table B). FIX: `ts.clear_selection(cx)` + reset
  `current_row` in the ONE place every result lands — the async query-result
  handler in `run_sql` (right where `delegate_mut().set(...)` swaps the rows).
  That covers table switch, re-sort, reload, and edit-reload in a single spot.
- The per-tab toolbar (`render_tab_body`): Run toggles to a red Stop while the tab is
  `running`; `+`/`-`/refresh are ALWAYS shown and disabled when N/A (`+` needs an
  editable result; `-` needs a selected row — its glyph is red only when enabled,
  else neutral `c.fg`; refresh needs the tab's `base_sql`). `reload_data(tab_id)` re-runs
  it. (`render_center` = tab strip + active tab body | welcome pane.)
- **Live theme switch / Settings modal** (`render_settings`, gear in the sidebar
  header or Ctrl/Cmd+,): `gpui_component::Theme::change(mode, Some(window), cx)` swaps
  the theme at runtime; persist with `self.settings.save()`. List/row UIs (schema
  tree, connection list) use `.hover(|s| s.bg(c.hover))` where `c.hover = theme
  .list_hover` and `.bg(c.active)` (`theme.list_active`) for the selected row; the
  schema tree is Zed-explorer-style (chevron-right/down + database/table/eye icons +
  a `border_l_1` indent guide on each expanded schema's children).

- **Center is TABBED (Zed-style)** — `Workspace.tabs: Vec<Tab>` + `active: Option<usize>`
  (`None` = welcome pane). Each `Tab` (id, `TabKind::{Query,Scratch,Table}`, title) OWNS
  its `editor`/`where_input`/`table` Entities + ALL result+edit state (headers/rows/
  orig_rows/base_sql/last_sql/sort_state/edit_target/edit_cols/editing/current_row/
  new_row_idx/pending/edited_cells/running) so switching tabs preserves each one's grid,
  selection, and staged edits. Key rules baked in:
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
  - `activate_tab` focuses the tab's input only when `window.root::<gpui_component::Root>()`
    exists — headless `#[gpui::test]` windows have no `Root`, and focusing a code-editor
    input there panics (`root.rs` "window first layer should be a Root").
  - Opening a tab needs `&mut Window`; from windowless async contexts (the selftest) set
    `pending_open` and let `render()` consume it next frame.

See the auto-memory under the Claude projects dir for the running phase log.
