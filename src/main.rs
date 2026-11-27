// Files: a file explorer. The Rust side does all file-system work;
// the UI in `ui/` calls these commands through Tauri's `invoke`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod shell;

use serde::Serialize;
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Res<T> = Result<T, String>;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Entry {
    name: String,
    path: String,
    dir: bool,
    size: u64,
    modified: f64,
    created: f64,
    hidden: bool,
    system: bool,
    readonly: bool,
    link: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Drive {
    path: String,
    letter: String,
    label: String,
    kind: String,
    total: u64,
    free: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Place {
    id: String,
    name: String,
    path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FolderSize {
    size: u64,
    files: u64,
    folders: u64,
    partial: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrashEntry {
    id: String,
    name: String,
    original: String,
    deleted: f64,
}

/* ---------- helpers ---------- */

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

fn friendly(e: io::Error) -> String {
    match e.kind() {
        io::ErrorKind::PermissionDenied => {
            "Access is denied. You don't have permission to open or change this item.".into()
        }
        io::ErrorKind::NotFound => {
            "The item could not be found. It may have been moved or deleted.".into()
        }
        io::ErrorKind::AlreadyExists => "An item with that name already exists here.".into(),
        _ => e.to_string(),
    }
}

fn millis(t: io::Result<SystemTime>) -> f64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

#[cfg(windows)]
fn attrs(md: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}
#[cfg(not(windows))]
fn attrs(_: &fs::Metadata) -> u32 {
    0
}

fn lossy(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn make_entry(path: &Path) -> Option<Entry> {
    let sm = fs::symlink_metadata(path).ok()?;
    let link = sm.file_type().is_symlink();
    let md = if link {
        fs::metadata(path).unwrap_or_else(|_| sm.clone())
    } else {
        sm.clone()
    };
    let a = attrs(&sm);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| lossy(path));
    let dir = md.is_dir();
    Some(Entry {
        hidden: a & 0x2 != 0 || (!cfg!(windows) && name.starts_with('.')),
        system: a & 0x4 != 0,
        readonly: md.permissions().readonly(),
        size: if dir { 0 } else { md.len() },
        modified: millis(md.modified()),
        created: millis(md.created()),
        path: lossy(path),
        name,
        dir,
        link,
    })
}

fn check_name(name: &str) -> Res<()> {
    let n = name.trim();
    if n.is_empty() {
        return Err("Type a name.".into());
    }
    if n.chars().any(|c| "<>:\"/\\|?*".contains(c) || (c as u32) < 32) {
        return Err("A name can't contain any of these characters: \\ / : * ? \" < > |".into());
    }
    if n.ends_with('.') {
        return Err("A name can't end with a period.".into());
    }
    let stem = n.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    let numbered = (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.len() == 4
        && stem.as_bytes()[3].is_ascii_digit();
    if reserved.contains(&stem.as_str()) || numbered {
        return Err(format!("\"{n}\" is reserved by Windows. Choose a different name."));
    }
    Ok(())
}

fn split_name(name: &str, is_dir: bool) -> (String, String) {
    if is_dir {
        return (name.to_string(), String::new());
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// A free path in `dir` for `name`. Copies get Explorer-style "name - Copy" names.
fn unique_path(dir: &Path, name: &str, is_dir: bool, copy: bool) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = split_name(name, is_dir);
    if copy {
        let c = dir.join(format!("{stem} - Copy{ext}"));
        if !c.exists() {
            return c;
        }
        let mut n = 2;
        loop {
            let c = dir.join(format!("{stem} - Copy ({n}){ext}"));
            if !c.exists() {
                return c;
            }
            n += 1;
        }
    }
    let mut n = 2;
    loop {
        let c = dir.join(format!("{stem} ({n}){ext}"));
        if !c.exists() {
            return c;
        }
        n += 1;
    }
}

fn copy_rec(src: &Path, dst: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(src)?;
    if md.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_rec(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(src, dst).map(|_| ())
    }
}

fn remove_any(p: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(p)?;
    if md.is_dir() {
        fs::remove_dir_all(p)
    } else if md.file_type().is_symlink() && fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false) {
        fs::remove_dir(p)
    } else {
        fs::remove_file(p)
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        lossy(a).trim_end_matches('\\').to_lowercase() == lossy(b).trim_end_matches('\\').to_lowercase()
    } else {
        a == b
    }
}

fn is_within(child: &Path, parent: &Path) -> bool {
    if cfg!(windows) {
        let c = lossy(child).to_lowercase();
        let mut p = lossy(parent).to_lowercase();
        if !p.ends_with('\\') {
            p.push('\\');
        }
        c.starts_with(&p) || same_path(child, parent)
    } else {
        child.starts_with(parent)
    }
}

fn name_of(p: &Path) -> Res<String> {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| "A whole drive can't be copied or moved.".to_string())
}

/* ---------- listing ---------- */

#[tauri::command]
async fn list_dir(path: String) -> Res<Vec<Entry>> {
    blocking(move || {
        let rd = fs::read_dir(&path).map_err(friendly)?;
        Ok(rd.flatten().filter_map(|e| make_entry(&e.path())).collect())
    })
    .await
}

#[tauri::command]
async fn stat_many(paths: Vec<String>) -> Res<Vec<Entry>> {
    blocking(move || Ok(paths.iter().filter_map(|p| make_entry(Path::new(p))).collect())).await
}

#[tauri::command]
fn places() -> Vec<Place> {
    let list = [
        ("home", dirs::home_dir()),
        ("desktop", dirs::desktop_dir()),
        ("documents", dirs::document_dir()),
        ("downloads", dirs::download_dir()),
        ("pictures", dirs::picture_dir()),
        ("music", dirs::audio_dir()),
        ("videos", dirs::video_dir()),
    ];
    let names = ["Home", "Desktop", "Documents", "Downloads", "Pictures", "Music", "Videos"];
    list.into_iter()
        .zip(names)
        .filter_map(|((id, p), name)| {
            let p = p?;
            if !p.is_dir() {
                return None;
            }
            let name = if id == "home" {
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| name.into())
            } else {
                name.to_string()
            };
            Some(Place { id: id.into(), name, path: lossy(&p) })
        })
        .collect()
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn list_drives() -> Vec<Drive> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = ((b'A' + i as u8) as char).to_string();
        let root = format!("{letter}:\\");
        let w = wide(&root);
        let kind = match unsafe { GetDriveTypeW(w.as_ptr()) } {
            2 => "removable",
            3 => "fixed",
            4 => "network",
            5 => "cdrom",
            6 => "ram",
            _ => "unknown",
        };
        let mut buf = [0u16; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                w.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        } != 0;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
        let mut label = if ok { String::from_utf16_lossy(&buf[..end]) } else { String::new() };
        if label.is_empty() {
            label = match kind {
                "removable" => "USB Drive",
                "network" => "Network Drive",
                "cdrom" => "DVD Drive",
                _ => "Local Disk",
            }
            .into();
        }
        let (total, free) = if ok {
            (fs2::total_space(&root).unwrap_or(0), fs2::available_space(&root).unwrap_or(0))
        } else {
            (0, 0)
        };
        out.push(Drive { path: root, letter, label, kind: kind.into(), total, free });
    }
    out
}

#[cfg(not(windows))]
fn list_drives() -> Vec<Drive> {
    vec![Drive {
        path: "/".into(),
        letter: "/".into(),
        label: "File System".into(),
        kind: "fixed".into(),
        total: fs2::total_space("/").unwrap_or(0),
        free: fs2::available_space("/").unwrap_or(0),
    }]
}

#[tauri::command]
async fn drives() -> Res<Vec<Drive>> {
    blocking(|| Ok(list_drives())).await
}

/* ---------- opening ---------- */

#[tauri::command]
fn open_path(path: String, trust_cloud: Option<bool>) -> Res<()> {
    if trust_cloud.unwrap_or(false) {
        trust_cloud_download(&path);
        // Viewers like Photos let you flip through the whole folder and ask again for every marked
        // file, so the other iCloud/OneDrive downloads next to it are cleared too.
        if let Some(dir) = std::path::Path::new(&path).parent().map(|d| d.to_path_buf()) {
            std::thread::spawn(move || {
                if let Ok(rd) = std::fs::read_dir(dir) {
                    for e in rd.flatten().take(5000) {
                        if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                            trust_cloud_download(&e.path().to_string_lossy());
                        }
                    }
                }
            });
        }
    }
    open::that_detached(&path).map_err(|e| format!("Windows couldn't open this item: {e}"))
}

/// Files downloaded from your own iCloud or OneDrive carry Windows' "from the internet" mark, which
/// makes apps like Photos ask "Is this file from a trusted source?". When the mark says the file came
/// from one of those services, remove it (the same as ticking Unblock in Properties). Marks from any
/// other site are left alone.
fn trust_cloud_download(path: &str) {
    let stream = format!("{path}:Zone.Identifier");
    let Ok(text) = std::fs::read_to_string(&stream) else { return };
    let trusted = |host: &str| {
        let host = host.to_ascii_lowercase();
        [
            "icloud.com", "icloud-content.com", "apple.com",
            "onedrive.live.com", "onedrive.com", "1drv.ms", "1drv.com", "live.com", "sharepoint.com", "files.1drv.com",
        ]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    };
    let mut hosts = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(url) = line.strip_prefix("HostUrl=").or_else(|| line.strip_prefix("ReferrerUrl=")) {
            let rest = url.split("://").nth(1).unwrap_or("");
            let host = rest.split(['/', ':', '?', '#']).next().unwrap_or("");
            if !host.is_empty() {
                hosts.push(host.to_string());
            }
        }
    }
    // Every recorded source must be iCloud or OneDrive; a file with no source recorded keeps its mark.
    if !hosts.is_empty() && hosts.iter().all(|h| trusted(h)) {
        let _ = std::fs::remove_file(&stream);
    }
}

