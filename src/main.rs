// Studio Deck (studiodeck.exe): one window with a live DWM thumbnail of every open Roblox Studio, its RAM, and the
// task/agent status Claude Code sessions report for it. Runs in the background, shows itself while
// 2+ Studios are open; tiles reorder by drag (eased, timer runs only while moving). Win32 + GDI; DWM composites thumbnails.
#![windows_subsystem = "windows"]

use std::{collections::HashMap, ffi::c_void, mem::zeroed, path::PathBuf, ptr::{null, null_mut}, time::{Instant, SystemTime}};
mod remote;
use windows_sys::core::BOOL;
const WM_MOUSELEAVE: u32 = 0x02A3;
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::{LibraryLoader::GetModuleHandleW, ProcessStatus::*, Registry::*, Threading::*},
    UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

const STALE_MS: u64 = 30 * 60 * 1000;
const DROP_MS: u64 = 2 * 60 * 60 * 1000;
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn w(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
const fn rgb(r: u8, g: u8, b: u8) -> u32 { r as u32 | (g as u32) << 8 | (b as u32) << 16 }
const BG: u32 = rgb(18, 18, 20);
const CARD: u32 = rgb(30, 30, 33);
const CARD_HOVER: u32 = rgb(42, 42, 46);
const TEXT: u32 = rgb(245, 245, 247);
const MUTED: u32 = rgb(152, 152, 157);
const DIM: u32 = rgb(88, 88, 92);
fn now_ms() -> u64 { SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

// ---------- status sources ----------

#[derive(Clone)]
struct Agent { name: String, status: String, step: String, studio: Option<String>, updated: Option<u64> }

fn status_dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("studiodeck").join("status")
}
fn parse_source(text: &str) -> Option<Vec<Agent>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let updated = v.get("updated").and_then(|u| u.as_u64());
    let s = |a: &serde_json::Value, k: &str| a.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    Some(v.get("agents")?.as_array()?.iter().map(|a| Agent {
        name: s(a, "name"),
        status: s(a, "status").to_lowercase(),
        step: s(a, "step"),
        studio: a.get("studio").and_then(|x| x.as_str()).map(|x| x.to_lowercase().trim_end_matches(".rbxl").to_string()).filter(|x| !x.is_empty()),
        updated,
    }).collect())
}

#[derive(Default)]
struct Status { cache: HashMap<PathBuf, (SystemTime, Option<Vec<Agent>>)>, agents: Vec<Agent>, has_info: bool }

impl Status {
    // Re-parses a file only when its modified time changed.
    fn refresh(&mut self) {
        let _ = std::fs::create_dir_all(status_dir());
        let mut paths: Vec<PathBuf> = std::fs::read_dir(status_dir()).into_iter().flatten().flatten()
            .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        let mut next = HashMap::new();
        for p in paths {
            let Ok(mt) = std::fs::metadata(&p).and_then(|m| m.modified()) else { continue };
            let parsed = match self.cache.remove(&p) {
                Some((t, a)) if t == mt => a,
                _ => std::fs::read_to_string(&p).ok().and_then(|t| parse_source(&t)),
            };
            next.insert(p, (mt, parsed));
        }
        self.cache = next;
        let now = now_ms();
        self.has_info = self.cache.values().any(|(_, a)| a.is_some());
        self.agents = self.cache.values().flat_map(|(_, a)| a.iter().flatten())
            .filter(|a| a.updated.is_none_or(|u| now.saturating_sub(u) < DROP_MS)).cloned().collect();
    }
    // Best agent for a tile: fresh before stale, then running > blocked > failed > anything else.
    fn for_tile(&self, place: &str, title: &str) -> Option<(&Agent, bool, usize)> {
        let (p, t, now) = (place.to_lowercase(), title.to_lowercase(), now_ms());
        let rank = |s: &str| match s { "running" => 0, "blocked" | "waiting" => 1, "failed" | "error" => 2, _ => 3 };
        let mut m: Vec<(&Agent, bool)> = self.agents.iter()
            .filter(|a| a.studio.as_ref().is_some_and(|s| p.contains(s.as_str()) || t.contains(s.as_str())))
            .map(|a| (a, a.updated.is_some_and(|u| now.saturating_sub(u) >= STALE_MS))).collect();
        m.sort_by_key(|(a, stale)| (*stale, rank(&a.status)));
        m.first().map(|&(a, s)| (a, s, m.len() - 1))
    }
}

