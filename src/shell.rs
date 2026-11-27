//! Windows shell integration: the Explorer right-click menu (as data, so the UI can draw it
//! in its own design) and thumbnails from the Windows thumbnail cache.

use serde::Serialize;
use std::ffi::c_void;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{Interface, HSTRING, PCSTR, PCWSTR, PSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    HBITMAP, HGDIOBJ,
};
use windows::Win32::System::Com::{CoInitializeEx, IBindCtx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, IShellItemImageFactory, ILFindLastID, ILFree,
    SHBindToParent, SHCreateItemFromParsingName, SHGetDesktopFolder, SHParseDisplayName, CMF_NORMAL,
    CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, GCS_VERBW, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
    SIIGBF_THUMBNAILONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, DispatchMessageW, GetMenuItemCount, GetMenuItemInfoW, PeekMessageW,
    TranslateMessage, HMENU, MENUITEMINFOW, MFT_SEPARATOR, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING,
    MIIM_SUBMENU, MSG, PM_REMOVE,
};

type Res<T> = Result<T, String>;

const FIRST: u32 = 1;
const LAST: u32 = 0x7FFF;
const WM_INITMENUPOPUP: u32 = 0x0117;
const CMIC_MASK_UNICODE: u32 = 0x4000;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ShellItem {
    pub id: u32,
    pub label: String,
    pub verb: String,
    pub disabled: bool,
    pub separator: bool,
    pub children: Vec<ShellItem>,
}

pub enum Job {
    Menu { paths: Vec<String>, background: bool, hwnd: isize, reply: Sender<Res<Vec<ShellItem>>> },
    Invoke { id: u32, hwnd: isize, reply: Sender<Res<()>> },
}

struct Held {
    cm: IContextMenu,
    menu: HMENU,
}

fn msg(e: windows::core::Error) -> String {
    let m = e.message().to_string();
    if m.is_empty() { format!("Windows error {:#x}", e.code().0) } else { m }
}