#[tauri::command]
fn open_terminal(path: String) -> Res<()> {
    #[cfg(windows)]
    {
        if std::process::Command::new("wt").args(["-d", &path]).spawn().is_ok() {
            return Ok(());
        }
        std::process::Command::new("cmd")
            .args(["/C", "start", "", "powershell", "-NoExit"])
            .current_dir(&path)
            .spawn()
            .map(|_| ())
            .map_err(friendly)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("Opening a terminal is only supported on Windows.".into())
    }
}

/* ---------- changing files ---------- */

#[tauri::command]
fn rename_item(path: String, new_name: String) -> Res<String> {
    check_name(&new_name)?;
    let new_name = new_name.trim().to_string();
    let p = PathBuf::from(&path);
    let parent = p.parent().ok_or("A drive can't be renamed here.")?;
    let target = parent.join(&new_name);
    if target.exists() && !same_path(&target, &p) {
        return Err(format!("There is already an item named \"{new_name}\" in this folder."));
    }
    fs::rename(&p, &target).map_err(friendly)?;
    Ok(lossy(&target))
}

#[tauri::command]
fn create_folder(parent: String, name: String) -> Res<String> {
    check_name(&name)?;
    let target = unique_path(Path::new(&parent), name.trim(), true, false);
    fs::create_dir(&target).map_err(friendly)?;
    Ok(lossy(&target))
}