fn dot_color(status: &str) -> u32 {
    match status {
        "running" => rgb(48, 209, 88),
        "blocked" | "waiting" => rgb(255, 159, 10),
        "failed" | "error" => rgb(255, 69, 58),
        _ => rgb(142, 142, 147),
    }
}

// --status <session> <project> <agents-json|->: atomic write, silent on success.
fn write_status(args: &[String]) -> Result<(), String> {
    let [session, project, agents] = args else { return Err("usage: --status <session> <project> <agents-json|->".into()) };
    let agents = if agents == "-" { std::io::read_to_string(std::io::stdin()).map_err(|e| e.to_string())? } else { agents.clone() };
    let agents: serde_json::Value = serde_json::from_str(agents.trim_start_matches('\u{feff}')).map_err(|e| format!("bad agents json: {e}"))?;
    if !agents.is_array() { return Err("agents must be a JSON array".into()) }
    let doc = serde_json::json!({ "session": session, "project": project, "updated": now_ms(), "agents": agents });
    let dir = status_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file: String = session.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let tmp = dir.join(format!("{file}.json.tmp"));
    std::fs::write(&tmp, doc.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(format!("{file}.json"))).map_err(|e| e.to_string())
}

fn install(on: bool) -> Result<(), String> {
    let (key, name) = (w(RUN_KEY), w("StudioDeck"));
    let rc = unsafe {
        if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let val = w(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, val.as_ptr() as _, (val.len() * 2) as u32)
        } else {
            match RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) { ERROR_FILE_NOT_FOUND => 0, r => r }
        }
    };
    if rc == 0 { Ok(()) } else { Err(format!("registry error {rc}")) }
}

// ---------- Studio windows ----------

// slot = target top-left of the card, cur = animated top-left, toff = thumbnail rect relative to the card.
struct Tile { src: HWND, thumb: isize, place: String, title: String, ram_gb: f64, minimized: bool, aspect: f64,
    slot: (f64, f64), cur: (f64, f64), placed: bool, lift: f64, toff: (f64, f64, f64, f64) }

fn studio_pid(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() { return false }
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        ok && String::from_utf16_lossy(&buf[..len as usize]).to_lowercase().ends_with("\\robloxstudiobeta.exe")
    }
}

fn working_set_gb(hwnd: HWND) -> f64 {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() { return 0.0 }
        let mut c: PROCESS_MEMORY_COUNTERS = zeroed();
        c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        GetProcessMemoryInfo(h, &mut c, c.cb);
        CloseHandle(h);
        c.WorkingSetSize as f64 / (1u64 << 30) as f64
    }
}

unsafe extern "system" fn enum_cb(h: HWND, lp: LPARAM) -> BOOL {
    unsafe {
        let out = &mut *(lp as *mut Vec<HWND>);
        if IsWindowVisible(h) != 0 && GetWindow(h, GW_OWNER).is_null() && GetWindowTextLengthW(h) > 0
            && GetWindowLongPtrW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW == 0
        {
            let mut pid = 0;
            GetWindowThreadProcessId(h, &mut pid);
            if studio_pid(pid) { out.push(h) }
        }
        1
    }
}

fn find_studios() -> Vec<HWND> {
    let mut v: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(enum_cb), &mut v as *mut _ as LPARAM) };
    v
}