/// One STA thread owns the shell menu objects, so the menu built for a right-click
/// is still alive when the user picks an item from it.
pub fn spawn_menu_thread() -> Sender<Job> {
    let (tx, rx): (Sender<Job>, Receiver<Job>) = channel();
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mut held: Option<Held> = None;
        loop {
            pump();
            match rx.recv_timeout(Duration::from_millis(40)) {
                Ok(Job::Menu { paths, background, hwnd, reply }) => {
                    if let Some(h) = held.take() {
                        let _ = DestroyMenu(h.menu);
                    }
                    let r = build(&paths, background, HWND(hwnd as *mut c_void)).map(|(h, items)| {
                        held = Some(h);
                        items
                    });
                    let _ = reply.send(r);
                }
                Ok(Job::Invoke { id, hwnd, reply }) => {
                    let r = match &held {
                        Some(h) => invoke(&h.cm, id, HWND(hwnd as *mut c_void)),
                        None => Err("That menu is no longer available. Right-click again.".into()),
                    };
                    let _ = reply.send(r);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    tx
}

unsafe fn pump() {
    let mut m = MSG::default();
    while PeekMessageW(&mut m, None, 0, 0, PM_REMOVE).as_bool() {
        let _ = TranslateMessage(&m);
        DispatchMessageW(&m);
    }
}

unsafe fn pidl_of(path: &str) -> Res<*mut ITEMIDLIST> {
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    SHParseDisplayName(&HSTRING::from(path), None::<&IBindCtx>, &mut pidl, 0, None).map_err(msg)?;
    Ok(pidl)
}

unsafe fn build(paths: &[String], background: bool, hwnd: HWND) -> Res<(Held, Vec<ShellItem>)> {
    if paths.is_empty() {
        return Err("Nothing selected.".into());
    }
    let cm: IContextMenu = if background {
        let pidl = pidl_of(&paths[0])?;
        let desktop = SHGetDesktopFolder().map_err(msg)?;
        let folder: Res<IShellFolder> = if (*pidl).mkid.cb == 0 {
            Ok(desktop)
        } else {
            desktop.BindToObject(pidl, None::<&IBindCtx>).map_err(msg)
        };
        let r = folder.and_then(|f| f.CreateViewObject::<IContextMenu>(hwnd).map_err(msg));
        ILFree(Some(pidl));
        r?
    } else {
        let pidls = paths.iter().map(|p| pidl_of(p)).collect::<Res<Vec<_>>>()?;
        let parent: Res<IShellFolder> = SHBindToParent(pidls[0], None).map_err(msg);
        let children: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| ILFindLastID(*p) as *const ITEMIDLIST).collect();
        let r = parent.and_then(|f| f.GetUIObjectOf::<IContextMenu>(hwnd, &children, None).map_err(msg));
        for p in pidls {
            ILFree(Some(p));
        }
        r?
    };
    let menu = CreatePopupMenu().map_err(msg)?;
    let hr = cm.QueryContextMenu(menu, 0, FIRST, LAST, CMF_NORMAL);
    if hr.is_err() {
        let _ = DestroyMenu(menu);
        return Err(msg(hr.into()));
    }
    let cm2 = cm.cast::<IContextMenu2>().ok();
    let cm3 = cm.cast::<IContextMenu3>().ok();
    let items = read(menu, &cm, &cm2, &cm3, 0);
    Ok((Held { cm, menu }, items))
}

fn clean(label: &str) -> String {
    let label = label.split('\t').next().unwrap_or("");
    let mut out = String::new();
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                out.push('&');
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

unsafe fn read(menu: HMENU, cm: &IContextMenu, cm2: &Option<IContextMenu2>, cm3: &Option<IContextMenu3>, depth: u32) -> Vec<ShellItem> {
    let mut out: Vec<ShellItem> = Vec::new();
    let n = GetMenuItemCount(Some(menu)).max(0) as u32;
    for i in 0..n {
        let mut buf = [0u16; 512];
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_STRING,
            dwTypeData: PWSTR(buf.as_mut_ptr()),
            cch: (buf.len() - 1) as u32,
            ..Default::default()
        };
        if GetMenuItemInfoW(menu, i, true, &mut mii).is_err() {
            continue;
        }
        if mii.fType.0 & MFT_SEPARATOR.0 != 0 {
            if out.last().map(|l| !l.separator).unwrap_or(false) {
                out.push(ShellItem { id: 0, label: String::new(), verb: String::new(), disabled: true, separator: true, children: vec![] });
            }
            continue;
        }
        let len = (mii.cch as usize).min(buf.len() - 1);
        let label = clean(&String::from_utf16_lossy(&buf[..len]));
        let has_sub = !mii.hSubMenu.0.is_null();
        let mut children = Vec::new();
        if has_sub && depth < 4 {
            // Submenus such as "Send to" and "Open with" fill themselves in when they are about to open.
            let w = WPARAM(mii.hSubMenu.0 as usize);
            let l = LPARAM(i as isize);
            if let Some(c3) = cm3 {
                let _ = c3.HandleMenuMsg2(WM_INITMENUPOPUP, w, l, None);
            } else if let Some(c2) = cm2 {
                let _ = c2.HandleMenuMsg(WM_INITMENUPOPUP, w, l);
            }
            children = read(mii.hSubMenu, cm, cm2, cm3, depth + 1);
        }
        let mut verb = String::new();
        if !has_sub && mii.wID >= FIRST && mii.wID <= LAST {
            let mut vb = [0u16; 256];
            if cm
                .GetCommandString((mii.wID - FIRST) as usize, GCS_VERBW, None, PSTR(vb.as_mut_ptr() as *mut u8), vb.len() as u32)
                .is_ok()
            {
                let end = vb.iter().position(|&c| c == 0).unwrap_or(0);
                verb = String::from_utf16_lossy(&vb[..end]).to_lowercase();
            }
        }
        if label.is_empty() || (has_sub && children.iter().all(|c| c.separator)) {
            continue;
        }
        out.push(ShellItem { id: mii.wID, label, verb, disabled: mii.fState.0 & 3 != 0, separator: false, children });
    }
    while out.last().map(|l| l.separator).unwrap_or(false) {
        out.pop();
    }
    if out.first().map(|l| l.separator).unwrap_or(false) {
        out.remove(0);
    }
    out
}

unsafe fn invoke(cm: &IContextMenu, id: u32, hwnd: HWND) -> Res<()> {
    let offset = (id - FIRST) as usize;
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE,
        hwnd,
        lpVerb: PCSTR(offset as *const u8),
        lpVerbW: PCWSTR(offset as *const u16),
        nShow: 1,
        ..Default::default()
    };
    cm.InvokeCommand(&info as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO).map_err(msg)
}

/* ---------- Thumbnails ---------- */

pub struct ThumbJob {
    pub path: String,
    pub size: i32,
    pub icon: bool,
    pub done: Box<dyn FnOnce(Res<Vec<u8>>) + Send>,
}

/// A few STA worker threads that ask Windows for thumbnails (the same ones Explorer shows).
pub fn thumb_pool(workers: usize) -> Sender<ThumbJob> {
    let (tx, rx) = channel::<ThumbJob>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..workers {
        let rx = rx.clone();
        std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            loop {
                let job = match rx.lock() {
                    Ok(g) => g.recv(),
                    Err(_) => break,
                };
                match job {
                    Ok(j) => {
                        let r = thumbnail(&j.path, j.size, j.icon);
                        (j.done)(r);
                    }
                    Err(_) => break,
                }
            }
        });
    }
    tx
}