#[tauri::command]
fn create_file(parent: String, name: String) -> Res<String> {
    check_name(&name)?;
    let target = unique_path(Path::new(&parent), name.trim(), false, false);
    fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(friendly)?;
    Ok(lossy(&target))
}

#[tauri::command]
async fn trash_items(paths: Vec<String>) -> Res<()> {
    blocking(move || trash::delete_all(&paths).map_err(|e| format!("Couldn't move to the Recycle Bin: {e}"))).await
}

#[tauri::command]
async fn delete_items(paths: Vec<String>) -> Res<()> {
    blocking(move || {
        for p in &paths {
            remove_any(Path::new(p)).map_err(friendly)?;
        }
        Ok(())
    })
    .await
}

#[tauri::command]
async fn copy_items(paths: Vec<String>, dest: String) -> Res<Vec<String>> {
    blocking(move || {
        let d = PathBuf::from(&dest);
        let mut out = Vec::new();
        for src in &paths {
            let s = PathBuf::from(src);
            let name = name_of(&s)?;
            if is_within(&d, &s) {
                return Err(format!("\"{name}\" can't be copied into itself."));
            }
            let target = unique_path(&d, &name, s.is_dir(), true);
            copy_rec(&s, &target).map_err(friendly)?;
            out.push(lossy(&target));
        }
        Ok(out)
    })
    .await
}

