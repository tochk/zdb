# zdb

A fast, minimal, Zed-style desktop database client for PostgreSQL, built in Rust on
[GPUI](https://www.gpui.rs/) (the UI framework behind the Zed editor).

## Features

- **Connections** — add/manage saved connections from the UI; one active connection at a
  time. Details persist to a settings file; passwords are stored in the **OS keychain**
  (Windows Credential Manager / macOS Keychain / Linux Secret Service).
- **Schema browser** — a Zed-style file-explorer tree of schemas → tables / views /
  matviews / foreign tables, with icons and lazy loading.
- **SQL editor** with Tree-sitter highlighting; run with `Ctrl/Cmd+Enter`. A separate
  auto-saved **scratch editor** (`Ctrl/Cmd+Shift+E`).
- **Virtualized results grid** that streams large results without buffering them whole.
- **Inline editing** — any query result that maps to a single table with a primary key is
  editable. **Double-click a cell** to edit it; **Add row** inserts; selecting a row lets
  you Delete it. Edited cells are **highlighted**, and editing a value back to its original
  un-stages it. Every edit is **staged and shown as SQL** for review before you **Apply**
  it (applied in one transaction, then the query reloads). Opening a table from the tree
  shows its rows directly (no redundant `SELECT` editor).
- **Sort** — click a column header to re-run the query ordered by that column (asc/desc).
- **Query log** — every executed query and applied edit, with success/failure.
- **Command palette** (`Ctrl/Cmd+Shift+P`) and keyboard-first actions.
- **Embedded terminal** (`Ctrl/Cmd+\``) — a real PTY shell (run `psql`, `claude`, …).
- **Custom Zed-style title bar** with in-window controls (no OS chrome).
- **Settings** (gear icon, or `Ctrl/Cmd+,`) — switch light/dark theme live, view the
  keybindings and config-file path. Light theme by default.

## Workspace layout

```
crates/
├─ zdb-db/      data layer: async DbHandle, streaming query exec, introspection,
│               inline-edit engine + query "describe" (editability), rustls TLS
├─ zdb-config/  JSON settings (connections/theme/keymap) + OS-keychain passwords
└─ zdb-app/     the GPUI application (bin `zdb`)
```

The data layer runs on a dedicated Tokio runtime thread; the UI talks to it only through
channels and never touches Tokio directly.

## Build & run

Requires a recent stable Rust toolchain.

```bash
cargo run -p zdb-app            # debug
cargo build -p zdb-app --release
```

Linux build deps (Debian/Ubuntu): `libfontconfig1-dev libfreetype-dev libxkbcommon-dev
libwayland-dev libvulkan-dev libxcb*-dev libdbus-1-dev clang`.

### Connecting

Use the in-app **Connections** dialog (opens on first launch). Details are saved to
`<config>/zdb/config/settings.json`; the password goes to the OS keychain. For development
you can also auto-connect via `ZDB_HOST`, `ZDB_PORT`, `ZDB_USER`, `ZDB_DB`, `ZDB_PASSWORD`
(+ `ZDB_SSL_DISABLE=1` for non-TLS). Switch the theme from **Settings** (gear icon /
`Ctrl/Cmd+,`), or by editing `"theme"` in the settings file.

## Keybindings

| Key | Action |
|-----|--------|
| `Ctrl/Cmd+Enter` | Run the SQL in the editor |
| `Ctrl/Cmd+Shift+P` | Toggle the command palette |
| `Ctrl/Cmd+Shift+O` | Manage connections |
| `Ctrl/Cmd+Shift+E` | Toggle the scratch editor |
| `Ctrl/Cmd+,` | Open settings |
| `Ctrl/Cmd+\`` | Toggle the embedded terminal |
| `Esc` | Close palette / dialog |

Inline editing: **double-click** a cell to edit, **Enter** to stage, then **Apply**.

## Tests

```bash
cargo test                       # unit + UI tests (no DB needed)

# include PostgreSQL integration tests against a local server:
ZDB_TEST_HOST=127.0.0.1 ZDB_TEST_USER=zdb ZDB_TEST_DB=zdb ZDB_TEST_PASSWORD=zdb cargo test
```

## Windows

Built for Windows (incl. ARM64) — see `BUILDING-WINDOWS.md` and `CLAUDE.md`. Windows
binaries are built natively (locally or by CI); the packaged folder is self-contained:
keep `zdb.exe` together with `sqls.exe` and any bundled `.dll`.