struct Img {
    w: u32,
    h: u32,
    px: Vec<u8>, // straight RGBA
}

unsafe fn grab(path: &str, size: i32, icon: bool) -> Res<Img> {
    let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None::<&IBindCtx>).map_err(msg)?;
    let flags = if icon { SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK } else { SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK };
    let hbmp = factory.GetImage(SIZE { cx: size, cy: size }, flags).map_err(msg)?;
    let r = bitmap_rgba(hbmp);
    let _ = DeleteObject(HGDIOBJ(hbmp.0));
    r
}

/// Bounding box of the visible (non-transparent) pixels.
fn bbox(img: &Img) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for y in 0..img.h {
        for x in 0..img.w {
            if img.px[((y * img.w + x) * 4 + 3) as usize] > 8 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x0 == u32::MAX { None } else { Some((x0, y0, x1, y1)) }
}

/// Crops to the visible pixels, kept square so icons stay centered.
fn trim(img: Img) -> Img {
    let Some((x0, y0, x1, y1)) = bbox(&img) else { return img };
    let side = (x1 - x0).max(y1 - y0);
    let cx = (x0 + x1) / 2;
    let cy = (y0 + y1) / 2;
    let sx = cx as i64 - side as i64 / 2;
    let sy = cy as i64 - side as i64 / 2;
    let mut px = vec![0u8; (side * side * 4) as usize];
    for y in 0..side {
        for x in 0..side {
            let (ox, oy) = (sx + x as i64, sy + y as i64);
            if ox < 0 || oy < 0 || ox >= img.w as i64 || oy >= img.h as i64 {
                continue;
            }
            let s = ((oy as u32 * img.w + ox as u32) * 4) as usize;
            let d = ((y * side + x) * 4) as usize;
            px[d..d + 4].copy_from_slice(&img.px[s..s + 4]);
        }
    }
    Img { w: side, h: side, px }
}

/// When a type only has a small icon, Windows draws it inside a thin grey square at large sizes.
/// Detects that frame (a mostly solid outer ring around a mostly empty inside) and wipes it, leaving
/// only the real icon. Returns whether a frame was removed.
fn unframe(mut img: Img) -> (Img, bool) {
    let (w, h) = (img.w as i64, img.h as i64);
    if w < 64 || h < 64 {
        return (img, false);
    }
    let a = |img: &Img, x: i64, y: i64| img.px[((y * w + x) * 4 + 3) as usize];
    // Outer ring: is the 2 px border mostly visible?
    let (mut ring, mut ring_on) = (0u32, 0u32);
    for i in 0..w {
        for d in 0..2 {
            for (x, y) in [(i, d), (i, h - 1 - d)] {
                ring += 1;
                ring_on += (a(&img, x, y) > 8) as u32;
            }
        }
    }
    for j in 2..h - 2 {
        for d in 0..2 {
            for (x, y) in [(d, j), (w - 1 - d, j)] {
                ring += 1;
                ring_on += (a(&img, x, y) > 8) as u32;
            }
        }
    }
    if (ring_on as f32) < ring as f32 * 0.85 {
        return (img, false);
    }
    // Just inside the ring: is there a clear band (the frame's empty padding)?
    // The frame line itself is a few px wide with a soft shadow, so the band starts a bit further in.
    let inset = (w.min(h) / 16).max(10);
    let (mut band, mut band_on) = (0u32, 0u32);
    for y in 6..h - 6 {
        for x in 6..w - 6 {
            let edge = x < inset || y < inset || x >= w - inset || y >= h - inset;
            if edge {
                band += 1;
                band_on += (a(&img, x, y) > 8) as u32;
            }
        }
    }
    if (band_on as f32) > band as f32 * 0.1 {
        return (img, false);
    }
    // Framed: clear everything outside the inset so trim() keeps only the icon.
    for y in 0..h {
        for x in 0..w {
            if x < inset || y < inset || x >= w - inset || y >= h - inset {
                img.px[((y * w + x) * 4 + 3) as usize] = 0;
            }
        }
    }
    (img, true)
}

/// Icons: Windows puts small-only icons (e.g. a 48 px icon) in the middle of a big empty canvas when a
/// large size is requested. Try a few sizes, keep the one with the most real detail, and trim the padding
/// so every icon fills its frame the same way.
unsafe fn best_icon(path: &str, size: i32) -> Res<(Img, String)> {
    // Programs and shortcuts: read the icon straight from the file that holds it. The shell's image
    // factory sometimes hands back a small icon drawn inside a grey frame, which looks tiny.
    if let Some((file, index)) = icon_source(path) {
        match resource_icon(&file, index, size.clamp(16, 256)) {
            Ok(img) if bbox(&img).is_some() => return Ok((trim(img), format!("resource {file},{index}; "))),
            Ok(_) => {}
            Err(e) => eprintln!("icon {file}: {e}"),
        }
    }
    let mut sizes = vec![size.min(256), 256, 128, 96, 64, 48, 32];
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    sizes.dedup();
    let mut best: Option<(Img, u32)> = None;
    let mut log = String::new();
    // Once a frame shows up, the real icon is only as big as what was inside it: bigger sizes are framed too.
    let mut art_cap: Option<u32> = None;
    for s in sizes {
        if art_cap.is_some_and(|c| s as u32 > c) {
            continue;
        }
        let Ok(img) = grab(path, s, true) else { continue };
        let (img, framed) = unframe(img);
        let side = bbox(&img).map(|(x0, y0, x1, y1)| (x1 - x0).max(y1 - y0)).unwrap_or(0);
        if framed {
            log.push_str("framed; ");
            art_cap = Some(art_cap.map_or(side, |c| c.min(side)));
        }
        log.push_str(&format!("{s}px->canvas {}x{} visible {side}; ", img.w, img.h));
        if let Ok(dir) = std::env::var("FILES_THUMB_DUMP") {
            if let Ok(b) = encode(&img) { let _ = std::fs::write(format!("{dir}/{s}.png"), b); }
        }
        // A canvas that is mostly filled is the real artwork at that size: stop there.
        let filled = side as f32 >= img.w.min(img.h) as f32 * 0.7;
        if best.as_ref().map(|(_, b)| side > *b).unwrap_or(true) {
            best = Some((img, side));
        }
        if filled && std::env::var("FILES_THUMB_DUMP").is_err() {
            break;
        }
    }
    let (img, _) = best.ok_or_else(|| "No icon.".to_string())?;
    Ok((trim(img), log))
}

fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find('%') {
        let Some(b) = rest[a + 1..].find('%') else { break };
        out.push_str(&rest[..a]);
        let name = &rest[a + 1..a + 1 + b];
        out.push_str(&std::env::var(name).unwrap_or_else(|_| format!("%{name}%")));
        rest = &rest[a + 2 + b..];
    }
    out.push_str(rest);
    out
}