#[tauri::command]
async fn move_items(paths: Vec<String>, dest: String) -> Res<Vec<String>> {
    blocking(move || {
        let d = PathBuf::from(&dest);
        let mut out = Vec::new();
        for src in &paths {
            let s = PathBuf::from(src);
            let name = name_of(&s)?;
            if s.parent().map(|p| same_path(p, &d)).unwrap_or(false) {
                out.push(src.clone());
                continue;
            }
            if is_within(&d, &s) {
                return Err(format!("\"{name}\" can't be moved into itself."));
            }
            let target = unique_path(&d, &name, s.is_dir(), false);
            match fs::rename(&s, &target) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(17) || e.kind() == io::ErrorKind::CrossesDevices => {
                    copy_rec(&s, &target).map_err(friendly)?;
                    remove_any(&s).map_err(friendly)?;
                }
                Err(e) => return Err(friendly(e)),
            }
            out.push(lossy(&target));
        }
        Ok(out)
    })
    .await
}

/* ---------- inspecting ---------- */

#[tauri::command]
async fn read_text(path: String) -> Res<Option<String>> {
    blocking(move || {
        let f = fs::File::open(&path).map_err(friendly)?;
        let mut buf = Vec::new();
        f.take(64 * 1024).read_to_end(&mut buf).map_err(friendly)?;
        if buf.iter().take(8000).any(|&b| b == 0) {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    })
    .await
}

#[tauri::command]
async fn folder_size(path: String) -> Res<FolderSize> {
    blocking(move || {
        let start = Instant::now();
        let mut r = FolderSize { size: 0, files: 0, folders: 0, partial: false };
        let mut stack = vec![PathBuf::from(&path)];
        while let Some(dir) = stack.pop() {
            if start.elapsed() > Duration::from_secs(20) {
                r.partial = true;
                break;
            }
            let Ok(rd) = fs::read_dir(&dir) else {
                r.partial = true;
                continue;
            };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_symlink() {
                    continue;
                }
                if ft.is_dir() {
                    r.folders += 1;
                    stack.push(e.path());
                } else if let Ok(md) = e.metadata() {
                    r.files += 1;
                    r.size += md.len();
                }
            }
        }
        Ok(r)
    })
    .await
}

#[tauri::command]
async fn search(root: String, query: String, show_hidden: bool) -> Res<Vec<Entry>> {
    blocking(move || {
        const SKIP: [&str; 5] = ["node_modules", ".git", "$recycle.bin", "system volume information", "winsxs"];
        let terms: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let start = Instant::now();
        let mut out = Vec::new();
        let mut queue = VecDeque::from([PathBuf::from(&root)]);
        while let Some(dir) = queue.pop_front() {
            if out.len() >= 1000 || start.elapsed() > Duration::from_secs(8) {
                break;
            }
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_lowercase();
                let Ok(ft) = e.file_type() else { continue };
                let hidden = e.metadata().map(|m| attrs(&m) & 0x6 != 0).unwrap_or(false)
                    || (!cfg!(windows) && name.starts_with('.'));
                if hidden && !show_hidden {
                    continue;
                }
                let path = e.path();
                if terms.iter().all(|t| name.contains(t.as_str())) {
                    if let Some(en) = make_entry(&path) {
                        out.push(en);
                    }
                }
                if ft.is_dir() && !ft.is_symlink() && !SKIP.contains(&name.as_str()) {
                    queue.push_back(path);
                }
            }
        }
        Ok(out)
    })
    .await
}

/* ---------- Recycle Bin ---------- */

#[cfg(windows)]
fn trash_find(ids: &[String]) -> Res<Vec<trash::TrashItem>> {
    let all = trash::os_limited::list().map_err(|e| e.to_string())?;
    Ok(all
        .into_iter()
        .filter(|it| ids.is_empty() || ids.contains(&Path::new(&it.id).to_string_lossy().into_owned()))
        .collect())
}