fn window_title(h: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

// AUTO-NO: Studio's "<plugin> is not responding - Would you like Studio to stop this plugin?" message box is answered
// No in the background (WM_COMMAND IDNO to the dialog: no focus change), so a busy MCP/test plugin is never killed.
fn dialog_text(h: HWND) -> String {
    unsafe extern "system" fn cb(c: HWND, lp: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lp as *mut String) };
        out.push_str(&window_title(c));
        out.push(' ');
        1
    }
    let mut out = String::new();
    unsafe { EnumChildWindows(h, Some(cb), &mut out as *mut _ as LPARAM) };
    out
}
fn auto_no_hung_plugin(studios: &[HWND]) {
    let mut pids: Vec<u32> = Vec::new();
    for &h in studios {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(h, &mut pid) };
        pids.push(pid);
    }
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let v = unsafe { &mut *(lp as *mut Vec<HWND>) };
        let mut cls = [0u16; 16];
        let n = unsafe { GetClassNameW(h, cls.as_mut_ptr(), cls.len() as i32) };
        if String::from_utf16_lossy(&cls[..n.max(0) as usize]) == "#32770" && unsafe { IsWindowVisible(h) } != 0 { v.push(h) }
        1
    }
    let mut dialogs: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(cb), &mut dialogs as *mut _ as LPARAM) };
    for d in dialogs {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(d, &mut pid) };
        if !pids.contains(&pid) && !studio_pid(pid) { continue }
        let text = dialog_text(d).to_lowercase();
        if text.contains("is not responding") && text.contains("stop this plugin") {
            unsafe { PostMessageW(d, WM_COMMAND, IDNO as usize, 0) };
        }
    }
}
// "C:\...\MyPlace.rbxl - Roblox Studio" -> "MyPlace.rbxl"
fn place_name(title: &str) -> String {
    let t = title.trim_end_matches(" - Roblox Studio").trim_matches('*').trim();
    t.rsplit(['\\', '/']).next().unwrap_or(t).to_string()
}

// ---------- app ----------

struct Drag { src: HWND, down: (i32, i32), grab: (f64, f64), moved: bool }

struct App {
    hwnd: HWND, force: bool, dismissed: bool, visible: bool,
    tiles: Vec<Tile>, hover: Option<usize>, status: Status,
    dpi: u32, face: Vec<u16>, f_title: HFONT, f_body: HFONT,
    card: (f64, f64), slots: Vec<(f64, f64)>, drag: Option<Drag>, animating: bool, last_frame: Instant,
    settings: serde_json::Value,
}

thread_local! { static APP: std::cell::Cell<*mut App> = const { std::cell::Cell::new(null_mut()) }; }
fn app() -> &'static mut App { unsafe { &mut *APP.get() } }

const POLL_TIMER: usize = 1;
const ANIM_TIMER: usize = 2;
const EASE_TAU: f64 = 0.06; // seconds; exponential ease-out, settles in ~250 ms

impl App {
    fn px(&self, v: i32) -> i32 { v * self.dpi as i32 / 96 }
    fn pxf(&self, v: f64) -> f64 { v * self.dpi as f64 / 96.0 }

    fn make_fonts(&mut self) {
        unsafe {
            DeleteObject(self.f_title);
            DeleteObject(self.f_body);
            let (dpi, face) = (self.dpi as i32, self.face.as_ptr());
            let mk = |size: i32, weight: i32| CreateFontW(-(size * dpi / 96), 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, DEFAULT_PITCH as u32, face);
            self.f_title = mk(13, 600);
            self.f_body = mk(12, 400);
        }
    }

    fn dragging(&self) -> Option<HWND> { self.drag.as_ref().filter(|d| d.moved).map(|d| d.src) }