/// Where a file's icon lives: (file, index). Shortcuts use their icon location, else their target.
unsafe fn icon_source(path: &str) -> Option<(String, i32)> {
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    let lower = path.to_ascii_lowercase();
    let holds_icon = |p: &str| {
        let p = p.to_ascii_lowercase();
        [".exe", ".ico", ".dll"].iter().any(|x| p.ends_with(x))
    };
    if holds_icon(&lower) {
        return Some((path.to_string(), 0));
    }
    if !lower.ends_with(".lnk") {
        return type_icon_location(path);
    }
    let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
    let file: IPersistFile = link.cast().ok()?;
    file.Load(&HSTRING::from(path), STGM_READ).ok()?;
    let mut buf = [0u16; 1024];
    let mut idx = 0i32;
    if link.GetIconLocation(&mut buf, &mut idx).is_ok() {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
        let loc = expand_env(&String::from_utf16_lossy(&buf[..end]));
        if !loc.is_empty() && std::path::Path::new(&loc).exists() {
            return Some((loc, idx));
        }
    }
    let mut buf = [0u16; 1024];
    link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
    let target = expand_env(&String::from_utf16_lossy(&buf[..end]));
    if target.is_empty() || !std::path::Path::new(&target).exists() {
        return None;
    }
    if holds_icon(&target) { Some((target, 0)) } else { type_icon_location(&target) }
}

