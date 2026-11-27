# Files

A file explorer for Windows built with Rust and Tauri. The window has no Windows title bar: tabs, minimize, maximize and close sit in Files's own title bar.

## Run

```powershell
cargo run
```

Build a fast, standalone `files.exe` (and an installer):

```powershell
cargo build --release
```

If the project lives in a synced folder (OneDrive, Dropbox), keep the build output elsewhere with a local `.cargo/config.toml` (it is git-ignored):

```toml
[build]
target-dir = "C:/Users/<you>/AppData/Local/files-build"
```

## Layout

- `src/main.rs` — all file-system work: listing folders, drives, search, copy/move, rename, Recycle Bin, opening files.
- `ui/index.html` — the interface. It calls the Rust commands with `invoke`.
- `tauri.conf.json` — window setup (frameless, size) and file access for previews.
- `capabilities/default.json` — what the UI may do with the window.

## Keys

| Key | Action |
| --- | --- |
| Enter / double-click | Open folder, or open file in its default app |
| Backspace, Alt+Left / Alt+Right | Back / forward |
| Alt+Up | Up one folder |
| Alt+Enter | Properties |
| Ctrl+N | New window |
| Ctrl+, | Settings |
| Ctrl+L | Type a path |
| Ctrl+K | Go to anything |
| Ctrl+F | Search this folder and its subfolders |
| Ctrl+T / Ctrl+W / Ctrl+Tab | New tab / close tab / next tab |
| Ctrl+C, Ctrl+X, Ctrl+V | Copy, cut, paste |
| Ctrl+D | Duplicate |
| F2 | Rename |
| Del / Shift+Del | Recycle Bin / delete permanently |
| Ctrl+Shift+N | New folder |
| Ctrl+H | Show or hide hidden items |
| Ctrl+1 / 2 / 3 | Icons / details / columns |
| S, 1–6 | Star, toggle a tag |
| F5 | Refresh |
