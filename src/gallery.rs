// Gallery: images Claude showed in chat (SendUserFile, or an image it Read), copied by the hook into
// %LOCALAPPDATA%\studiodeck\gallery as "<ms>_<name>" (so they survive screenshot clean-up), newest KEEP kept.
// Decoding is GDI+ on one worker thread; results come back as HBITMAPs (GDI objects are process-wide) + WM_GALLERY.
use std::{path::{Path, PathBuf}, ptr::null_mut, sync::mpsc};
use windows_sys::Win32::{Foundation::HWND, Graphics::{Gdi::HBITMAP, GdiPlus::{GdipCreateBitmapFromFile, GdipCreateHBITMAPFromBitmap, GdipDisposeImage, GdipGetImageHeight, GdipGetImageThumbnail, GdipGetImageWidth, GdiplusStartup, GdiplusStartupInput, GpBitmap, GpImage}}, UI::WindowsAndMessaging::PostMessageW};

pub const KEEP: usize = 200;
pub const WM_GALLERY: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 6;
const EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "bmp", "tif"];

pub fn dir() -> PathBuf { crate::deck_dir().join("gallery") }
pub fn is_image(p: &Path) -> bool { p.extension().is_some_and(|e| EXTS.contains(&e.to_string_lossy().to_lowercase().as_str())) }

// Newest first (names start with the copy time in ms).
pub fn list() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir()).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| is_image(p)).collect();
    v.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    v
}
// "1794281152795_deck.png" -> ("deck.png", ms)
pub fn parts(p: &Path) -> (String, u64) {
    let n = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    match n.split_once('_') { Some((ms, rest)) => (rest.to_string(), ms.parse().unwrap_or(0)), None => (n, 0) }
}

// Copies `src` in unless the same name + size is already there; prunes past KEEP.
pub fn add(src: &Path) -> Result<(), String> {
    if !is_image(src) { return Ok(()) }
    let len = std::fs::metadata(src).map_err(|e| e.to_string())?.len();
    let name = src.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let all = list();
    if all.iter().any(|p| parts(p).0 == name && std::fs::metadata(p).is_ok_and(|m| m.len() == len)) { return Ok(()) }
    std::fs::create_dir_all(dir()).map_err(|e| e.to_string())?;
    std::fs::copy(src, dir().join(format!("{}_{name}", crate::now_ms()))).map_err(|e| e.to_string())?;
    for old in all.iter().skip(KEEP - 1) { let _ = std::fs::remove_file(old); }
    Ok(())
}

pub struct Decoded { pub path: PathBuf, pub big: bool, pub bmp: usize, pub w: i32, pub h: i32 } // bmp: HBITMAP (not Send)
pub static DONE: std::sync::Mutex<Vec<Decoded>> = std::sync::Mutex::new(Vec::new());

// Worker: (path, fit box w x h in px, big) in; Decoded out (bmp null if it failed).
pub fn worker(deck: HWND) -> mpsc::Sender<(PathBuf, (u32, u32), bool)> {
    let (tx, rx) = mpsc::channel::<(PathBuf, (u32, u32), bool)>();
    let deck = deck as usize;
    std::thread::spawn(move || unsafe {
        let mut token = 0usize;
        let input = GdiplusStartupInput { GdiplusVersion: 1, DebugEventCallback: 0, SuppressBackgroundThread: 0, SuppressExternalCodecs: 0 };
        if GdiplusStartup(&mut token, &input, null_mut()) != 0 { return }
        for (path, fit, big) in rx {
            let (bmp, w, h) = decode(&path, fit).unwrap_or((null_mut(), 0, 0));
            DONE.lock().unwrap().push(Decoded { path, big, bmp: bmp as usize, w, h });
            PostMessageW(deck as HWND, WM_GALLERY, 0, 0);
        }
    });
    tx
}

unsafe fn decode(path: &Path, fit: (u32, u32)) -> Option<(HBITMAP, i32, i32)> {
    unsafe {
        let wide = crate::w(&path.to_string_lossy());
        let mut img: *mut GpBitmap = null_mut();
        if GdipCreateBitmapFromFile(wide.as_ptr(), &mut img) != 0 || img.is_null() { return None }
        let (mut w, mut h) = (0u32, 0u32);
        GdipGetImageWidth(img as *mut GpImage, &mut w);
        GdipGetImageHeight(img as *mut GpImage, &mut h);
        let s = (fit.0 as f64 / w.max(1) as f64).min(fit.1 as f64 / h.max(1) as f64).min(1.0);
        let (tw, th) = (((w as f64 * s) as u32).max(1), ((h as f64 * s) as u32).max(1));
        let mut src = img as *mut GpImage;
        if s < 1.0 {
            let mut t: *mut GpImage = null_mut();
            if GdipGetImageThumbnail(src, tw, th, &mut t, 0, null_mut()) == 0 && !t.is_null() { GdipDisposeImage(src); src = t }
        }
        let mut bmp: HBITMAP = null_mut();
        let ok = GdipCreateHBITMAPFromBitmap(src as *mut GpBitmap, &mut bmp, 0xFF1E1E21) == 0;
        GdipDisposeImage(src);
        ok.then_some((bmp, tw as i32, th as i32))
    }
}
