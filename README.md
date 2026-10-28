# zdb

A fast, minimal, Zed-style desktop client for PostgreSQL, built in Rust on
[GPUI](https://www.gpui.rs/) (the UI framework behind the Zed editor). Native on Linux,
Windows and macOS (x64 and ARM64).

![Browsing a table: filter, sort, pager, and staged inline edits](docs/screenshots/table.png)

<table>
  <tr>
    <td width="33%"><img src="docs/screenshots/query-dark.png" alt="SQL editor over its results, dark theme"></td>
    <td width="33%"><img src="docs/screenshots/apply.png" alt="Reviewing staged changes before applying them"></td>
    <td width="33%"><img src="docs/screenshots/explain-dark.png" alt="EXPLAIN ANALYZE plan view"></td>
  </tr>
  <tr>
    <td align="center">SQL editor over its results</td>
    <td align="center">Staged changes, reviewed before they run</td>
    <td align="center">EXPLAIN ANALYZE</td>
  </tr>
</table>

## Features

**Connections**
- Saved connections, added and edited in the app; one active at a time. Passwords live in
  the **OS keychain** (Windows Credential Manager / macOS Keychain / Secret Service), never
  in the settings file.
- TLS per `sslmode` (rustls), plus per-connection setup: extra connection parameters
  (`application_name`, `statement_timeout`, …) and SQL run on every new connection.
- A small **connection pool** per database (size in Settings): tabs run their queries side
  by side, and **Stop** cancels only that tab's query. A tab that opens a transaction keeps
  its connection until COMMIT / ROLLBACK, marked with an amber dot. Dropped connections
  are redialed.

**Schema browser**
- A Zed-style tree: schemas → tables / views / materialized / foreign / partitioned
  tables → columns (type, PK, NOT NULL), indexes and constraints; sequences and functions
  per schema. Loaded lazily, filterable across all schemas, and it follows the active tab.

**Tables and tabs**
- Opening a table shows its rows straight away, with **WHERE / ORDER BY / LIMIT** boxes
  (with column completion). Rows come in primary-key order unless you set one; a click on
  a column header sorts by it.
- A **pager** over the LIMIT: first / previous / next / last, with the row count. Far pages
  are read from the end, so the last page of a huge table is as quick as the first.
- Query tabs, plus an auto-saved **scratch** tab (`Ctrl/Cmd+Shift+E`). Each connection's
  tabs are saved and reopened the next time you connect to it.

**SQL editor**
- Tree-sitter highlighting and schema-aware **completion** (the bundled
  [sqls](https://github.com/sqls-server/sqls) language server). `Ctrl/Cmd+Enter` runs the
  statement under the cursor; a failed one is underlined where Postgres says it failed.
- The editor and its results share the tab half and half — drag the divider between them
  (double-click it to even them out). **Format** button for the SQL.

**Results**
- A virtualized grid with row numbers, resizable columns and middle-click autoscroll;
  `Ctrl/Cmd+C` copies a cell.
- **EXPLAIN** and **EXPLAIN ANALYZE** buttons — the plan shows in place of the rows; click
  the same button again to close it.
- **Export** to CSV, JSON or SQL `INSERT`s, or copy as TSV. The Export button saves in the
  format picked in Settings (CSV by default); the arrow next to it offers every format.

**Inline editing**
- Any result that maps to one table with a primary key is editable: double-click (or
  Enter / F2 / just type) to edit a cell, **+** adds a row, **−** deletes the selected one.
  Multi-line values get a larger editor below the grid.
- Changes are **staged** and highlighted; editing a value back un-stages it. **✓** next to
  Run (or `Ctrl/Cmd+S`) shows the statements for review, then runs them in **one
  transaction** and reloads the rows with the same filter, sort and page; **↶** reverts
  them. The review can be turned off ("Don't ask again", or Settings).

**Everything else**
- **Keyboard-first**: panes, tabs, grid and tree are all keyboard-driven, with an optional
  **Vim** mode; `F1` lists every shortcut, `Ctrl/Cmd+Shift+P` opens the command palette.
- **Embedded terminal** (`` Ctrl/Cmd+` ``) — a real PTY shell for `psql`, `claude`, …
- Light and dark themes, a Zed-style title bar, a query log and message panel you can
  hide from the status bar; the window remembers its size and place.

## Download

Every push to the repository builds all platforms on GitHub Actions: open the latest
[build run](https://github.com/tochk/zdb/actions/workflows/build.yml) and download the
artifact for your system (`zdb-linux-x64`, `zdb-linux-arm64`, `zdb-windows-x64`,
`zdb-windows-arm64`, `zdb-macos-arm64`). Each contains the app and `sqls` (keep them
side by side — completion needs it).

**macOS:** the artifact holds `zdb.app.zip`. The app is only ad-hoc signed, so clear the
quarantine flag once after unzipping (otherwise Gatekeeper reports it as damaged):

```bash
xattr -dr com.apple.quarantine /Applications/zdb.app
```

## Build & run

Requires a recent stable Rust toolchain.

```bash
cargo run -p zdb-app                 # debug
cargo build -p zdb-app --release
```

- **Linux** (Debian/Ubuntu) build deps: `pkg-config libx11-dev libxcb1-dev
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libfontconfig1-dev libvulkan-dev
  libgl1-mesa-dev libdbus-1-dev`.
- **Windows:** build on Windows itself (MSVC + the Windows SDK). GPUI compiles its shaders
  with the SDK's `fxc` at build time, so a cross-compiled `.exe` can't draw anything.
- **macOS:** ship the bundle, not the bare executable (Finder would open it in Terminal,
  with no icon): `make package-macos` → `dist/zdb.app` (run it on a Mac).
- **Completion** needs the `sqls` binary next to `zdb` or on `PATH`. `make sqls` builds it
  for every platform (needs Go); `make run` stages it for the debug build.

## Connecting

Use the **Connections** dialog (it opens on first launch, or `Ctrl/Cmd+Shift+O`). Settings
are a JSON file you can also edit by hand:

| OS | Settings file |
|----|---------------|
| Linux | `~/.config/zdb/settings.json` |
| macOS | `~/Library/Application Support/zdb/settings.json` |
| Windows | `%APPDATA%\zdb\config\settings.json` |

Saved tabs and scratch text sit next to it (`tabs/`, `scratch/`). For development you can
also connect from the environment: `ZDB_HOST`, `ZDB_PORT`, `ZDB_USER`, `ZDB_DB`,
`ZDB_PASSWORD`, `ZDB_SSL_DISABLE=1` for no TLS, and `ZDB_NAME` to label it.

## Keybindings

On macOS use `Cmd` for `Ctrl` (tab switching stays `Ctrl+Tab`). `F1` in the app lists
them all.

| Key | Action |
|-----|--------|
| `Ctrl+Enter` | Run the statement under the cursor |
| `Ctrl+S` | Apply the staged changes |
| `Ctrl+1` / `Ctrl+2` / `Ctrl+3` | Focus schema tree / editor (filter) / result grid |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` / `Ctrl+W` | Next / previous / close tab |
| `Ctrl+Shift+E` | Scratch editor |
| `Ctrl+Shift+P` | Command palette |
| `Ctrl+Shift+O` | Connections |
| `Ctrl+,` | Settings |
| `` Ctrl+` `` | Terminal |
| `F1` | All keyboard shortcuts |
| `Esc` | Close a dialog / discard a cell edit |

## Development

```
crates/
├─ zdb-db/      data layer: async DbHandle (own Tokio runtime thread), connection pool,
│               streaming queries, introspection, inline-edit SQL, export, rustls TLS
├─ zdb-config/  JSON settings, saved tabs, OS-keychain passwords
└─ zdb-app/     the GPUI application (bin `zdb`)
```

The UI talks to the database only through `DbHandle`'s channels and never touches Tokio.

```bash
cargo test                           # unit + UI tests (no database needed)

# with the PostgreSQL integration tests, against a local server:
ZDB_TEST_HOST=127.0.0.1 ZDB_TEST_USER=zdb ZDB_TEST_DB=zdb ZDB_TEST_PASSWORD=zdb cargo test

make fmt                             # rustfmt (CI fails on unformatted code)
make clippy
```

The screenshots above use the demo database in `scripts/demo-shop.sql`.