    fn tick(&mut self) {
        let found = find_studios();
        auto_no_hung_plugin(&found);
        let want = if self.force { true } else if found.len() < 2 { self.dismissed = false; false } else { !self.dismissed };
        if want != self.visible { self.set_visible(want) }
        if !self.visible { return }
        self.status.refresh();
        self.tiles.retain(|t| {
            let keep = found.contains(&t.src);
            if !keep { unsafe { DwmUnregisterThumbnail(t.thumb) }; }
            keep
        });
        if self.drag.as_ref().is_some_and(|d| !self.tiles.iter().any(|t| t.src == d.src)) { self.drag = None }
        for &h in &found {
            if self.tiles.iter().any(|t| t.src == h) { continue }
            let mut thumb = 0;
            if unsafe { DwmRegisterThumbnail(self.hwnd, h, &mut thumb) } == 0 {
                self.tiles.push(Tile { src: h, thumb, place: String::new(), title: String::new(), ram_gb: 0.0, minimized: false, aspect: 1.6,
                    slot: (0.0, 0.0), cur: (0.0, 0.0), placed: false, lift: 0.0, toff: (0.0, 0.0, 0.0, 0.0) });
            }
        }
        for t in &mut self.tiles {
            t.title = window_title(t.src);
            t.place = place_name(&t.title);
            t.ram_gb = working_set_gb(t.src);
            t.minimized = unsafe { IsIconic(t.src) } != 0;
            let mut rc: RECT = unsafe { zeroed() };
            unsafe { GetClientRect(t.src, &mut rc) };
            if rc.right > 0 && rc.bottom > 0 { t.aspect = rc.right as f64 / rc.bottom as f64 }
        }
        if self.drag.is_none() {
            // Saved order (by window title) first, then new windows by place name.
            let order: Vec<&str> = self.settings["order"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            let key = |t: &Tile| (order.iter().position(|o| *o == t.title).unwrap_or(usize::MAX), t.place.to_lowercase());
            self.tiles.sort_by_cached_key(key);
        }
        self.layout(false);
    }

    fn set_visible(&mut self, on: bool) {
        self.visible = on;
        unsafe {
            if on {
                ShowWindow(self.hwnd, if self.force { SW_SHOW } else { SW_SHOWNOACTIVATE });
            } else {
                self.save_settings();
                ShowWindow(self.hwnd, SW_HIDE);
                self.drag = None;
                for t in self.tiles.drain(..) { DwmUnregisterThumbnail(t.thumb); }
            }
        }
    }

    // Auto grid: try every column count, keep the one that gives the biggest thumbnails.
    // Sets each tile's slot; snap = jump there (resize), otherwise glide.
    fn layout(&mut self, snap: bool) {
        let mut cr: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut cr) };
        let n = self.tiles.len();
        if n == 0 { unsafe { InvalidateRect(self.hwnd, null(), 0) }; return }
        let (g, pad, cap) = (self.pxf(16.0), self.pxf(8.0), self.pxf(48.0));
        let (cw, ch) = (cr.right as f64, cr.bottom as f64);
        let aspect = self.tiles.iter().map(|t| t.aspect).sum::<f64>() / n as f64;
        let fit = |w: f64, h: f64, a: f64| { let w2 = w.min(h * a).max(0.0); (w2, w2 / a) };
        let cell = |cols: usize| {
            let rows = n.div_ceil(cols);
            let cell_w = (cw - g * (cols as f64 + 1.0)) / cols as f64;
            let cell_h = (ch - g * (rows as f64 + 1.0)) / rows as f64;
            fit(cell_w - 2.0 * pad, cell_h - 2.0 * pad - cap, aspect)
        };
        let cols = (1..=n).max_by(|&a, &b| { let (x, y) = (cell(a), cell(b)); (x.0 * x.1).total_cmp(&(y.0 * y.1)) }).unwrap_or(1);
        let rows = n.div_ceil(cols);
        let (bw, bh) = cell(cols);
        self.card = (bw + 2.0 * pad, bh + 2.0 * pad + cap);
        let (card_w, card_h) = self.card;
        let (ox, oy) = ((cw - (cols as f64 * card_w + (cols as f64 - 1.0) * g)) / 2.0, (ch - (rows as f64 * card_h + (rows as f64 - 1.0) * g)) / 2.0);
        self.slots = (0..n).map(|i| (ox + (i % cols) as f64 * (card_w + g), oy + (i / cols) as f64 * (card_h + g))).collect();
        let dragged = self.dragging();
        for (i, t) in self.tiles.iter_mut().enumerate() {
            t.slot = self.slots[i];
            if (snap || !t.placed) && Some(t.src) != dragged { t.cur = t.slot; t.placed = true }
            let (tw, th) = fit(bw, bh, t.aspect);
            t.toff = (pad + (bw - tw) / 2.0, pad + (bh - th) / 2.0, tw, th);
        }
        self.animate();
    }

    // Card rect of a tile at its animated position, scaled up while lifted.
    fn card_rect(&self, t: &Tile) -> (f64, f64, f64) {
        let s = 1.0 + 0.05 * t.lift;
        let (w, h) = self.card;
        (t.cur.0 - w * (s - 1.0) / 2.0, t.cur.1 - h * (s - 1.0) / 2.0, s)
    }
    fn rect(x: f64, y: f64, w: f64, h: f64) -> RECT { RECT { left: x.round() as i32, top: y.round() as i32, right: (x + w).round() as i32, bottom: (y + h).round() as i32 } }

    fn apply_thumbs(&self) {
        for t in &self.tiles {
            let (x, y, s) = self.card_rect(t);
            let p = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: Self::rect(x + t.toff.0 * s, y + t.toff.1 * s, t.toff.2 * s, t.toff.3 * s),
                rcSource: unsafe { zeroed() }, opacity: (255.0 - 30.0 * t.lift) as u8, fVisible: 1, fSourceClientAreaOnly: 1,
            };
            unsafe { DwmUpdateThumbnailProperties(t.thumb, &p) };
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    // Starts the 60 fps timer if anything still has to move; frame() stops it once everything has settled.
    fn animate(&mut self) {
        if !self.animating && self.unsettled() {
            self.animating = true;
            self.last_frame = Instant::now();
            unsafe { SetTimer(self.hwnd, ANIM_TIMER, 16, None) };
        }
        self.apply_thumbs();
    }
    fn unsettled(&self) -> bool {
        let dragged = self.dragging();
        self.drag.as_ref().is_some_and(|d| d.moved) || self.tiles.iter().any(|t| {
            let lt = if Some(t.src) == dragged { 1.0 } else { 0.0 };
            (t.cur.0 - t.slot.0).abs() > 0.5 || (t.cur.1 - t.slot.1).abs() > 0.5 || (t.lift - lt).abs() > 0.005
        })
    }
    fn frame(&mut self) {
        let now = Instant::now();
        let k = 1.0 - (-(now - self.last_frame).as_secs_f64().min(0.1) / EASE_TAU).exp();
        self.last_frame = now;
        let dragged = self.dragging();
        for t in &mut self.tiles {
            let lt = if Some(t.src) == dragged { 1.0 } else { 0.0 };
            t.lift += (lt - t.lift) * k;
            if Some(t.src) != dragged {
                t.cur.0 += (t.slot.0 - t.cur.0) * k;
                t.cur.1 += (t.slot.1 - t.cur.1) * k;
            }
        }
        if !self.unsettled() {
            for t in &mut self.tiles { if Some(t.src) != dragged { t.cur = t.slot; t.lift = 0.0 } }
            self.animating = false;
            unsafe { KillTimer(self.hwnd, ANIM_TIMER) };
        }
        self.apply_thumbs();
    }

    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let (x, y, (w, h)) = (x as f64, y as f64, self.card);
        self.tiles.iter().position(|t| x >= t.cur.0 && x < t.cur.0 + w && y >= t.cur.1 && y < t.cur.1 + h)
    }

    fn mouse_down(&mut self, x: i32, y: i32) {
        let Some(i) = self.hit(x, y) else { return };
        let t = &self.tiles[i];
        self.drag = Some(Drag { src: t.src, down: (x, y), grab: (x as f64 - t.cur.0, y as f64 - t.cur.1), moved: false });
        unsafe { SetCapture(self.hwnd) };
    }

    fn mouse_move(&mut self, x: i32, y: i32) {
        let thresh = self.px(4);
        let Some(d) = self.drag.as_mut() else { return };
        if !d.moved {
            if (x - d.down.0).abs() < thresh && (y - d.down.1).abs() < thresh { return }
            d.moved = true;
            // Re-register so the lifted thumbnail draws above the others.
            if let Some(t) = self.tiles.iter_mut().find(|t| t.src == d.src) {
                unsafe {
                    DwmUnregisterThumbnail(t.thumb);
                    DwmRegisterThumbnail(self.hwnd, t.src, &mut t.thumb);
                }
            }
        }
        let (src, grab) = (d.src, d.grab);
        let Some(i) = self.tiles.iter().position(|t| t.src == src) else { return };
        self.tiles[i].cur = (x as f64 - grab.0, y as f64 - grab.1);
        // Move the tile to the slot nearest its centre; the others glide into the remaining slots.
        let c = self.tiles[i].cur;
        let j = (0..self.slots.len()).min_by(|&a, &b| {
            let d = |s: (f64, f64)| (s.0 - c.0).powi(2) + (s.1 - c.1).powi(2);
            d(self.slots[a]).total_cmp(&d(self.slots[b]))
        }).unwrap_or(i);
        if j != i {
            let t = self.tiles.remove(i);
            self.tiles.insert(j, t);
            for (k, t) in self.tiles.iter_mut().enumerate() { t.slot = self.slots[k] }
        }
        self.hover = Some(j);
        self.animate();
    }

    fn mouse_up(&mut self, x: i32, y: i32) {
        let Some(d) = self.drag.take() else { return };
        unsafe { ReleaseCapture() };
        if d.moved {
            self.settings["order"] = self.tiles.iter().map(|t| serde_json::Value::from(t.title.clone())).collect();
            self.save_settings();
            self.animate();
        } else if let Some(i) = self.hit(x, y).filter(|&i| self.tiles[i].src == d.src) {
            let src = self.tiles[i].src;
            unsafe {
                if IsIconic(src) != 0 { ShowWindow(src, SW_RESTORE); }
                SetForegroundWindow(src);
            }
        }
    }

    fn save_settings(&mut self) {
        unsafe {
            let h = self.hwnd;
            if IsIconic(h) == 0 && IsZoomed(h) == 0 && IsWindowVisible(h) != 0 {
                let mut r: RECT = zeroed();
                GetWindowRect(h, &mut r);
                for (k, v) in [("x", r.left), ("y", r.top), ("w", r.right - r.left), ("h", r.bottom - r.top)] { self.settings[k] = v.into() }
            }
        }
        let _ = std::fs::write(settings_path(), self.settings.to_string());
    }

    unsafe fn paint(&self, dc: HDC) {
        unsafe {
            let mut cr: RECT = zeroed();
            GetClientRect(self.hwnd, &mut cr);
            let fill = |r: &RECT, c: u32| { let b = CreateSolidBrush(c); FillRect(dc, r, b); DeleteObject(b); };
            fill(&cr, BG);
            SetBkMode(dc, TRANSPARENT as _);
            let text = |s: &str, r: RECT, font: HFONT, color: u32, flags: u32| {
                let mut r = r;
                let s: Vec<u16> = s.encode_utf16().collect();
                SelectObject(dc, font);
                SetTextColor(dc, color);
                DrawTextW(dc, s.as_ptr(), s.len() as i32, &mut r, flags | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
            };
            if self.tiles.is_empty() {
                text("No Studio open", cr, self.f_body, MUTED, DT_CENTER | DT_VCENTER);
                return;
            }
            SelectObject(dc, GetStockObject(NULL_PEN));
            let round = |r: RECT, c: u32, rad: i32| {
                let b = CreateSolidBrush(c);
                let old = SelectObject(dc, b);
                RoundRect(dc, r.left, r.top, r.right + 1, r.bottom + 1, rad, rad);
                SelectObject(dc, old);
                DeleteObject(b);
            };
            let shade = |c: u32, f: f64| rgb(((c & 0xFF) as f64 * f) as u8, ((c >> 8 & 0xFF) as f64 * f) as u8, ((c >> 16 & 0xFF) as f64 * f) as u8);
            // Lifted tiles last, so they sit on top.
            let mut order: Vec<usize> = (0..self.tiles.len()).collect();
            order.sort_by(|&a, &b| self.tiles[a].lift.total_cmp(&self.tiles[b].lift));
            for i in order {
                let t = &self.tiles[i];
                let (x, y, s) = self.card_rect(t);
                let (w, h) = (self.card.0 * s, self.card.1 * s);
                let rad = self.px(14);
                if t.lift > 0.01 {
                    // Soft shadow: stacked round rects, darkest innermost.
                    for k in (0..6).rev() {
                        let e = self.pxf(3.0) * k as f64;
                        let dy = self.pxf(10.0) * t.lift;
                        round(Self::rect(x - e, y - e + dy, w + 2.0 * e, h + 2.0 * e), shade(BG, 1.0 - 0.55 * t.lift * (6 - k) as f64 / 6.0), rad + 2 * e as i32);
                    }
                }
                let card = Self::rect(x, y, w, h);
                round(card, if self.hover == Some(i) || t.lift > 0.01 { CARD_HOVER } else { CARD }, rad);
                if t.minimized { text("Minimised", Self::rect(x + t.toff.0 * s, y + t.toff.1 * s, t.toff.2 * s, t.toff.3 * s), self.f_body, DIM, DT_CENTER | DT_VCENTER) }
                // caption
                let (l, r) = (card.left + self.px(12), card.right - self.px(12));
                let y1 = card.bottom - self.px(44);
                let row1 = RECT { left: l, top: y1, right: r, bottom: y1 + self.px(20) };
                let row2 = RECT { left: l, top: y1 + self.px(20), right: r, bottom: y1 + self.px(38) };
                text(&format!("{:.1} GB", t.ram_gb), row1, self.f_body, MUTED, DT_RIGHT | DT_VCENTER);
                text(&t.place, RECT { right: r - self.px(64), ..row1 }, self.f_title, TEXT, DT_LEFT | DT_VCENTER);
                match self.status.for_tile(&t.place, &t.title) {
                    Some((a, stale, more)) => {
                        let (dc_col, tc) = if stale { (DIM, DIM) } else { (dot_color(&a.status), MUTED) };
                        text("\u{25CF}", row2, self.f_body, dc_col, DT_LEFT | DT_VCENTER);
                        let mut s = if a.step.is_empty() { a.name.clone() } else { format!("{}  \u{00B7}  {}", a.name, a.step) };
                        if more > 0 { s += &format!("  +{more}") }
                        if stale { s += "  \u{00B7}  stale" }
                        text(&s, RECT { left: l + self.px(16), ..row2 }, self.f_body, tc, DT_LEFT | DT_VCENTER);
                    }
                    None => text(if self.status.has_info { "Idle" } else { "No task info" }, row2, self.f_body, DIM, DT_LEFT | DT_VCENTER),
                }
            }
        }
    }
}