#[tauri::command]
async fn trash_list() -> Res<Vec<TrashEntry>> {
    #[cfg(windows)]
    {
        blocking(|| {
            Ok(trash_find(&[])?
                .into_iter()
                .map(|it| TrashEntry {
                    id: Path::new(&it.id).to_string_lossy().into_owned(),
                    name: Path::new(&it.name).to_string_lossy().into_owned(),
                    original: lossy(&it.original_parent),
                    deleted: it.time_deleted as f64 * 1000.0,
                })
                .collect())
        })
        .await
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

#[tauri::command]
async fn trash_restore(ids: Vec<String>) -> Res<()> {
    #[cfg(windows)]
    {
        blocking(move || {
            if ids.is_empty() {
                return Ok(());
            }
            let items = trash_find(&ids)?;
            trash::os_limited::restore_all(items).map_err(|e| format!("Couldn't restore: {e}"))
        })
        .await
    }
    #[cfg(not(windows))]
    {
        let _ = ids;
        Ok(())
    }
}

/// Permanently removes the given Recycle Bin items, or everything when `ids` is empty.
#[tauri::command]
async fn trash_purge(ids: Vec<String>) -> Res<()> {
    #[cfg(windows)]
    {
        blocking(move || {
            let items = trash_find(&ids)?;
            if items.is_empty() {
                return Ok(());
            }
            trash::os_limited::purge_all(items).map_err(|e| format!("Couldn't delete: {e}"))
        })
        .await
    }
    #[cfg(not(windows))]
    {
        let _ = ids;
        Ok(())
    }
}

/* ---------- Windows clipboard (files), so copy/paste works with Explorer ---------- */

#[cfg(windows)]
mod winclip {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT};
    use windows_sys::Win32::UI::Shell::DragQueryFileW;

    const CF_HDROP: u32 = 15;
    const DROPEFFECT_COPY: u32 = 1;
    const DROPEFFECT_MOVE: u32 = 2;
    const DROPEFFECT_LINK: u32 = 4;

    fn open() -> Result<(), String> {
        for _ in 0..15 {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        Err("The clipboard is busy in another app. Try again.".into())
    }

    fn drop_effect_format() -> u32 {
        let w = super::wide("Preferred DropEffect");
        unsafe { RegisterClipboardFormatW(w.as_ptr()) }
    }

    pub fn set_files(paths: &[String], cut: bool) -> Result<(), String> {
        // CF_HDROP = DROPFILES header (20 bytes) + NUL-separated UTF-16 paths + final NUL.
        let mut chars: Vec<u16> = Vec::new();
        for p in paths {
            chars.extend(p.encode_utf16());
            chars.push(0);
        }
        chars.push(0);
        const HEADER: usize = 20;
        open()?;
        let result = unsafe {
            EmptyClipboard();
            let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, HEADER + chars.len() * 2);
            if h.is_null() {
                Err("Not enough memory to copy.".to_string())
            } else {
                let base = GlobalLock(h) as *mut u8;
                let header = base as *mut u32;
                *header = HEADER as u32; // pFiles: offset of the file list
                *header.add(4) = 1; // fWide: paths are UTF-16
                std::ptr::copy_nonoverlapping(chars.as_ptr() as *const u8, base.add(HEADER), chars.len() * 2);
                GlobalUnlock(h);
                if SetClipboardData(CF_HDROP, h).is_null() {
                    Err("Couldn't put the files on the clipboard.".to_string())
                } else {
                    let e = GlobalAlloc(GMEM_MOVEABLE, 4);
                    if !e.is_null() {
                        let p = GlobalLock(e) as *mut u32;
                        *p = if cut { DROPEFFECT_MOVE } else { DROPEFFECT_COPY | DROPEFFECT_LINK };
                        GlobalUnlock(e);
                        SetClipboardData(drop_effect_format(), e);
                    }
                    Ok(())
                }
            }
        };
        unsafe { CloseClipboard() };
        result
    }

    pub fn get_files() -> Result<(Vec<String>, bool), String> {
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) } == 0 {
            return Ok((Vec::new(), false));
        }
        open()?;
        let result = unsafe {
            let mut out = Vec::new();
            let h = GetClipboardData(CF_HDROP);
            if !h.is_null() {
                let n = DragQueryFileW(h, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
                for i in 0..n {
                    let len = DragQueryFileW(h, i, std::ptr::null_mut(), 0);
                    let mut buf = vec![0u16; len as usize + 1];
                    DragQueryFileW(h, i, buf.as_mut_ptr(), len + 1);
                    out.push(String::from_utf16_lossy(&buf[..len as usize]));
                }
            }
            let mut cut = false;
            let eh = GetClipboardData(drop_effect_format());
            if !eh.is_null() {
                let p = GlobalLock(eh) as *const u32;
                if !p.is_null() {
                    cut = *p & DROPEFFECT_MOVE != 0 && *p & DROPEFFECT_COPY == 0;
                    GlobalUnlock(eh);
                }
            }
            (out, cut)
        };
        unsafe { CloseClipboard() };
        Ok(result)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClipFiles {
    paths: Vec<String>,
    cut: bool,
}

#[tauri::command]
async fn clipboard_set(paths: Vec<String>, cut: bool) -> Res<()> {
    #[cfg(windows)]
    {
        blocking(move || winclip::set_files(&paths, cut)).await
    }
    #[cfg(not(windows))]
    {
        let _ = (paths, cut);
        Ok(())
    }
}

#[tauri::command]
async fn clipboard_get() -> Res<ClipFiles> {
    #[cfg(windows)]
    {
        blocking(|| winclip::get_files().map(|(paths, cut)| ClipFiles { paths, cut })).await
    }
    #[cfg(not(windows))]
    {
        Ok(ClipFiles { paths: Vec::new(), cut: false })
    }
}

/* ---------- Shell integration ---------- */

#[tauri::command]
fn show_properties(path: String) -> Res<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::SHObjectProperties;
        const SHOP_FILEPATH: u32 = 2;
        let w = wide(&path);
        let ok = unsafe { SHObjectProperties(std::ptr::null_mut(), SHOP_FILEPATH, w.as_ptr(), std::ptr::null()) };
        if ok == 0 {
            return Err("Windows couldn't show properties for this item.".into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("Properties are only available on Windows.".into())
    }
}

#[tauri::command]
fn open_with(path: String) -> Res<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("rundll32.exe")
            .raw_arg(format!("shell32.dll,OpenAs_RunDLL {path}"))
            .spawn()
            .map(|_| ())
            .map_err(friendly)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("Open with is only available on Windows.".into())
    }
}

