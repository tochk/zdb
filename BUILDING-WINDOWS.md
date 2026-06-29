# Building zdb for Windows

zdb's UI (gpui) renders with DirectX 11 + DirectWrite on Windows. gpui's Windows
build script precompiles its HLSL shaders with `fxc` (Windows SDK) and embeds the
Common-Controls v6 manifest — but only when the build runs **on Windows**
(`#[cfg(target_os = "windows")]`). Cross-compiling from Linux therefore can't
precompile shaders (forcing a runtime-shader workaround) and the resulting binary
fails at GPU/text init on real Windows (`Error creating DirectWriteTextSystem`) —
confirmed for both the GNU x64 and GNU ARM64 cross builds. **A working Windows
binary must be built on Windows.** Two supported ways below.

## Native build (on a Windows PC)

1. Install the **MSVC build tools** — either Visual Studio 2022 with
   "Desktop development with C++", or the standalone "Build Tools for Visual
   Studio" (includes `link.exe` and the Windows SDK with `fxc.exe`).
2. Install Rust via <https://rustup.rs> (defaults to `x86_64-pc-windows-msvc`).
3. From the repo root:

   ```powershell
   cargo build -p zdb-app --release
   # → target\release\zdb.exe
   ```

4. For Windows on ARM:

   ```powershell
   rustup target add aarch64-pc-windows-msvc
   cargo build -p zdb-app --release --target aarch64-pc-windows-msvc
   ```

The resulting `zdb.exe` is self-contained against system DLLs and needs no
installer (Windows 10 1903+ / 11).

## CI build (no Windows machine needed locally)

`.github/workflows/windows.yml` builds x64 and arm64 with MSVC on a
`windows-latest` runner and uploads the binaries as artifacts. Trigger it from
the Actions tab ("Run workflow") or by pushing a `v*` tag, then download the
`zdb-windows-x64` / `zdb-windows-arm64` artifacts.

## Running

See the in-app connection notes in `README.md`. Set `ZDB_HOST`/`ZDB_USER`/
`ZDB_DB`/`ZDB_PASSWORD` (and `ZDB_SSL_DISABLE=1` for non-TLS) or add a connection
to `settings.json`.