/// The icon Windows registers for a file's type (e.g. the default browser's icon for .html),
/// as (file, index). None when the type draws its icon some other way.
unsafe fn type_icon_location(path: &str) -> Option<(String, i32)> {
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICONLOCATION};
    let mut fi = SHFILEINFOW::default();
    let ok = SHGetFileInfoW(
        &HSTRING::from(path),
        FILE_FLAGS_AND_ATTRIBUTES(0),
        Some(&mut fi),
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_ICONLOCATION,
    );
    if ok == 0 {
        return None;
    }
    let end = fi.szDisplayName.iter().position(|&c| c == 0).unwrap_or(0);
    let loc = expand_env(&String::from_utf16_lossy(&fi.szDisplayName[..end]));
    if loc.is_empty() || loc.starts_with('@') || !std::path::Path::new(&loc).exists() {
        return None;
    }
    Some((loc, fi.iIcon))
}

/// Files whose icon belongs to the file itself rather than to its type.
fn own_icon(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    [".exe", ".lnk", ".ico", ".url", ".scr", ".cpl", ".appref-ms", ".cur", ".ani", ".msc"].iter().any(|x| p.ends_with(x))
}

/// Type icons are the same for every file of that type, so they are made once per extension and size.
fn type_icon_cache() -> &'static Mutex<std::collections::HashMap<String, Vec<u8>>> {
    static C: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Vec<u8>>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Extracts the icon at `index` from `file` at `size` px, picking the best image the file has.
unsafe fn resource_icon(file: &str, index: i32, size: i32) -> Res<Img> {
    use windows::Win32::UI::Shell::SHDefExtractIconW;
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
    let mut hicon = HICON::default();
    SHDefExtractIconW(&HSTRING::from(file), index, 0, Some(&mut hicon), None, size as u32).ok().map_err(msg)?;
    if hicon.is_invalid() {
        return Err("No icon in file.".into());
    }
    let mut info = ICONINFO::default();
    let r = GetIconInfo(hicon, &mut info).map_err(msg).and_then(|_| {
        let color = raw_bgra(info.hbmColor);
        let mask = raw_bgra(info.hbmMask);
        let (w, h, mut px) = color?;
        let has_alpha = px.chunks(4).any(|p| p[3] != 0);
        let mask = mask.ok();
        for (i, p) in px.chunks_mut(4).enumerate() {
            p.swap(0, 2);
            if !has_alpha {
                // Old icons: transparency comes from the AND mask (white = transparent).
                let see_through = mask.as_ref().map(|(_, _, m)| m.get(i * 4).copied().unwrap_or(0) > 127).unwrap_or(false);
                p[3] = if see_through { 0 } else { 255 };
            }
        }
        Ok(Img { w, h, px })
    });
    if !info.hbmColor.is_invalid() { let _ = DeleteObject(HGDIOBJ(info.hbmColor.0)); }
    if !info.hbmMask.is_invalid() { let _ = DeleteObject(HGDIOBJ(info.hbmMask.0)); }
    let _ = DestroyIcon(hicon);
    r
}