#[tauri::command]
fn show_in_explorer(path: String) -> Res<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{path}\""))
            .spawn()
            .map(|_| ())
            .map_err(friendly)
    }
    #[cfg(not(windows))]
    {
        open::that_detached(Path::new(&path).parent().unwrap_or(Path::new("/"))).map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn new_window() -> Res<()> {
    let exe = std::env::current_exe().map_err(friendly)?;
    std::process::Command::new(exe).spawn().map(|_| ()).map_err(friendly)
}

/// Hands a drag that left Files's window over to Windows, so files can be dropped in other apps.
#[tauri::command]
fn start_drag(window: tauri::WebviewWindow, paths: Vec<String>) -> Res<()> {
    let w = window.clone();
    window
        .run_on_main_thread(move || {
            let item = drag::DragItem::Files(paths.into_iter().map(PathBuf::from).collect());
            let image = drag::Image::Raw(include_bytes!("../icons/icon.png").to_vec());
            let _ = drag::start_drag(&w, item, image, |_, _| {}, drag::Options::default());
        })
        .map_err(|e| e.to_string())
}

/* ---------- Live updates for the open folder ---------- */

struct Watch(std::sync::Mutex<Option<notify::RecommendedWatcher>>);

#[tauri::command]
fn watch_dir(app: tauri::AppHandle, state: tauri::State<'_, Watch>, path: Option<String>) -> Res<()> {
    use notify::Watcher;
    use tauri::Emitter;
    let mut slot = state.0.lock().map_err(|e| e.to_string())?;
    *slot = None;
    let Some(path) = path else { return Ok(()) };
    let reported = path.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if !matches!(ev.kind, notify::EventKind::Access(_)) {
                let _ = app.emit("fs-changed", &reported);
            }
        }
    })
    .map_err(|e| e.to_string())?;
    // Some locations (e.g. empty DVD drives) can't be watched; that's not an error for the user.
    if watcher.watch(Path::new(&path), notify::RecursiveMode::NonRecursive).is_ok() {
        *slot = Some(watcher);
    }
    Ok(())
}