// ---------- settings (window rect + tile order) ----------

fn settings_path() -> PathBuf { PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("studiodeck.json") }

// Saved rect if it is still on a monitor, else 70% of the primary work area, centred.
fn initial_rect(v: &serde_json::Value) -> RECT {
    let g = |k: &str| v.get(k)?.as_i64().map(|x| x as i32);
    let saved = (|| Some(RECT { left: g("x")?, top: g("y")?, right: g("x")? + g("w")?, bottom: g("y")? + g("h")? }))()
        .filter(|r| r.right - r.left >= 200 && r.bottom - r.top >= 150 && unsafe { !MonitorFromRect(r, MONITOR_DEFAULTTONULL).is_null() });
    saved.unwrap_or_else(|| unsafe {
        let mut wa: RECT = zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as *mut c_void, 0);
        let (ww, wh) = (wa.right - wa.left, wa.bottom - wa.top);
        let (w, h) = (ww * 7 / 10, wh * 7 / 10);
        RECT { left: wa.left + (ww - w) / 2, top: wa.top + (wh - h) / 2, right: wa.left + (ww - w) / 2 + w, bottom: wa.top + (wh - h) / 2 + h }
    })
}

// ---------- window proc ----------

fn xy(lp: LPARAM) -> (i32, i32) { ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32) }

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if APP.get().is_null() { return DefWindowProcW(h, msg, wp, lp) }
        let a = app();
        match msg {
            WM_TIMER if wp == ANIM_TIMER => { a.frame(); 0 }
            WM_TIMER => { a.tick(); 0 }
            WM_SIZE => { a.layout(true); 0 }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(h, &mut ps);
                let mut cr: RECT = zeroed();
                GetClientRect(h, &mut cr);
                let mem = CreateCompatibleDC(dc);
                let bmp = CreateCompatibleBitmap(dc, cr.right.max(1), cr.bottom.max(1));
                let old = SelectObject(mem, bmp);
                a.paint(mem);
                BitBlt(dc, 0, 0, cr.right, cr.bottom, mem, 0, 0, SRCCOPY);
                SelectObject(mem, old);
                DeleteObject(bmp);
                DeleteDC(mem);
                EndPaint(h, &ps);
                0
            }
            WM_LBUTTONDOWN => { let (x, y) = xy(lp); a.mouse_down(x, y); 0 }
            WM_LBUTTONUP => { let (x, y) = xy(lp); a.mouse_up(x, y); 0 }
            WM_CAPTURECHANGED if a.drag.is_some() => { a.mouse_up(i32::MIN, i32::MIN); 0 }
            WM_MOUSEMOVE => {
                let (x, y) = xy(lp);
                if a.drag.is_some() { a.mouse_move(x, y); return 0 }
                let hit = a.hit(x, y);
                if hit != a.hover {
                    a.hover = hit;
                    let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                    TrackMouseEvent(&mut tme);
                    InvalidateRect(h, null(), 0);
                }
                0
            }
            WM_MOUSELEAVE => { if a.drag.is_none() { a.hover = None; InvalidateRect(h, null(), 0); } 0 }
            WM_SETCURSOR if a.hover.is_some() && (lp & 0xFFFF) as u32 == HTCLIENT => { SetCursor(LoadCursorW(null_mut(), IDC_HAND)); 1 }
            WM_DPICHANGED => {
                a.dpi = (wp >> 16) as u32 & 0xFFFF;
                a.make_fonts();
                let r = &*(lp as *const RECT);
                SetWindowPos(h, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                0
            }
            WM_CLOSE => {
                // Closing hides the deck until the Studio count drops below 2 again; with --show it quits.
                if a.force { a.save_settings(); DestroyWindow(h); } else { a.dismissed = true; a.set_visible(false); }
                0
            }
            WM_DESTROY => { PostQuitMessage(0); 0 }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

fn pick_face() -> Vec<u16> {
    for face in ["Segoe UI Variable Text", "Segoe UI"] {
        let wf = w(face);
        unsafe {
            let f = CreateFontW(-12, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0, 0, 0, wf.as_ptr());
            let dc = CreateCompatibleDC(null_mut());
            let old = SelectObject(dc, f);
            let mut buf = [0u16; 64];
            let n = GetTextFaceW(dc, buf.len() as i32, buf.as_mut_ptr());
            SelectObject(dc, old);
            DeleteDC(dc);
            DeleteObject(f);
            if n > 0 && String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1)]).eq_ignore_ascii_case(face) { return wf }
        }
    }
    w("Segoe UI")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cli = match args.get(1).map(String::as_str) {
        Some("--status") => Some(write_status(&args[2..])),
        Some("--install") => Some(install(true)),
        Some("--uninstall") => Some(install(false)),
        Some(c @ ("--shot" | "--click" | "--key" | "--scroll")) => Some(remote::run(c, &args[2..])),
        _ => None,
    };
    if let Some(r) = cli {
        if let Err(e) = r { eprintln!("studiodeck: {e}"); std::process::exit(1) }
        return;
    }
    let force = args.iter().any(|a| a == "--show");
    unsafe {
        let name = w("Local\\studiodeck-single-instance");
        let _mutex = CreateMutexW(null(), 0, name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS { return }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let inst = GetModuleHandleW(null());
        let class = w("StudioDeck");
        let icon = |size: i32| LoadImageW(inst, 1 as _, IMAGE_ICON, GetSystemMetrics(size), GetSystemMetrics(size), 0);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32, style: 0, lpfnWndProc: Some(wndproc), cbClsExtra: 0, cbWndExtra: 0, hInstance: inst,
            hIcon: icon(SM_CXICON), hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(), lpszMenuName: null(), lpszClassName: class.as_ptr(), hIconSm: icon(SM_CXSMICON),
        };
        RegisterClassExW(&wc);
        let settings = std::fs::read_to_string(settings_path()).ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).filter(|v| v.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
        let r = initial_rect(&settings);
        let title = w("Studio Deck");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW, r.left, r.top,
            r.right - r.left, r.bottom - r.top, null_mut(), null_mut(), inst, null());
        if hwnd.is_null() { return }
        // Dark title bar, rounded corners, Mica title bar (Win11; ignored on Win10).
        let set = |attr: i32, v: u32| DwmSetWindowAttribute(hwnd, attr as _, &v as *const u32 as *const c_void, 4);
        set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
        set(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND as u32);
        set(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_MAINWINDOW as u32);

        let boxed = Box::new(App {
            hwnd, force, dismissed: false, visible: false, tiles: Vec::new(), hover: None, status: Status::default(),
            dpi: GetDpiForWindow(hwnd).max(96), face: pick_face(), f_title: null_mut(), f_body: null_mut(),
            card: (0.0, 0.0), slots: Vec::new(), drag: None, animating: false, last_frame: Instant::now(), settings,
        });
        APP.set(Box::into_raw(boxed));
        app().make_fonts();
        app().tick();
        SetTimer(hwnd, POLL_TIMER, 1000, None);
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