/// Raw 32-bit BGRA pixels of a bitmap, top-down.
unsafe fn raw_bgra(h: HBITMAP) -> Res<(u32, u32, Vec<u8>)> {
    if h.is_invalid() {
        return Err("No bitmap.".into());
    }
    let mut bm = BITMAP::default();
    if GetObjectW(HGDIOBJ(h.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut BITMAP as *mut c_void)) == 0 {
        return Err("Couldn't read the bitmap.".into());
    }
    let (w, ht) = (bm.bmWidth, bm.bmHeight.abs());
    if w <= 0 || ht <= 0 {
        return Err("Empty bitmap.".into());
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -ht,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: 0,
        ..Default::default()
    };
    let mut px = vec![0u8; (w * ht * 4) as usize];
    let dc = GetDC(None);
    let lines = GetDIBits(dc, h, 0, ht as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bmi, DIB_RGB_COLORS);
    ReleaseDC(None, dc);
    if lines == 0 {
        return Err("Couldn't read the bitmap pixels.".into());
    }
    Ok((w as u32, ht as u32, px))
}

unsafe fn thumbnail(path: &str, size: i32, icon: bool) -> Res<Vec<u8>> {
    if icon {
        let ext = std::path::Path::new(path).extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
        let key = match (&ext, own_icon(path)) {
            (Some(e), false) => Some(format!("{e}|{size}")),
            _ => None,
        };
        if let Some(k) = &key {
            if let Some(b) = type_icon_cache().lock().ok().and_then(|c| c.get(k).cloned()) {
                return Ok(b);
            }
        }
        let png = encode(&best_icon(path, size)?.0)?;
        if let (Some(k), Ok(mut c)) = (key, type_icon_cache().lock()) {
            c.insert(k, png.clone());
        }
        Ok(png)
    } else {
        encode(&grab(path, size, false)?)
    }
}

fn encode(img: &Img) -> Res<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.w, img.h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(&img.px).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

unsafe fn bitmap_rgba(h: HBITMAP) -> Res<Img> {
    let mut bm = BITMAP::default();
    if GetObjectW(HGDIOBJ(h.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut BITMAP as *mut c_void)) == 0 {
        return Err("Couldn't read the thumbnail.".into());
    }
    let (w, ht) = (bm.bmWidth, bm.bmHeight.abs());
    if w <= 0 || ht <= 0 {
        return Err("Empty thumbnail.".into());
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -ht, // top-down rows
        biPlanes: 1,
        biBitCount: 32,
        biCompression: 0, // BI_RGB
        ..Default::default()
    };
    let mut px = vec![0u8; (w * ht * 4) as usize];
    let dc = GetDC(None);
    let lines = GetDIBits(dc, h, 0, ht as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bmi, DIB_RGB_COLORS);
    ReleaseDC(None, dc);
    if lines == 0 {
        return Err("Couldn't read the thumbnail pixels.".into());
    }
    // BGRA (premultiplied) -> straight RGBA
    let has_alpha = px.chunks(4).any(|p| p[3] != 0);
    for p in px.chunks_mut(4) {
        p.swap(0, 2);
        if !has_alpha {
            p[3] = 255;
        } else if p[3] > 0 && p[3] < 255 {
            let a = p[3] as u32;
            for c in p.iter_mut().take(3) {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Ok(Img { w: w as u32, h: ht as u32, px })
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/* ---------- Shortcuts (.lnk) ---------- */

/// Returns the target path of a .lnk shortcut, or None when it points at something without a path.
pub fn resolve_link(path: &str) -> Res<Option<String>> {
    use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(msg)?;
        let file: IPersistFile = link.cast().map_err(msg)?;
        file.Load(&HSTRING::from(path), STGM_READ).map_err(msg)?;
        let mut buf = [0u16; 1024];
        link.GetPath(&mut buf, std::ptr::null_mut(), 0).map_err(msg)?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
        Ok(if end == 0 { None } else { Some(String::from_utf16_lossy(&buf[..end])) })
    }
}

/// Diagnostics: `files.exe --thumb-test <path> <size> <icon 0|1>` prints the PNG size or the error.
pub fn thumb_test(path: &str, size: i32, icon: bool) -> String {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if icon {
            return match best_icon(path, size) {
                Ok((img, log)) => {
                    if let (Ok(dir), Ok(b)) = (std::env::var("FILES_THUMB_DUMP"), encode(&img)) { let _ = std::fs::write(format!("{dir}/final.png"), b); }
                    format!("{log}=> {}x{}", img.w, img.h)
                }
                Err(e) => format!("error: {e}"),
            };
        }
        match thumbnail(path, size, icon) {
            Ok(b) => format!("ok {} bytes", b.len()),
            Err(e) => format!("error: {e}"),
        }
    }
}