/* ---------- Explorer right-click menu ---------- */

struct ShellMenu(std::sync::Mutex<std::sync::mpsc::Sender<shell::Job>>);

fn hwnd_of(window: &tauri::WebviewWindow) -> Res<isize> {
    window.hwnd().map(|h| h.0 as isize).map_err(|e| e.to_string())
}

/// Builds the Windows right-click menu for `paths` (or the folder background) and returns its items.
#[tauri::command]
async fn shell_menu(window: tauri::WebviewWindow, state: tauri::State<'_, ShellMenu>, paths: Vec<String>, background: bool) -> Res<Vec<shell::ShellItem>> {
    let hwnd = hwnd_of(&window)?;
    let (tx, rx) = std::sync::mpsc::channel();
    state
        .0
        .lock()
        .map_err(|e| e.to_string())?
        .send(shell::Job::Menu { paths, background, hwnd, reply: tx })
        .map_err(|e| e.to_string())?;
    blocking(move || rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "Windows took too long to build the menu.".to_string())?).await
}

/// Where a .lnk shortcut points (used to open folder shortcuts inside Files).
#[tauri::command]
async fn resolve_link(path: String) -> Res<Option<String>> {
    blocking(move || shell::resolve_link(&path)).await
}

/// Runs an item from the last menu built by `shell_menu`.
#[tauri::command]
async fn shell_invoke(window: tauri::WebviewWindow, state: tauri::State<'_, ShellMenu>, id: u32) -> Res<()> {
    let hwnd = hwnd_of(&window)?;
    let (tx, rx) = std::sync::mpsc::channel();
    state
        .0
        .lock()
        .map_err(|e| e.to_string())?
        .send(shell::Job::Invoke { id, hwnd, reply: tx })
        .map_err(|e| e.to_string())?;
    blocking(move || rx.recv_timeout(Duration::from_secs(120)).map_err(|_| "Windows didn't finish that action.".to_string())?).await
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 5 && args[1] == "--thumb-test" {
        println!("{}", shell::thumb_test(&args[2], args[3].parse().unwrap_or(256), args[4] == "1"));
        return;
    }
    if args.len() >= 3 && args[1] == "--trust-test" {
        trust_cloud_download(&args[2]);
        return;
    }
    let thumbs = shell::thumb_pool(4);
    tauri::Builder::default()
        .manage(Watch(std::sync::Mutex::new(None)))
        .manage(ShellMenu(std::sync::Mutex::new(shell::spawn_menu_thread())))
        // Thumbnails: <img src="http://thumb.localhost/<encoded path>?s=256">
        .register_asynchronous_uri_scheme_protocol("thumb", move |_ctx, request, responder| {
            let uri = request.uri();
            let path = shell::percent_decode(uri.path().trim_start_matches('/'));
            let query = uri.query().unwrap_or("");
            let size = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("s="))
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or(256)
                .clamp(16, 1024);
            let icon = query.split('&').any(|kv| kv == "icon=1");
            let job = shell::ThumbJob {
                path,
                size,
                icon,
                done: Box::new(move |r| {
                    let resp = match r {
                        Ok(bytes) => tauri::http::Response::builder()
                            .status(200)
                            .header("Content-Type", "image/png")
                            .header("Cache-Control", "max-age=604800")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(bytes),
                        Err(_) => tauri::http::Response::builder().status(404).body(Vec::new()),
                    };
                    if let Ok(resp) = resp {
                        responder.respond(resp);
                    }
                }),
            };
            let _ = thumbs.send(job);
        })
        .invoke_handler(tauri::generate_handler![
            shell_menu,
            shell_invoke,
            resolve_link,
            clipboard_set,
            clipboard_get,
            show_properties,
            open_with,
            show_in_explorer,
            new_window,
            start_drag,
            watch_dir,
            list_dir,
            stat_many,
            places,
            drives,
            open_path,
            open_terminal,
            rename_item,
            create_folder,
            create_file,
            trash_items,
            delete_items,
            copy_items,
            move_items,
            read_text,
            folder_size,
            search,
            trash_list,
            trash_restore,
            trash_purge
        ])
        .run(tauri::generate_context!())
        .expect("error while running Files");
}
