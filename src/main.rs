// Studio Deck (studiodeck.exe): one window with a live DWM thumbnail of every open Roblox Studio, its RAM, and the
// task/agent status Claude Code sessions report for it. Runs in the background, shows itself while
// 2+ Studios are open; tiles reorder by drag (eased, timer runs only while moving). Win32 + GDI; DWM composites thumbnails.
// v0.2: header (counts, RAM, pin = topmost, Pause all, update pill); hover action bar per tile (Save, Publish, Pause
// agents, Close) + tooltips in a click-through layered popup above the thumbnails; pause = control file agents poll
// via --check; "Waiting on" line + resolveOn; toasts; spring-physics drag paced by DwmFlush; self-update (update.rs).
#![windows_subsystem = "windows"]

use std::{collections::HashMap, ffi::c_void, mem::zeroed, path::PathBuf, ptr::{null, null_mut}, time::{Instant, SystemTime}};
mod remote;
mod update;
use windows_sys::core::BOOL;
const WM_MOUSELEAVE: u32 = 0x02A3;
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::{LibraryLoader::GetModuleHandleW, ProcessStatus::*, Registry::*, SystemInformation::*, Threading::*},
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
const AMBER: u32 = rgb(255, 159, 10);
const AMBER_BG: u32 = rgb(66, 46, 12);
fn now_ms() -> u64 { SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

// ---------- status sources ----------

#[derive(Clone)]
struct Agent { name: String, status: String, step: String, studio: Option<String>, updated: Option<u64>, waiting_on: String, since: u64,
    resolve_on: String, resolved: String, src: PathBuf, place_id: String, universe: String }
fn is_waiting(status: &str) -> bool { matches!(status, "waiting" | "blocked") }

// RESOLVE: a waiting entry with "resolveOn" clears itself: "published:<universeId>" when the public games API reports
// an `updated` time after the wait began (polled every 30 s, only while such an entry is shown and waiting);
// "file:<path>" when that file's mtime passes it. The status file entry is rewritten to "done" + "resolved".
static PUBLISHED: std::sync::Mutex<Vec<(String, u64, u64)>> = std::sync::Mutex::new(Vec::new()); // (universe, polled at, updated) ms
fn is_id(s: &str) -> bool { !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) }
// Public, unauthenticated Roblox APIs via curl.exe (ships with Windows 10+), no console window.
fn curl_json(url: &str) -> Option<serde_json::Value> {
    use std::os::windows::process::CommandExt;
    let o = std::process::Command::new("curl.exe").args(["-s", "-m", "10", url]).creation_flags(0x0800_0000).output().ok()?; // CREATE_NO_WINDOW
    serde_json::from_slice(&o.stdout).ok()
}
fn game_updated(universe: &str) -> Option<u64> {
    if !is_id(universe) { return None }
    parse_iso(curl_json(&format!("https://games.roblox.com/v1/games?universeIds={universe}"))?["data"][0]["updated"].as_str()?)
}
fn universe_of(place_id: &str) -> Option<String> {
    if !is_id(place_id) { return None }
    Some(curl_json(&format!("https://apis.roblox.com/universes/v1/places/{place_id}/universe"))?["universeId"].as_u64()?.to_string())
}
fn published_at(universe: &str) -> u64 {
    let now = now_ms();
    let mut v = PUBLISHED.lock().unwrap();
    let i = v.iter().position(|e| e.0 == universe).unwrap_or_else(|| { v.push((universe.to_string(), 0, 0)); v.len() - 1 });
    if now.saturating_sub(v[i].1) >= 30_000 && is_id(universe) {
        v[i].1 = now;
        let u = universe.to_string();
        std::thread::spawn(move || {
            if let Some(t) = game_updated(&u) && let Some(e) = PUBLISHED.lock().unwrap().iter_mut().find(|e| e.0 == u) { e.2 = t }
        });
    }
    v[i].2
}

// TOASTS: short confirmations on a tile's caption ("Saved ✓", "Published ✓ 14:02"). Background threads queue them here
// (src hwnd, text, colour) and wake the deck with WM_TOAST.
static TOASTS: std::sync::Mutex<Vec<(usize, String, u32)>> = std::sync::Mutex::new(Vec::new());
const WM_TOAST: u32 = WM_APP + 1;
const GREEN: u32 = rgb(48, 209, 88);
const RED: u32 = rgb(255, 69, 58);
fn queue_toast(deck: usize, src: usize, text: String, color: u32) {
    TOASTS.lock().unwrap().push((src, text, color));
    unsafe { PostMessageW(deck as HWND, WM_TOAST, 0, 0) };
}
// Publish confirmation: the experience's public `updated` time moves past the click (polled 5 s x 12).
fn confirm_publish(deck: usize, src: usize, universe: Option<String>, place_id: Option<String>, at: u64) {
    std::thread::spawn(move || {
        let Some(u) = universe.or_else(|| universe_of(place_id.as_deref()?)) else {
            return queue_toast(deck, src, "Publish sent \u{00B7} no place id to confirm".into(), AMBER);
        };
        for _ in 0..12 {
            std::thread::sleep(std::time::Duration::from_secs(5));
            if let Some(t) = game_updated(&u) && t + 2000 >= at {
                return queue_toast(deck, src, format!("Published \u{2713} {}", local_hhmm(t)), GREEN);
            }
        }
        queue_toast(deck, src, "Publish not confirmed".into(), RED)
    });
}
fn process_windows(pid: u32) -> Vec<HWND> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let (pid, v) = unsafe { &mut *(lp as *mut (u32, Vec<HWND>)) };
        let mut p = 0;
        unsafe { GetWindowThreadProcessId(h, &mut p) };
        if p == *pid && unsafe { IsWindowVisible(h) } != 0 { v.push(h) }
        1
    }
    let mut st = (pid, Vec::new());
    unsafe { EnumWindows(Some(cb), &mut st as *mut _ as LPARAM) };
    st.1
}
fn window_pid(h: HWND) -> u32 { let mut p = 0; unsafe { GetWindowThreadProcessId(h, &mut p) }; p }
// Titles of Studio's "publish as a NEW experience" flow; seen after a deck Publish -> cancelled at once.
const NEW_GAME: [&str; 6] = ["publish experience", "publish game", "publish as", "create new", "new experience", "save game"];
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let (era, mp) = (y.div_euclid(400), (m + 9) % 12);
    let yoe = y - era * 400;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * mp + 2) / 5 + d - 1 - 719468
}
// "2026-10-07T05:59:39.123Z" -> unix ms (UTC; the API always answers in Z)
fn parse_iso(s: &str) -> Option<u64> {
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let secs = days_from_civil(n(0, 4)?, n(5, 7)?, n(8, 10)?) * 86400 + n(11, 13)? * 3600 + n(14, 16)? * 60 + n(17, 19)?;
    let frac: String = s.get(19..).and_then(|f| f.strip_prefix('.')).unwrap_or("").chars().take_while(char::is_ascii_digit).take(3).collect();
    Some(secs as u64 * 1000 + format!("{frac:0<3}").parse::<u64>().ok()?)
}
fn local_hhmm(ms: u64) -> String {
    let t = unsafe { let mut t = zeroed(); GetLocalTime(&mut t); t };
    let st: SYSTEMTIME = t;
    let local = (days_from_civil(st.wYear as i64, st.wMonth as i64, st.wDay as i64) * 86400 + st.wHour as i64 * 3600 + st.wMinute as i64 * 60 + st.wSecond as i64) * 1000;
    let off = ((local - now_ms() as i64) as f64 / 60_000.0).round() as i64 * 60_000;
    let m = (ms as i64 + off).div_euclid(60_000).rem_euclid(1440);
    format!("{:02}:{:02}", m / 60, m % 60)
}
// Rewrites the waiting entries named `name` in a status file to done (atomic; agents read it back).
fn mark_done(path: &std::path::Path, name: &str, text: &str) {
    let Some(mut v) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(t.trim_start_matches('\u{feff}')).ok()) else { return };
    let Some(list) = v.get_mut("agents").and_then(|a| a.as_array_mut()) else { return };
    for a in list.iter_mut().filter(|a| a["name"].as_str() == Some(name) && is_waiting(&a["status"].as_str().unwrap_or("").to_lowercase())) {
        a["status"] = "done".into();
        a["resolved"] = text.into();
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() { let _ = std::fs::rename(&tmp, path); }
}

fn deck_dir() -> PathBuf { PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("studiodeck") }
fn status_dir() -> PathBuf { deck_dir().join("status") }

// PAUSE: %LOCALAPPDATA%\studiodeck\control\<place-key>.json {paused,by,at}; agents run `--check <title>` (exit 3 = paused).
// Place key = lowercased place name, ".rbxl" stripped, filename-unsafe chars -> '_'.
fn place_key(place: &str) -> String {
    let p = place.to_lowercase();
    p.trim().trim_end_matches(".rbxl").chars().map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c }).collect()
}
fn control_path(key: &str) -> PathBuf { deck_dir().join("control").join(format!("{key}.json")) }
fn is_paused(key: &str) -> bool {
    std::fs::read_to_string(control_path(key)).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(t.trim_start_matches('\u{feff}')).ok())
        .and_then(|v| v.get("paused")?.as_bool()).unwrap_or(false)
}
fn set_paused(key: &str, on: bool) -> Result<(), String> {
    let p = control_path(key);
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::json!({ "paused": on, "by": "user", "at": now_ms() }).to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}
// <title> substring -> key of the matching Studio window, else the argument itself as a place name.
fn key_for(title: &str) -> String {
    let t = title.to_lowercase();
    find_studios().into_iter().map(window_title).find(|w| w.to_lowercase().contains(&t))
        .map(|w| place_key(&place_name(&w))).unwrap_or_else(|| place_key(title))
}
fn log(what: &str, place: &str) {
    use std::io::Write;
    let t = unsafe { let mut t = zeroed(); GetLocalTime(&mut t); t };
    let line = format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}  {what}  {place}\n", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
    let _ = std::fs::create_dir_all(deck_dir());
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(deck_dir().join("log.txt")).and_then(|mut f| f.write_all(line.as_bytes()));
}

fn parse_source(text: &str) -> Option<Vec<Agent>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let updated = v.get("updated").and_then(|u| u.as_u64());
    let s = |a: &serde_json::Value, k: &str| a.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let id = |a: &serde_json::Value, k: &str| a.get(k).and_then(|x| x.as_u64().map(|n| n.to_string()).or_else(|| x.as_str().map(String::from))).unwrap_or_default();
    Some(v.get("agents")?.as_array()?.iter().map(|a| Agent {
        name: s(a, "name"),
        status: s(a, "status").to_lowercase(),
        step: s(a, "step"),
        studio: a.get("studio").and_then(|x| x.as_str()).map(|x| x.to_lowercase().trim_end_matches(".rbxl").to_string()).filter(|x| !x.is_empty()),
        updated,
        waiting_on: s(a, "waitingOn"),
        since: 0,
        resolve_on: s(a, "resolveOn"),
        resolved: s(a, "resolved"),
        src: PathBuf::new(),
        place_id: id(a, "placeId"),
        universe: id(a, "universeId"),
    }).collect())
}

// waiting: first time each (file, agent, studio) was seen waiting/blocked (its file's "updated" if the deck saw it late).
#[derive(Default)]
struct Status { cache: HashMap<PathBuf, (SystemTime, Option<Vec<Agent>>)>, agents: Vec<Agent>, has_info: bool, waiting: HashMap<String, u64> }

impl Status {
    // Re-parses a file only when its modified time changed.
    fn refresh(&mut self) {
        let _ = std::fs::create_dir_all(status_dir());
        let paths: Vec<PathBuf> = std::fs::read_dir(status_dir()).into_iter().flatten().flatten()
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
        let mut waiting = HashMap::new();
        self.agents.clear();
        for (p, (_, list)) in &self.cache {
            for a in list.iter().flatten().filter(|a| a.updated.is_none_or(|u| now.saturating_sub(u) < DROP_MS)) {
                let mut a = a.clone();
                a.src = p.clone();
                if is_waiting(&a.status) {
                    let k = format!("{}|{}|{:?}", p.display(), a.name, a.studio);
                    a.since = *self.waiting.get(&k).unwrap_or(&a.updated.unwrap_or(now));
                    waiting.insert(k, a.since);
                    let done = if let Some(u) = a.resolve_on.strip_prefix("published:") {
                        let t = published_at(u.trim());
                        (t > a.since).then(|| format!("Published \u{00B7} {}", local_hhmm(t)))
                    } else if let Some(f) = a.resolve_on.strip_prefix("file:") {
                        let t = std::fs::metadata(f.trim()).and_then(|m| m.modified()).ok()
                            .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64);
                        (t > a.since).then(|| format!("Done \u{00B7} {}", local_hhmm(t)))
                    } else { None };
                    if let Some(text) = done { Self::resolve(&mut a, text) }
                }
                self.agents.push(a);
            }
        }
        self.waiting = waiting;
    }
    fn resolve(a: &mut Agent, text: String) {
        mark_done(&a.src, &a.name, &text);
        (a.status, a.resolved) = ("done".into(), text);
    }
    fn matches(a: &Agent, place: &str, title: &str) -> bool {
        let (p, t) = (place.to_lowercase(), title.to_lowercase());
        a.studio.as_ref().is_some_and(|s| p.contains(s.as_str()) || t.contains(s.as_str()))
    }
    // The deck's own Publish button: resolve this instance's "published:" waits at once.
    fn resolve_published(&mut self, place: &str, title: &str) {
        let text = format!("Published \u{00B7} {}", local_hhmm(now_ms()));
        for a in self.agents.iter_mut().filter(|a| is_waiting(&a.status) && a.resolve_on.starts_with("published:") && Self::matches(a, place, title)) {
            Self::resolve(a, text.clone());
        }
    }
    // Best agent for a tile: fresh before stale, then running > blocked > failed > anything else.
    fn for_tile(&self, place: &str, title: &str) -> Option<(&Agent, bool, usize)> {
        let now = now_ms();
        let rank = |s: &str| match s { "running" => 0, "blocked" | "waiting" => 1, "failed" | "error" => 2, _ => 3 };
        let mut m: Vec<(&Agent, bool)> = self.agents.iter()
            .filter(|a| Self::matches(a, place, title))
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
    slot: (f64, f64), cur: (f64, f64), placed: bool, lift: f64, toff: (f64, f64, f64, f64), paused: bool,
    local: bool, toast: Option<(String, u32, u64)>,
    sx: Spring, sy: Spring, sc: Spring, due: Option<(Instant, (f64, f64))> } // cur/lift mirror the springs; due = staggered retarget

// Damped spring (unit mass), semi-implicit Euler. k stiffness, d damping = 2·ζ·√k.
#[derive(Clone, Copy, Default)]
struct Spring { pos: f64, vel: f64, target: f64, k: f64, d: f64 }
impl Spring {
    fn tune(&mut self, k: f64, zeta: f64) { (self.k, self.d) = (k, 2.0 * zeta * k.sqrt()) }
    fn step(&mut self, dt: f64) { self.vel += (-self.k * (self.pos - self.target) - self.d * self.vel) * dt; self.pos += self.vel * dt; }
    fn settled(&self, eps: f64) -> bool { (self.pos - self.target).abs() < eps && self.vel.abs() < eps * 10.0 }
    fn snap(&mut self, v: f64) { (self.pos, self.target, self.vel) = (v, v, 0.0) }
}
fn period_k(seconds: f64) -> f64 { (std::f64::consts::TAU / seconds).powi(2) }
const LIFT_SCALE: f64 = 1.06;
// iOS rubber band: past an edge, travel x maps to (1 - 1/(x·c/d + 1))·d.
fn rubber(v: f64, lo: f64, hi: f64, dim: f64) -> f64 {
    let band = |x: f64| (1.0 - 1.0 / (x * 0.55 / dim + 1.0)) * dim;
    if v < lo { lo - band(lo - v) } else if v > hi { hi + band(v - hi) } else { v }
}
// Local .rbxl file (title carries a path / extension): never published, so Publish would create a NEW experience.
fn is_local(title: &str) -> bool {
    let t = title.trim_end_matches(" - Roblox Studio").trim_matches('*').trim().to_lowercase();
    t.contains('\\') || t.contains('/') || t.ends_with(".rbxl") || t.ends_with(".rbxlx")
}
// A deck-initiated Save/Publish being watched for its outcome (WATCH_TIMER, 250 ms, only while non-empty).
struct Pending { src: HWND, act: Act, at: u64, before: Vec<HWND>, star: bool, file: Option<PathBuf> }

// Save / Publish: Qt ignores posted modifier chords, so (on the user's click) bring that Studio to the front and
// SendInput the chord. Nothing is typed unless the Studio really is the foreground window.
fn send_chord(src: HWND, modifier: u16, key: u16) -> bool {
    unsafe {
        if IsIconic(src) != 0 { ShowWindow(src, SW_RESTORE); }
        if SetForegroundWindow(src) == 0 {
            // foreground lock (deck not the foreground process): borrow the foreground thread's input state
            let (fg, me) = (GetWindowThreadProcessId(GetForegroundWindow(), null_mut()), GetCurrentThreadId());
            AttachThreadInput(me, fg, 1);
            SetForegroundWindow(src);
            AttachThreadInput(me, fg, 0);
        }
        for _ in 0..25 {
            if GetForegroundWindow() == src { break }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if GetForegroundWindow() != src { return false }
        let k = |vk: u16, up: bool| INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT {
            wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: 0 } } };
        let seq = [k(modifier, false), k(key, false), k(key, true), k(modifier, true)];
        SendInput(seq.len() as u32, seq.as_ptr(), size_of::<INPUT>() as i32) == seq.len() as u32
    }
}

fn process_path(pid: u32) -> String {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() { return String::new() }
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        if ok { String::from_utf16_lossy(&buf[..len as usize]) } else { String::new() }
    }
}
fn studio_pid(pid: u32) -> bool { process_path(pid).to_lowercase().ends_with("\\robloxstudiobeta.exe") }

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

// samples: recent raw (rubber-banded) targets, for the release velocity (last ~80 ms).
struct Drag { src: HWND, down: (i32, i32), grab: (f64, f64), moved: bool, samples: std::collections::VecDeque<(Instant, f64, f64)> }

#[derive(Clone, Copy, PartialEq)]
enum Act { Save, Publish, Pause, Close }
const BAR: [Act; 4] = [Act::Save, Act::Publish, Act::Pause, Act::Close];
// Hover bar of tile `tile` in deck client coords; confirm = it is showing "<act> <place>?" [Cancel] [OK].
struct Bar { tile: usize, pill: RECT, btns: Vec<RECT>, confirm: Option<Act>, label: String }
fn inside(r: &RECT, x: i32, y: i32) -> bool { x >= r.left && x < r.right && y >= r.top && y < r.bottom }

struct App {
    hwnd: HWND, force: bool, dismissed: bool, visible: bool,
    tiles: Vec<Tile>, hover: Option<usize>, status: Status,
    dpi: u32, face: Vec<u16>, icon_face: Vec<u16>, f_title: HFONT, f_body: HFONT, f_icon: HFONT,
    card: (f64, f64), slots: Vec<(f64, f64)>, drag: Option<Drag>, animating: bool, last_frame: Instant,
    settings: serde_json::Value,
    overlay: HWND, hot: Option<usize>, hdr: u8, confirm: Option<(HWND, Act)>, pc_mem: (f64, f64),
    pending: Vec<Pending>,
    update: Option<update::Release>, update_open: bool, update_msg: String,
    // TOOLTIPS: tip_want = (text, anchor) under the pointer; shown as `tip` (text, label rect) after 500 ms (TIP_TIMER).
    tip_want: Option<(String, RECT, (i32, i32))>, tip: Option<(String, RECT)>,
    // shown = the user opened it (--show, shortcut) or touched it (click, drag, resize): stays up until closed.
    shown: bool, back: (HBITMAP, i32, i32), trace: Option<std::fs::File>,
}
const WATCH_TIMER: usize = 3;
const TIP_TIMER: usize = 4;
const WM_SHOWDECK: u32 = WM_APP + 2;
const WM_FRAME: u32 = WM_APP + 3; // next animation frame (paced by DwmFlush)
const WM_UPDATE: u32 = WM_APP + 4; // UPD has a result
const WM_RESTART: u32 = WM_APP + 5; // `--update` swapped our exe: restart into it
// Ok(found release or None) from the check thread, Err(install result) from the install thread.
static UPD: std::sync::Mutex<Option<Result<Option<update::Release>, Result<(), String>>>> = std::sync::Mutex::new(None);
const HEADER: i32 = 36;
const PANEL: i32 = 100; // update panel height under the header row while open
// header hit codes
const H_PAUSE: u8 = 1;
const H_PIN: u8 = 2;
const H_UPD: u8 = 3;
const H_LATER: u8 = 4;
const H_DO: u8 = 5;

thread_local! { static APP: std::cell::Cell<*mut App> = const { std::cell::Cell::new(null_mut()) }; }
fn app() -> &'static mut App { unsafe { &mut *APP.get() } }

const POLL_TIMER: usize = 1;

impl App {
    fn px(&self, v: i32) -> i32 { v * self.dpi as i32 / 96 }
    fn pxf(&self, v: f64) -> f64 { v * self.dpi as f64 / 96.0 }

    fn make_fonts(&mut self) {
        unsafe {
            DeleteObject(self.f_title);
            DeleteObject(self.f_body);
            DeleteObject(self.f_icon);
            let dpi = self.dpi as i32;
            let mk = |size: i32, weight: i32, face: &[u16]| CreateFontW(-(size * dpi / 96), 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, DEFAULT_PITCH as u32, face.as_ptr());
            self.f_title = mk(13, 600, &self.face);
            self.f_body = mk(12, 400, &self.face);
            self.f_icon = mk(14, 400, &self.icon_face);
        }
    }

    fn dragging(&self) -> Option<HWND> { self.drag.as_ref().filter(|d| d.moved).map(|d| d.src) }

    fn tick(&mut self) {
        if !self.force && !background_enabled() { unsafe { PostQuitMessage(0) }; return }
        let found = find_studios();
        auto_no_hung_plugin(&found);
        let min = min_studios();
        // A window the user opened or touched stays until closed. An auto-shown one hides below `min` Studios,
        // unless an agent is waiting or a toast is up.
        let stay = self.visible && (self.status.agents.iter().any(|a| is_waiting(&a.status)) || self.tiles.iter().any(|t| t.toast.is_some()));
        let want = if self.force || self.shown { true } else if found.len() < min && !stay { self.dismissed = false; false } else { !self.dismissed };
        if want != self.visible { self.set_visible(want) }
        if !self.visible { return }
        let new_studio = found.iter().any(|h| !self.tiles.iter().any(|t| t.src == *h));
        self.apply_topmost(new_studio, &found);
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
                    slot: (0.0, 0.0), cur: (0.0, 0.0), placed: false, lift: 0.0, toff: (0.0, 0.0, 0.0, 0.0), paused: false, local: false, toast: None,
                    sx: Spring::default(), sy: Spring::default(), sc: Spring { pos: 1.0, target: 1.0, ..Default::default() }, due: None });
            }
        }
        unsafe {
            let mut m: MEMORYSTATUSEX = zeroed();
            m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
            GlobalMemoryStatusEx(&mut m);
            let gb = (1u64 << 30) as f64;
            self.pc_mem = ((m.ullTotalPhys - m.ullAvailPhys) as f64 / gb, m.ullTotalPhys as f64 / gb);
        }
        for t in &mut self.tiles {
            t.title = window_title(t.src);
            t.place = place_name(&t.title);
            t.paused = is_paused(&place_key(&t.place));
            t.local = is_local(&t.title);
            if t.toast.as_ref().is_some_and(|x| now_ms() > x.2) { t.toast = None }
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

    // TOPMOST (setting "topmost", default on): kept above Studio without taking focus; dropped while a Studio modal is
    // up (its main window disabled, or a #32770 box) so the dialog is never hidden, and while a chord is sent.
    fn topmost_setting(&self) -> bool { self.settings["topmost"].as_bool().unwrap_or(true) }
    fn apply_topmost(&mut self, reassert: bool, studios: &[HWND]) {
        let modal = studios.iter().any(|&h| unsafe { IsWindowEnabled(h) } == 0) || studios.iter().any(|&h| {
            process_windows(window_pid(h)).into_iter().any(|d| {
                let mut cls = [0u16; 16];
                let n = unsafe { GetClassNameW(d, cls.as_mut_ptr(), cls.len() as i32) };
                String::from_utf16_lossy(&cls[..n.max(0) as usize]) == "#32770"
            })
        });
        let want = self.topmost_setting() && !modal;
        if want != self.is_top() || (reassert && want) { self.set_top(want) }
    }
    // Read from the window itself: Studio's activation can clear the bit behind our back.
    fn is_top(&self) -> bool { unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0 } }
    fn set_top(&mut self, on: bool) {
        unsafe { SetWindowPos(self.hwnd, if on { HWND_TOPMOST } else { HWND_NOTOPMOST }, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
    }
    fn toggle_topmost(&mut self) {
        let on = !self.topmost_setting();
        self.settings["topmost"] = on.into();
        self.save_settings();
        self.set_top(on);
        unsafe { CheckMenuItem(GetSystemMenu(self.hwnd, 0), SC_TOPMOST as u32, if on { MF_CHECKED } else { MF_UNCHECKED }) };
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn set_visible(&mut self, on: bool) {
        self.visible = on;
        unsafe {
            if on {
                ShowWindow(self.hwnd, if self.force { SW_SHOW } else { SW_SHOWNOACTIVATE });
                let found = find_studios();
                self.apply_topmost(true, &found);
            } else {
                self.save_settings();
                ShowWindow(self.hwnd, SW_HIDE);
                ShowWindow(self.overlay, SW_HIDE);
                (self.drag, self.hover, self.hot, self.confirm) = (None, None, None, None);
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
        let (g, pad, cap) = (self.pxf(16.0), self.pxf(8.0), self.pxf(48.0 + 18.0 * self.extra_line() as i32 as f64));
        let top = self.pxf(self.header_h() as f64 - 8.0); // header replaces most of the top gutter
        let (cw, ch) = (cr.right as f64, cr.bottom as f64 - top);
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
        let (ox, oy) = ((cw - (cols as f64 * card_w + (cols as f64 - 1.0) * g)) / 2.0, top + (ch - (rows as f64 * card_h + (rows as f64 - 1.0) * g)) / 2.0);
        self.slots = (0..n).map(|i| (ox + (i % cols) as f64 * (card_w + g), oy + (i / cols) as f64 * (card_h + g))).collect();
        let dragged = self.dragging();
        for (i, t) in self.tiles.iter_mut().enumerate() {
            t.slot = self.slots[i];
            if Some(t.src) != dragged {
                if snap || !t.placed {
                    t.sx.snap(t.slot.0);
                    t.sy.snap(t.slot.1);
                    (t.cur, t.placed, t.due) = (t.slot, true, None);
                } else if t.due.is_none() && (t.sx.target, t.sy.target) != t.slot {
                    // a slot moved (tile added/removed/resized): glide there on the neighbour spring
                    for s in [&mut t.sx, &mut t.sy] { s.tune(period_k(0.3), 0.85) }
                    (t.sx.target, t.sy.target) = t.slot;
                }
            }
            let (tw, th) = fit(bw, bh, t.aspect);
            t.toff = (pad + (bw - tw) / 2.0, pad + (bh - th) / 2.0, tw, th);
        }
        self.animate();
    }

    // A tile's shown agent is waiting -> every caption gets a third line ("Waiting on: ...").
    fn extra_line(&self) -> bool {
        self.tiles.iter().any(|t| self.status.for_tile(&t.place, &t.title).is_some_and(|(a, stale, _)| !stale && is_waiting(&a.status)))
    }

    // Card rect of a tile at its animated position, scaled up while lifted.
    fn card_rect(&self, t: &Tile) -> (f64, f64, f64) {
        let s = t.sc.pos.max(0.5);
        let (w, h) = self.card;
        (t.cur.0 - w * (s - 1.0) / 2.0, t.cur.1 - h * (s - 1.0) / 2.0, s)
    }
    fn rect(x: f64, y: f64, w: f64, h: f64) -> RECT { RECT { left: x.round() as i32, top: y.round() as i32, right: (x + w).round() as i32, bottom: (y + h).round() as i32 } }

    // Pushes thumbnail rects/opacity to DWM; the lifted tile stays opaque and the others dim slightly.
    // dirty = only that client rect is repainted (animation frames), else the whole window.
    fn apply_thumbs_in(&self, dirty: Option<RECT>) {
        let top = self.tiles.iter().map(|t| t.lift).fold(0.0, f64::max).min(1.0);
        for t in &self.tiles {
            let (x, y, s) = self.card_rect(t);
            let dim = if t.lift + 0.001 >= top { 0.0 } else { 45.0 * top };
            let p = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: Self::rect(x + t.toff.0 * s, y + t.toff.1 * s, t.toff.2 * s, t.toff.3 * s),
                rcSource: unsafe { zeroed() }, opacity: ((if t.paused { 160.0 } else { 255.0 }) - dim) as u8, fVisible: 1, fSourceClientAreaOnly: 1,
            };
            unsafe { DwmUpdateThumbnailProperties(t.thumb, &p) };
        }
        unsafe { InvalidateRect(self.hwnd, dirty.as_ref().map_or(null(), |r| r as *const RECT), 0) };
        self.render_overlay();
    }
    fn apply_thumbs(&self) { self.apply_thumbs_in(None) }
    // Card rect incl. shadow, for dirty-rect repaints.
    fn card_bounds(&self, t: &Tile) -> RECT {
        let (x, y, s) = self.card_rect(t);
        let m = self.pxf(24.0);
        Self::rect(x - m, y - m, self.card.0 * s + 2.0 * m, self.card.1 * s + 2.0 * m + self.pxf(12.0))
    }

    fn text_w(&self, s: &str, f: HFONT) -> i32 {
        let s: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            let old = SelectObject(dc, f);
            let mut sz: SIZE = zeroed();
            GetTextExtentPoint32W(dc, s.as_ptr(), s.len() as i32, &mut sz);
            SelectObject(dc, old);
            DeleteDC(dc);
            sz.cx
        }
    }

    // Pill centred near the bottom of the hovered tile's thumbnail: 4 icon buttons, or a confirm prompt.
    fn bar(&self) -> Option<Bar> {
        if self.dragging().is_some() { return None }
        let i = self.hover?;
        let t = self.tiles.get(i)?;
        let (x, y, s) = self.card_rect(t);
        let cx = (x + (t.toff.0 + t.toff.2 / 2.0) * s).round() as i32;
        let bottom = (y + (t.toff.1 + t.toff.3) * s).round() as i32 - self.px(10);
        let (h, pad) = (self.px(36), self.px(3));
        let confirm = self.confirm.filter(|c| c.0 == t.src).map(|c| c.1);
        let name = t.place.trim_end_matches(".rbxl");
        let label = match confirm { Some(Act::Publish) => format!("Publish {name} to Roblox?"), Some(_) => format!("Close {name}?"), None => String::new() };
        let (lead, sizes, gap, inset) = match confirm {
            None => (pad, vec![h - 2 * pad; 4], self.px(2), pad),
            Some(a) => {
                let ok = if a == Act::Publish { "Publish" } else { "Close" };
                (self.px(16) + self.text_w(&label, self.f_body) + self.px(12),
                 vec![self.text_w("Cancel", self.f_title) + self.px(24), self.text_w(ok, self.f_title) + self.px(24)], self.px(4), self.px(5))
            }
        };
        let w = lead + sizes.iter().sum::<i32>() + gap * (sizes.len() as i32 - 1) + inset;
        let pill = RECT { left: cx - w / 2, top: bottom - h, right: cx - w / 2 + w, bottom };
        let mut bx = pill.left + lead;
        let btns = sizes.iter().map(|&sw| { let r = RECT { left: bx, top: pill.top + inset, right: bx + sw, bottom: pill.bottom - inset }; bx += sw + gap; r }).collect();
        Some(Bar { tile: i, pill, btns, confirm, label })
    }

    // The bar lives in an owned, click-through, per-pixel-alpha popup (DWM thumbnails cover the deck's own GDI).
    // Shapes are anti-aliased analytically; text/glyphs are GDI ClearType on the opaque fill, then alpha is applied.
    fn render_overlay(&self) {
        let bar = self.bar();
        let tip = self.tip.as_ref().filter(|_| self.dragging().is_none());
        if bar.is_none() && tip.is_none() { unsafe { ShowWindow(self.overlay, SW_HIDE) }; return }
        let union = |a: RECT, b: RECT| RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) };
        let area = match (&bar, tip) { (Some(b), Some(t)) => union(b.pill, t.1), (Some(b), None) => b.pill, (None, Some(t)) => t.1, _ => unreachable!() };
        let (w, h) = ((area.right - area.left).max(1), (area.bottom - area.top).max(1));
        let loc = |r: &RECT| RECT { left: r.left - area.left, top: r.top - area.top, right: r.right - area.left, bottom: r.bottom - area.top };
        let pill = bar.as_ref().map(|b| loc(&b.pill));
        let tip = tip.map(|(s, r)| (s.as_str(), loc(r)));
        let cov = |x: i32, y: i32, r: &RECT, rad: f64| {
            let (cx, cy) = ((r.left + r.right) as f64 / 2.0, (r.top + r.bottom) as f64 / 2.0);
            let qx = (x as f64 + 0.5 - cx).abs() - ((r.right - r.left) as f64 / 2.0 - rad);
            let qy = (y as f64 + 0.5 - cy).abs() - ((r.bottom - r.top) as f64 / 2.0 - rad);
            let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - rad;
            (0.5 - d).clamp(0.0, 1.0)
        };
        let argb = |c: u32| (c & 0xFF) << 16 | (c & 0xFF00) | (c >> 16 & 0xFF);
        unsafe {
            let mut bi: BITMAPINFO = zeroed();
            bi.bmiHeader = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, ..zeroed() };
            let mut bits = null_mut();
            let dc = CreateCompatibleDC(null_mut());
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bmp.is_null() { DeleteDC(dc); return }
            let old = SelectObject(dc, bmp);
            let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
            px.fill(argb(rgb(30, 30, 33)));
            let mut fill = |r: &RECT, c: u32| {
                let c = argb(c);
                let rad = (r.bottom - r.top) as f64 / 2.0;
                for y in r.top.max(0)..r.bottom.min(h) { for x in r.left.max(0)..r.right.min(w) {
                    let a = cov(x, y, r, rad);
                    if a <= 0.0 { continue }
                    let p = &mut px[(y * w + x) as usize];
                    let mix = |s: u32| (((*p >> s & 0xFF) as f64) * (1.0 - a) + ((c >> s & 0xFF) as f64) * a) as u32;
                    *p = mix(16) << 16 | mix(8) << 8 | mix(0);
                } }
            };
            let lighten = |c: u32| rgb(((c & 0xFF) + 24).min(255) as u8, ((c >> 8 & 0xFF) + 24).min(255) as u8, ((c >> 16 & 0xFF) + 24).min(255) as u8);
            if let Some(b) = &bar { let t = &self.tiles[b.tile]; match b.confirm {
                None => if let Some(k) = self.hot.filter(|&k| !(t.local && BAR[k] == Act::Publish)) { fill(&loc(&b.btns[k]), rgb(74, 74, 78)) },
                Some(a) => for (k, r) in b.btns.iter().enumerate() {
                    let c = if k == 0 { rgb(62, 62, 66) } else if a == Act::Close { rgb(255, 69, 58) } else { rgb(10, 132, 255) };
                    fill(&loc(r), if self.hot == Some(k) { lighten(c) } else { c });
                },
            } }
            SetBkMode(dc, TRANSPARENT as _);
            let text = |s: &str, r: RECT, f: HFONT, c: u32, flags: u32| {
                let mut r = r;
                let s: Vec<u16> = s.encode_utf16().collect();
                SelectObject(dc, f);
                SetTextColor(dc, c);
                DrawTextW(dc, s.as_ptr(), s.len() as i32, &mut r, flags | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER);
            };
            if let Some(b) = &bar { let t = &self.tiles[b.tile]; let pill = pill.unwrap(); match b.confirm {
                None => for (k, r) in b.btns.iter().enumerate() {
                    // Segoe MDL2 / Fluent: Save, Upload, Pause|Play, Cancel
                    let g = match BAR[k] { Act::Save => "\u{E74E}", Act::Publish => "\u{E898}", Act::Pause => if t.paused { "\u{E768}" } else { "\u{E769}" }, Act::Close => "\u{E711}" };
                    text(g, loc(r), self.f_icon, if t.local && BAR[k] == Act::Publish { DIM } else { TEXT }, DT_CENTER);
                },
                Some(a) => {
                    text(&b.label, RECT { left: pill.left + self.px(16), right: loc(&b.btns[0]).left, ..pill }, self.f_body, TEXT, DT_LEFT);
                    text("Cancel", loc(&b.btns[0]), self.f_title, TEXT, DT_CENTER);
                    text(if a == Act::Publish { "Publish" } else { "Close" }, loc(&b.btns[1]), self.f_title, rgb(255, 255, 255), DT_CENTER);
                }
            } }
            if let Some((s, r)) = tip { text(s, r, self.f_body, TEXT, DT_CENTER) }
            GdiFlush();
            for y in 0..h { for x in 0..w {
                let mut a = pill.map_or(0.0, |pill| cov(x, y, &pill, (pill.bottom - pill.top) as f64 / 2.0) * 0.88);
                if let Some((_, r)) = tip { a = a.max(cov(x, y, &r, self.pxf(7.0)) * 0.92) }
                let p = &mut px[(y * w + x) as usize];
                let m = |s: u32| (((*p >> s & 0xFF) as f64) * a) as u32;
                *p = ((a * 255.0) as u32) << 24 | m(16) << 16 | m(8) << 8 | m(0);
            } }
            let mut pos = POINT { x: area.left, y: area.top };
            ClientToScreen(self.hwnd, &mut pos);
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            UpdateLayeredWindow(self.overlay, null_mut(), &pos, &size, dc, &src, 0, &blend, ULW_ALPHA);
            SelectObject(dc, old);
            DeleteObject(bmp);
            DeleteDC(dc);
            ShowWindow(self.overlay, SW_SHOWNOACTIVATE);
        }
    }

    // Header row, right to left: [Pause all] [pin] [Update vX]; the update panel (when open) sits below it.
    fn header_h(&self) -> i32 { HEADER + if self.update_open { PANEL } else { 0 } }
    fn header_btn(&self) -> RECT {
        let mut cr: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut cr) };
        RECT { left: cr.right - self.px(16 + 92), top: self.px(8), right: cr.right - self.px(16), bottom: self.px(HEADER - 4) }
    }
    fn pin_btn(&self) -> RECT { let b = self.header_btn(); RECT { left: b.left - self.px(8 + 28), right: b.left - self.px(8), ..b } }
    fn update_label(&self) -> Option<String> { self.update.as_ref().map(|r| format!("Update {}", r.tag)) }
    fn update_btn(&self) -> Option<RECT> {
        let p = self.pin_btn();
        self.update_label().map(|l| RECT { left: p.left - self.px(8) - self.text_w(&l, self.f_body) - self.px(24), right: p.left - self.px(8), ..p })
    }
    fn panel_btns(&self) -> (RECT, RECT) {
        let b = self.header_btn();
        let (top, bottom) = (self.px(HEADER + PANEL - 44), self.px(HEADER + PANEL - 16));
        let doit = RECT { left: b.right - self.px(28 + 84), top, right: b.right - self.px(28), bottom };
        (RECT { left: doit.left - self.px(8 + 84), right: doit.left - self.px(8), ..doit }, doit)
    }
    fn header_hit(&self, x: i32, y: i32) -> u8 {
        let (later, doit) = self.panel_btns();
        if !self.tiles.is_empty() && inside(&self.header_btn(), x, y) { H_PAUSE }
        else if inside(&self.pin_btn(), x, y) { H_PIN }
        else if self.update_btn().is_some_and(|r| inside(&r, x, y)) { H_UPD }
        else if self.update_open && self.update_msg.is_empty() && inside(&later, x, y) { H_LATER }
        else if self.update_open && self.update_msg.is_empty() && inside(&doit, x, y) { H_DO }
        else { 0 }
    }
    fn all_paused(&self) -> bool { !self.tiles.is_empty() && self.tiles.iter().all(|t| t.paused) }

    // Hover state from a deck client point (the popup is click-through, so the deck gets all mouse input).
    fn update_hover(&mut self, x: i32, y: i32) {
        let hit = self.hit(x, y);
        let moved_tile = hit != self.hover;
        if moved_tile { self.hover = hit; self.confirm = None }
        let hot = self.bar().and_then(|b| b.btns.iter().position(|r| inside(r, x, y)));
        let hh = self.header_hit(x, y);
        unsafe {
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
        }
        let changed = moved_tile || hot != self.hot || hh != self.hdr;
        (self.hot, self.hdr) = (hot, hh);
        let want = if self.drag.is_some() { None } else { self.tip_target(x, y) };
        if want.as_ref().map(|w| &w.0) != self.tip_want.as_ref().map(|w| &w.0) {
            let had = self.tip.take().is_some();
            self.tip_want = want.map(|(s, r)| (s, r, (x, y)));
            unsafe { if self.tip_want.is_some() { SetTimer(self.hwnd, TIP_TIMER, 500, None); } else { KillTimer(self.hwnd, TIP_TIMER); } }
            if had && !changed { self.render_overlay() }
        }
        if changed {
            unsafe { InvalidateRect(self.hwnd, null(), 0) };
            self.render_overlay();
        }
    }
    fn hide_tip(&mut self) {
        unsafe { KillTimer(self.hwnd, TIP_TIMER) };
        self.tip_want = None;
        if self.tip.take().is_some() { self.render_overlay() }
    }
    // Small anchors get the label centred above them; big ones (a whole tile) get it under the pointer.
    fn show_tip(&mut self) {
        unsafe { KillTimer(self.hwnd, TIP_TIMER) };
        let Some((s, a, (x, y))) = self.tip_want.clone() else { return };
        if self.dragging().is_some() { return }
        let mut cr: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut cr) };
        let (w, h) = (self.text_w(&s, self.f_body) + self.px(20), self.px(24));
        let (cx, mut top) = if a.bottom - a.top <= self.px(60) { ((a.left + a.right) / 2, a.top - h - self.px(6)) } else { (x, y + self.px(22)) };
        if top < 0 { top = a.bottom + self.px(6) }
        let left = (cx - w / 2).clamp(self.px(4), (cr.right - w - self.px(4)).max(self.px(4)));
        self.tip = Some((s, RECT { left, top, right: left + w, bottom: top + h }));
        self.render_overlay();
    }
    fn caption_rows(&self, t: &Tile) -> [RECT; 3] {
        let (x, y, s) = self.card_rect(t);
        let card = Self::rect(x, y, self.card.0 * s, self.card.1 * s);
        let (l, r) = (card.left + self.px(12), card.right - self.px(12));
        let y1 = card.bottom - self.px(44 + self.extra_line() as i32 * 18);
        let row = |a: i32, b: i32| RECT { left: l, top: y1 + self.px(a), right: r, bottom: y1 + self.px(b) };
        [row(0, 20), row(20, 38), row(38, 56)]
    }
    fn tip_target(&self, x: i32, y: i32) -> Option<(String, RECT)> {
        let s = |t: &str| t.to_string();
        match self.hdr {
            H_PAUSE => return Some((s(if self.all_paused() { "Let all agents continue" } else { "Pause all agents" }), self.header_btn())),
            H_PIN => return Some((s("Keep Studio Deck on top"), self.pin_btn())),
            H_UPD => return self.update.as_ref().zip(self.update_btn()).map(|(r, b)| (format!("Update to {}", r.tag), b)),
            0 => {}
            _ => return None,
        }
        let ram = RECT { left: self.px(20), top: self.px(4), right: self.px(380), bottom: self.px(HEADER) };
        if inside(&ram, x, y) { return Some((s("Studio memory / PC memory in use"), ram)) }
        if let Some(b) = self.bar() {
            if b.confirm.is_some() { return None }
            if let Some(k) = self.hot {
                let t = &self.tiles[b.tile];
                let text = match BAR[k] {
                    Act::Save => "Save (to Roblox for published places)",
                    Act::Publish => if t.local { "Local file \u{2014} not a published place" } else { "Publish to Roblox \u{2014} updates the existing game" },
                    Act::Pause => if t.paused { "Let agents continue" } else { "Pause agents on this Studio" },
                    Act::Close => "Close this Studio",
                };
                return Some((s(text), b.btns[k]));
            }
            if inside(&b.pill, x, y) { return None }
        }
        let i = self.hover?;
        let t = &self.tiles[i];
        let [_, row2, row3] = self.caption_rows(t);
        if let Some((a, stale, _)) = self.status.for_tile(&t.place, &t.title) {
            let dot = RECT { right: row2.left + self.px(14), ..row2 };
            if inside(&dot, x, y) {
                let st = match a.status.as_str() {
                    "running" => s("Running"),
                    "waiting" | "blocked" if !a.waiting_on.is_empty() => format!("{} on {}", if a.status == "waiting" { "Waiting" } else { "Blocked" }, a.waiting_on),
                    "waiting" => s("Waiting"), "blocked" => s("Blocked"), "failed" | "error" => s("Failed"), "done" => s("Done"), o => o.to_string(),
                };
                return Some((if stale { format!("{st} (stale)") } else { st }, dot));
            }
            if !stale && is_waiting(&a.status) && inside(&row3, x, y) {
                return Some((if a.waiting_on.is_empty() { s("Waiting") } else { format!("Waiting on: {}", a.waiting_on) }, row3));
            }
        }
        let (cx, cy, sc) = self.card_rect(t);
        Some((s("Click to focus \u{00B7} drag to reorder"), Self::rect(cx, cy, self.card.0 * sc, self.card.1 * sc)))
    }

    fn press(&mut self, b: Bar, k: usize) {
        let t = &self.tiles[b.tile];
        let (src, place, title, paused, local) = (t.src, t.place.clone(), t.title.clone(), t.paused, t.local);
        // Watch starts before the keystroke so windows the chord opens are recognisably new.
        let pend = |act: Act| Pending { src, act, at: now_ms(), before: process_windows(window_pid(src)), star: title.contains('*'),
            file: if local && act == Act::Save { Some(PathBuf::from(title.trim_end_matches(" - Roblox Studio").trim_matches('*').trim())) } else { None } };
        // topmost is dropped while Studio takes the chord, then re-applied (apply_topmost also yields to Studio modals)
        let top = self.topmost_setting();
        if top { self.set_top(false) }
        let chord = |what: &str, m: VIRTUAL_KEY, key: u8| { let ok = send_chord(src, m, key as u16); log(if ok { what } else { "focus failed" }, &place); ok };
        match b.confirm {
            Some(act) => {
                self.confirm = None;
                if k == 1 {
                    if act == Act::Publish {
                        let p = pend(Act::Publish);
                        if chord("publish", VK_MENU, b'P') {
                            self.status.resolve_published(&place, &title);
                            let (u, pid) = self.ids_for(&place, &title);
                            confirm_publish(self.hwnd as usize, src as usize, u, pid, p.at);
                            self.watch_start(p);
                        }
                    } else { log("close", &place); unsafe { PostMessageW(src, WM_CLOSE, 0, 0) }; }
                }
            }
            None => match BAR[k] {
                Act::Save => { let p = pend(Act::Save); if chord("save", VK_CONTROL, b'S') { self.watch_start(p) } }
                Act::Pause => {
                    if set_paused(&place_key(&place), !paused).is_ok() { self.tiles[b.tile].paused = !paused }
                    log(if paused { "resume" } else { "pause" }, &place);
                }
                Act::Publish if local => {} // disabled: a local file has no experience to publish to
                a => self.confirm = Some((src, a)),
            },
        }
        if top { let f = find_studios(); self.apply_topmost(true, &f) }
        self.apply_thumbs();
    }

    // Universe / place id for a tile: status entries (universeId, placeId, resolveOn published:<u>), then the
    // settings map "places": { "<place-key>": <placeId> }.
    fn ids_for(&self, place: &str, title: &str) -> (Option<String>, Option<String>) {
        let mine: Vec<&Agent> = self.status.agents.iter().filter(|a| Status::matches(a, place, title)).collect();
        let u = mine.iter().find_map(|a| Some(a.universe.clone()).filter(|u| is_id(u))
            .or_else(|| a.resolve_on.strip_prefix("published:").map(|u| u.trim().to_string()).filter(|u| is_id(u))));
        let p = mine.iter().map(|a| a.place_id.clone()).find(|p| is_id(p))
            .or_else(|| self.settings["places"][place_key(place)].as_u64().map(|n| n.to_string()));
        (u, p)
    }

    fn watch_start(&mut self, p: Pending) {
        self.pending.push(p);
        unsafe { SetTimer(self.hwnd, WATCH_TIMER, 250, None) };
    }
    fn toast(&mut self, src: HWND, text: &str, color: u32) {
        log(text, &self.tiles.iter().find(|t| t.src == src).map(|t| t.place.clone()).unwrap_or_default());
        if let Some(t) = self.tiles.iter_mut().find(|t| t.src == src) { t.toast = Some((text.to_string(), color, now_ms() + 8000)) }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }
    // Save: confirmed by the file's mtime (local) or the title's unsaved "*" clearing (cloud).
    // Publish: a new window titled like the new-experience flow is escaped + closed at once.
    fn watch(&mut self) {
        let now = now_ms();
        let mut out: Vec<(HWND, &str, u32)> = Vec::new();
        self.pending.retain(|p| {
            let age = now.saturating_sub(p.at);
            match p.act {
                Act::Publish => {
                    for h in process_windows(window_pid(p.src)).into_iter().filter(|h| !p.before.contains(h)) {
                        let t = window_title(h).to_lowercase();
                        if NEW_GAME.iter().any(|k| t.contains(k)) {
                            unsafe {
                                PostMessageW(h, WM_KEYDOWN, VK_ESCAPE as usize, 0);
                                PostMessageW(h, WM_KEYUP, VK_ESCAPE as usize, 0);
                                PostMessageW(h, WM_CLOSE, 0, 0);
                            }
                            out.push((p.src, "Publish needs the File menu \u{2013} cancelled to avoid creating a new game", AMBER));
                            return false;
                        }
                    }
                    age < 10_000
                }
                _ => {
                    let saved = match &p.file {
                        Some(f) => std::fs::metadata(f).and_then(|m| m.modified()).ok().and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
                            .is_some_and(|d| d.as_millis() as u64 + 500 >= p.at),
                        None => p.star && !window_title(p.src).contains('*'),
                    };
                    if saved { out.push((p.src, "Saved \u{2713}", GREEN)); false }
                    else if p.file.is_none() && !p.star && age > 2000 { out.push((p.src, "Save sent", MUTED)); false }
                    else if age > 20_000 { out.push((p.src, "Save not confirmed", RED)); false }
                    else { true }
                }
            }
        });
        for (src, text, c) in out { self.toast(src, text, c) }
        if self.pending.is_empty() { unsafe { KillTimer(self.hwnd, WATCH_TIMER) }; }
    }

    // UPDATE: the check thread (start + every 6 h) and the install thread hand results over through UPD + WM_UPDATE.
    fn update_found(&mut self, r: Option<update::Release>) {
        let s = &self.settings["snooze"];
        let snoozed = |r: &update::Release| s["tag"].as_str() == Some(&r.tag) && s["until"].as_u64().is_some_and(|u| now_ms() < u);
        self.update = r.filter(|r| !snoozed(r));
        if self.update.is_none() { self.update_open = false }
        self.layout(false);
    }
    fn snooze_update(&mut self) {
        if let Some(r) = self.update.take() { self.settings["snooze"] = serde_json::json!({ "tag": r.tag, "until": now_ms() + 24 * 3600 * 1000 }) }
        self.update_open = false;
        self.save_settings();
        self.layout(false);
    }
    fn start_update(&mut self) {
        let Some(r) = self.update.clone() else { return };
        self.update_msg = "Downloading\u{2026}".into();
        let deck = self.hwnd as usize;
        std::thread::spawn(move || {
            let res = update::install(&r).map(|_| ());
            *UPD.lock().unwrap() = Some(Err(res));
            unsafe { PostMessageW(deck as HWND, WM_UPDATE, 0, 0) };
        });
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    fn pause_all(&mut self) {
        let on = !self.all_paused();
        for t in &mut self.tiles { if set_paused(&place_key(&t.place), on).is_ok() { t.paused = on } }
        log(if on { "pause all" } else { "resume all" }, "");
        self.apply_thumbs();
    }

    // Frame loop: WM_FRAME -> DwmFlush (waits for the next composition) -> frame() -> repost, only while a spring
    // moves or a staggered retarget is due; idle = no timer, no frames.
    fn animate(&mut self) {
        if !self.animating && self.unsettled() {
            self.animating = true;
            self.last_frame = Instant::now();
            unsafe { PostMessageW(self.hwnd, WM_FRAME, 0, 0) };
        }
        self.apply_thumbs();
    }
    fn unsettled(&self) -> bool {
        self.tiles.iter().any(|t| t.due.is_some() || !t.sx.settled(0.1) || !t.sy.settled(0.1) || !t.sc.settled(0.001))
    }
    fn frame(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f64().min(1.0 / 30.0);
        self.last_frame = now;
        let mut dirty: Option<RECT> = None;
        let mut grow = |r: RECT| dirty = Some(match dirty { None => r, Some(d) => RECT {
            left: d.left.min(r.left), top: d.top.min(r.top), right: d.right.max(r.right), bottom: d.bottom.max(r.bottom) } });
        for i in 0..self.tiles.len() {
            let before = self.card_bounds(&self.tiles[i]);
            let t = &mut self.tiles[i];
            if let Some((at, slot)) = t.due && now >= at {
                (t.sx.target, t.sy.target, t.due) = (slot.0, slot.1, None);
            }
            let moving = !(t.sx.settled(0.1) && t.sy.settled(0.1) && t.sc.settled(0.001));
            for (s, eps) in [(&mut t.sx, 0.1), (&mut t.sy, 0.1), (&mut t.sc, 0.001)] {
                s.step(dt);
                if s.settled(eps) { s.snap(s.target) }
            }
            t.cur = (t.sx.pos, t.sy.pos);
            t.lift = ((t.sc.pos - 1.0) / (LIFT_SCALE - 1.0)).clamp(0.0, 1.5);
            if moving {
                if let Some(f) = self.trace.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(f, "{},{},{:.2},{:.2},{:.4}", now_ms(), t.src as usize, t.sx.pos, t.sy.pos, t.sc.pos);
                }
                grow(before);
                grow(self.card_bounds(&self.tiles[i]));
            }
        }
        if !self.unsettled() { self.animating = false }
        self.apply_thumbs_in(Some(dirty.unwrap_or(unsafe { zeroed() })));
        unsafe {
            UpdateWindow(self.hwnd); // posted WM_FRAMEs outrank WM_PAINT; paint the dirty rect now
            if self.animating { PostMessageW(self.hwnd, WM_FRAME, 0, 0); }
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let (x, y, (w, h)) = (x as f64, y as f64, self.card);
        self.tiles.iter().position(|t| x >= t.cur.0 && x < t.cur.0 + w && y >= t.cur.1 && y < t.cur.1 + h)
    }

    fn mouse_down(&mut self, x: i32, y: i32) {
        self.update_hover(x, y);
        self.hide_tip();
        self.shown = true; // touched: stays open until closed
        match self.hdr {
            H_PAUSE => return self.pause_all(),
            H_PIN => return self.toggle_topmost(),
            H_UPD => { self.update_open = !self.update_open; self.update_msg.clear(); return self.layout(false) }
            H_LATER => return self.snooze_update(),
            H_DO => return self.start_update(),
            _ => {}
        }
        if let (Some(b), Some(k)) = (self.bar(), self.hot) { self.press(b, k); return }
        if self.confirm.take().is_some() { self.render_overlay() }
        let Some(i) = self.hit(x, y) else { return };
        let t = &self.tiles[i];
        self.drag = Some(Drag { src: t.src, down: (x, y), grab: (x as f64 - t.cur.0, y as f64 - t.cur.1), moved: false, samples: Default::default() });
        unsafe { SetCapture(self.hwnd) };
    }

    // Lifted tile: scale springs to LIFT_SCALE (~0.25 s), position follows the (rubber-banded) pointer through a
    // stiff critically-damped spring; neighbours retarget with a small stagger.
    fn mouse_move(&mut self, x: i32, y: i32) {
        let thresh = self.px(4);
        let mut cr: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut cr) };
        let (cw, chh, card, top) = (cr.right as f64, cr.bottom as f64, self.card, self.pxf(self.header_h() as f64 - 8.0));
        let Some(d) = self.drag.as_mut() else { return };
        let Some(i) = self.tiles.iter().position(|t| t.src == d.src) else { return };
        if !d.moved {
            if (x - d.down.0).abs() < thresh && (y - d.down.1).abs() < thresh { return }
            d.moved = true;
            self.shown = true; // touched: stays open
            let t = &mut self.tiles[i];
            // Re-register so the lifted thumbnail draws above the others.
            unsafe {
                DwmUnregisterThumbnail(t.thumb);
                DwmRegisterThumbnail(self.hwnd, t.src, &mut t.thumb);
            }
            t.sc.tune(period_k(0.25), 0.8);
            t.sc.target = LIFT_SCALE;
            for s in [&mut t.sx, &mut t.sy] { s.tune(700.0, 0.9) }
        }
        let raw = (rubber(x as f64 - d.grab.0, 0.0, cw - card.0, cw), rubber(y as f64 - d.grab.1, top, chh - card.1, chh));
        let now = Instant::now();
        d.samples.push_back((now, raw.0, raw.1));
        while d.samples.front().is_some_and(|s| now - s.0 > std::time::Duration::from_millis(80)) { d.samples.pop_front(); }
        (self.tiles[i].sx.target, self.tiles[i].sy.target) = raw;
        // Move the tile to the slot nearest the pointer-driven position; the others make room.
        let j = (0..self.slots.len()).min_by(|&a, &b| {
            let d = |s: (f64, f64)| (s.0 - raw.0).powi(2) + (s.1 - raw.1).powi(2);
            d(self.slots[a]).total_cmp(&d(self.slots[b]))
        }).unwrap_or(i);
        if j != i {
            let t = self.tiles.remove(i);
            self.tiles.insert(j, t);
            for (k, t) in self.tiles.iter_mut().enumerate() {
                if k == j || t.slot == self.slots[k] { continue }
                t.slot = self.slots[k];
                for s in [&mut t.sx, &mut t.sy] { s.tune(period_k(0.3), 0.85) }
                t.due = Some((now + std::time::Duration::from_millis(14 * k.abs_diff(j) as u64), t.slot));
            }
            self.tiles[j].slot = self.slots[j];
        }
        self.hover = Some(j);
        self.animate();
    }

    fn mouse_up(&mut self, x: i32, y: i32) {
        let Some(d) = self.drag.take() else { return };
        unsafe { ReleaseCapture() };
        if d.moved {
            // Fly home carrying the pointer's velocity (last ~80 ms), with a slight bounce.
            let v = match (d.samples.front(), d.samples.back()) {
                (Some(a), Some(b)) if (b.0 - a.0).as_secs_f64() > 0.01 && Instant::now() - b.0 < std::time::Duration::from_millis(80) => {
                    let dt = (b.0 - a.0).as_secs_f64();
                    ((b.1 - a.1) / dt, (b.2 - a.2) / dt)
                }
                _ => (0.0, 0.0),
            };
            if let Some(t) = self.tiles.iter_mut().find(|t| t.src == d.src) {
                let cap = |v: f64| v.clamp(-5000.0, 5000.0);
                for (s, target, vel) in [(&mut t.sx, t.slot.0, v.0), (&mut t.sy, t.slot.1, v.1)] {
                    s.tune(period_k(0.35), 0.78);
                    (s.target, s.vel) = (target, cap(vel));
                }
                t.sc.tune(period_k(0.35), 0.78);
                t.sc.target = 1.0;
            }
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
        self.settings["background"] = background_enabled().into(); // (toggled elsewhere: never overwrite it)
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
            SelectObject(dc, GetStockObject(NULL_PEN));
            let round = |r: RECT, c: u32, rad: i32| {
                let b = CreateSolidBrush(c);
                let old = SelectObject(dc, b);
                RoundRect(dc, r.left, r.top, r.right + 1, r.bottom + 1, rad, rad);
                SelectObject(dc, old);
                DeleteObject(b);
            };
            // Right-aligned pill badge ending at `right`; returns its left edge.
            let badge = |s: &str, right: i32, row: RECT, fg: u32, bg: u32| {
                let bw = self.text_w(s, self.f_body) + self.px(14);
                let (bh, mid) = (self.px(18), (row.top + row.bottom) / 2);
                let r = RECT { left: right - bw, top: mid - bh / 2, right, bottom: mid - bh / 2 + bh };
                round(r, bg, bh);
                text(s, r, self.f_body, fg, DT_CENTER | DT_VCENTER);
                r.left
            };
            // header: counts + RAM, Pause all / Resume all
            let hb = self.header_btn();
            let n = self.tiles.len();
            let studio_ram: f64 = self.tiles.iter().map(|t| t.ram_gb).sum();
            let summary = format!("{n} Studio{}   \u{00B7}   {studio_ram:.1} GB Studio   \u{00B7}   PC {:.1} / {:.0} GB",
                if n == 1 { "" } else { "s" }, self.pc_mem.0, self.pc_mem.1);
            let pin = self.pin_btn();
            let ub = self.update_btn();
            text(&summary, RECT { left: self.px(20), top: self.px(4), right: ub.map_or(pin.left, |u| u.left) - self.px(8), bottom: self.px(HEADER) }, self.f_body, MUTED, DT_LEFT | DT_VCENTER);
            if n > 0 {
                round(hb, if self.hdr == H_PAUSE { CARD_HOVER } else { CARD }, hb.bottom - hb.top);
                text(if self.all_paused() { "Resume all" } else { "Pause all" }, hb, self.f_body, TEXT, DT_CENTER | DT_VCENTER);
            }
            // pin: Segoe MDL2/Fluent "Pinned" (on) / "Pin" (off)
            if self.hdr == H_PIN { round(pin, CARD_HOVER, pin.bottom - pin.top) }
            let on = self.topmost_setting();
            text(if on { "\u{E840}" } else { "\u{E718}" }, pin, self.f_icon, if on { TEXT } else { DIM }, DT_CENTER | DT_VCENTER);
            if let (Some(u), Some(l)) = (ub, self.update_label()) {
                round(u, if self.hdr == H_UPD { rgb(20, 92, 170) } else { rgb(14, 70, 132) }, u.bottom - u.top);
                text(&l, u, self.f_body, TEXT, DT_CENTER | DT_VCENTER);
            }
            if self.update_open && let Some(r) = &self.update {
                let p = RECT { left: self.px(16), top: self.px(HEADER), right: hb.right, bottom: self.px(HEADER + PANEL - 8) };
                round(p, CARD, self.px(14));
                let line = |i: i32, s: &str, f: HFONT, c: u32| text(s, RECT { left: p.left + self.px(16), top: p.top + self.px(10 + 18 * i), right: p.right - self.px(220), bottom: p.top + self.px(28 + 18 * i) }, f, c, DT_LEFT | DT_VCENTER);
                line(0, &format!("Studio Deck {}  \u{00B7}  {}", r.tag, r.title), self.f_title, TEXT);
                for (i, l) in r.notes.iter().enumerate() { line(1 + i as i32, l, self.f_body, MUTED) }
                let (later, doit) = self.panel_btns();
                if self.update_msg.is_empty() {
                    round(later, if self.hdr == H_LATER { CARD_HOVER } else { rgb(44, 44, 48) }, later.bottom - later.top);
                    text("Later", later, self.f_body, TEXT, DT_CENTER | DT_VCENTER);
                    round(doit, if self.hdr == H_DO { rgb(40, 150, 255) } else { rgb(10, 132, 255) }, doit.bottom - doit.top);
                    text("Update", doit, self.f_title, rgb(255, 255, 255), DT_CENTER | DT_VCENTER);
                } else {
                    text(&self.update_msg, RECT { left: later.left - self.px(160), ..doit }, self.f_body, MUTED, DT_RIGHT | DT_VCENTER);
                }
            }
            if self.tiles.is_empty() {
                text("No Studios open", cr, self.f_body, MUTED, DT_CENTER | DT_VCENTER);
                return;
            }
            let extra = self.extra_line() as i32 * 18;
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
                let y1 = card.bottom - self.px(44 + extra);
                let row1 = RECT { left: l, top: y1, right: r, bottom: y1 + self.px(20) };
                let row2 = RECT { left: l, top: y1 + self.px(20), right: r, bottom: y1 + self.px(38) };
                let row3 = RECT { left: l, top: y1 + self.px(38), right: r, bottom: y1 + self.px(56) };
                text(&format!("{:.1} GB", t.ram_gb), row1, self.f_body, MUTED, DT_RIGHT | DT_VCENTER);
                let mut name_right = r - self.px(56);
                if t.paused { name_right = badge("Paused", name_right, row1, AMBER, AMBER_BG) - self.px(8) }
                if let Some((s, c, _)) = &t.toast { name_right = badge(s, name_right, row1, *c, shade(*c, 0.25)) - self.px(8) }
                let name_right = name_right.min(r - self.px(64));
                text(&t.place, RECT { right: name_right, ..row1 }, self.f_title, TEXT, DT_LEFT | DT_VCENTER);
                match self.status.for_tile(&t.place, &t.title) {
                    Some((a, stale, more)) => {
                        let (mut dc_col, tc) = if stale { (DIM, DIM) } else { (dot_color(&a.status), MUTED) };
                        if t.paused { dc_col = AMBER }
                        let resolved = !stale && !a.resolved.is_empty();
                        if resolved { text("\u{2713}", row2, self.f_title, dot_color("running"), DT_LEFT | DT_VCENTER) }
                        else { text("\u{25CF}", row2, self.f_body, dc_col, DT_LEFT | DT_VCENTER) }
                        let mut s = if resolved { format!("{}  \u{00B7}  {}", a.name, a.resolved) }
                            else if a.step.is_empty() { a.name.clone() } else { format!("{}  \u{00B7}  {}", a.name, a.step) };
                        if more > 0 { s += &format!("  +{more}") }
                        if stale { s += "  \u{00B7}  stale" }
                        let mut right = r;
                        if !stale && is_waiting(&a.status) {
                            // only the user can unblock "user..." waits: brighter badge
                            let user = a.waiting_on.trim_start().to_lowercase().starts_with("user");
                            let (fg, bg) = if user { (rgb(255, 214, 102), rgb(122, 82, 14)) } else { (AMBER, AMBER_BG) };
                            right = badge("Waiting", r, row2, fg, bg) - self.px(8);
                            let mins = now_ms().saturating_sub(a.since) / 60_000;
                            let dur = if mins < 60 { format!("{mins}m") } else { format!("{}h {}m", mins / 60, mins % 60) };
                            let why = if a.waiting_on.is_empty() { format!("Waiting  \u{00B7}  {dur}") } else { format!("Waiting on: {}  \u{00B7}  {dur}", a.waiting_on) };
                            text(&why, RECT { left: l + self.px(16), ..row3 }, self.f_body, if user { fg } else { MUTED }, DT_LEFT | DT_VCENTER);
                        }
                        text(&s, RECT { left: l + self.px(16), right, ..row2 }, self.f_body, tc, DT_LEFT | DT_VCENTER);
                    }
                    None => text(if self.status.has_info { "Idle" } else { "No task info" }, row2, self.f_body, DIM, DT_LEFT | DT_VCENTER),
                }
            }
        }
    }
}

// ---------- settings (window rect + tile order) ----------

// DEV mode (env STUDIODECK_DEV set): own settings file, mutex and window class, so a dev build runs beside the real one.
fn dev() -> bool { std::env::var_os("STUDIODECK_DEV").is_some() }
fn deck_class() -> Vec<u16> { w(if dev() { "StudioDeckDev" } else { "StudioDeck" }) }
fn settings_path() -> PathBuf { PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join(if dev() { "studiodeck-dev.json" } else { "studiodeck.json" }) }
fn read_settings() -> serde_json::Value {
    std::fs::read_to_string(settings_path()).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v.is_object()).unwrap_or_else(|| serde_json::json!({}))
}
// BACKGROUND toggle (default OFF): when off, studiodeck.exe without --show exits at once and a running background
// instance quits within a second - nothing resident, no tray. On: `--background on`, the window's system menu, or --install.
fn background_enabled() -> bool { read_settings().get("background").and_then(|b| b.as_bool()).unwrap_or(false) }
fn set_background(on: bool) -> Result<(), String> {
    let mut v = read_settings();
    v["background"] = on.into();
    std::fs::write(settings_path(), v.to_string()).map_err(|e| e.to_string())
}
const SC_BACKGROUND: usize = 0x1010;
const SC_TOPMOST: usize = 0x1020;
// background mode shows the deck while at least this many Studios are open (setting "showWhen", default 2)
fn min_studios() -> usize { read_settings().get("showWhen").and_then(|n| n.as_u64()).map(|n| n.clamp(1, 9) as usize).unwrap_or(2) }
fn set_show_when(arg: Option<&String>) -> Result<(), String> {
    let n: u64 = arg.and_then(|a| a.parse().ok()).filter(|n| (1..=9).contains(n)).ok_or("usage: --show-when <1-9>")?;
    let mut v = read_settings();
    v["showWhen"] = n.into();
    std::fs::write(settings_path(), v.to_string()).map_err(|e| e.to_string())
}

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
            WM_FRAME => { DwmFlush(); a.frame(); 0 }
            WM_UPDATE => {
                match UPD.lock().unwrap().take() {
                    Some(Ok(r)) => a.update_found(r),
                    Some(Err(Ok(()))) => { log("updated", ""); a.save_settings(); update::restart() }
                    Some(Err(Err(e))) => { log(&format!("update failed: {e}"), ""); a.update_msg = format!("Update failed: {e}"); InvalidateRect(h, null(), 0); }
                    None => {}
                }
                0
            }
            WM_RESTART => { a.save_settings(); update::restart() }
            WM_EXITSIZEMOVE => { a.shown = true; 0 } // moved/resized by the user: stays open
            WM_SYSCOMMAND if (wp & 0xFFF0) == SC_TOPMOST => { a.toggle_topmost(); 0 }
            WM_SYSCOMMAND if !matches!((wp & 0xFFF0) as u32, SC_CLOSE | SC_KEYMENU | SC_MOUSEMENU) && (wp & 0xFFF0) != SC_BACKGROUND => {
                a.shown = true; // maximise / restore / minimise / size / move
                DefWindowProcW(h, msg, wp, lp)
            }
            WM_TIMER if wp == WATCH_TIMER => { a.watch(); 0 }
            WM_TIMER if wp == TIP_TIMER => { a.show_tip(); 0 }
            WM_TOAST => {
                let q: Vec<_> = TOASTS.lock().unwrap().drain(..).collect();
                for (src, text, c) in q { a.toast(src as HWND, &text, c) }
                0
            }
            WM_SHOWDECK => {
                // a second `--show` launch: show until the user closes it (closing hides; background keeps running)
                (a.shown, a.dismissed) = (true, false);
                if !a.visible { a.set_visible(true); a.tick(); }
                if IsIconic(h) != 0 { ShowWindow(h, SW_RESTORE); }
                SetForegroundWindow(h);
                0
            }
            WM_TIMER => { a.tick(); 0 }
            WM_SIZE => { a.layout(true); 0 }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = zeroed();
                let dc = BeginPaint(h, &mut ps);
                let mut cr: RECT = zeroed();
                GetClientRect(h, &mut cr);
                // cached back buffer; only the invalid rect is drawn and copied
                let (cw, ch) = (cr.right.max(1), cr.bottom.max(1));
                if a.back.1 != cw || a.back.2 != ch {
                    DeleteObject(a.back.0);
                    a.back = (CreateCompatibleBitmap(dc, cw, ch), cw, ch);
                }
                let mem = CreateCompatibleDC(dc);
                let old = SelectObject(mem, a.back.0);
                let r = ps.rcPaint;
                IntersectClipRect(mem, r.left, r.top, r.right, r.bottom);
                a.paint(mem);
                BitBlt(dc, r.left, r.top, r.right - r.left, r.bottom - r.top, mem, r.left, r.top, SRCCOPY);
                SelectObject(mem, old);
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
                a.update_hover(x, y);
                0
            }
            WM_MOUSELEAVE => {
                if a.drag.is_none() {
                    (a.hover, a.hot, a.hdr, a.confirm, a.tip, a.tip_want) = (None, None, 0, None, None, None);
                    KillTimer(h, TIP_TIMER);
                    ShowWindow(a.overlay, SW_HIDE);
                    InvalidateRect(h, null(), 0);
                }
                0
            }
            WM_KEYDOWN if wp == VK_ESCAPE as usize && a.confirm.is_some() => { a.confirm = None; a.render_overlay(); 0 }
            WM_SETCURSOR if (a.hover.is_some() || a.hdr != 0) && (lp & 0xFFFF) as u32 == HTCLIENT => { SetCursor(LoadCursorW(null_mut(), IDC_HAND)); 1 }
            WM_DPICHANGED => {
                a.dpi = (wp >> 16) as u32 & 0xFFFF;
                a.make_fonts();
                let r = &*(lp as *const RECT);
                SetWindowPos(h, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                0
            }
            WM_CLOSE => {
                // X: back to background mode when it is on (never quits it); a plain --show deck without it quits.
                if a.force && !background_enabled() { a.save_settings(); DestroyWindow(h); }
                else { (a.force, a.dismissed, a.shown) = (false, true, false); a.set_visible(false); }
                0
            }
            WM_SYSCOMMAND if (wp & 0xFFF0) == SC_BACKGROUND => {
                let on = !background_enabled();
                let _ = set_background(on);
                CheckMenuItem(GetSystemMenu(h, 0), SC_BACKGROUND as u32, if on { MF_CHECKED } else { MF_UNCHECKED });
                0
            }
            WM_DESTROY => { PostQuitMessage(0); 0 }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

fn pick_face(faces: &[&str]) -> Vec<u16> {
    for &face in faces {
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
    w(faces[faces.len() - 1])
}

// --check-update: exit 10 if a newer release exists (silent). --update: install it, then restart a deck running from
// this folder into the new exe.
fn update_cli(install: bool) -> Result<(), String> {
    let r = update::newer().or_else(|_| { std::thread::sleep(std::time::Duration::from_secs(3)); update::newer() })?;
    let Some(r) = r else { return Ok(()) };
    if !install { std::process::exit(10) }
    let exe = update::install(&r)?;
    unsafe {
        let deck = FindWindowW(deck_class().as_ptr(), null());
        if !deck.is_null() {
            let dir = |p: &str| std::path::Path::new(p).parent().map(|d| d.to_string_lossy().to_lowercase());
            if dir(&process_path(window_pid(deck))) == dir(&exe.to_string_lossy()) { PostMessageW(deck, WM_RESTART, 0, 0); }
        }
    }
    Ok(())
}

// --check: exit 3 (no output) when the instance is paused; --pause / --resume write its control file.
fn control_cli(cmd: &str, title: Option<&String>) -> Result<(), String> {
    let key = key_for(title.ok_or_else(|| format!("usage: {cmd} <title>"))?);
    match cmd {
        "--check" => { if is_paused(&key) { std::process::exit(3) } Ok(()) }
        _ => { let on = cmd == "--pause"; log(if on { "pause (cli)" } else { "resume (cli)" }, &key); set_paused(&key, on) }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cli = match args.get(1).map(String::as_str) {
        Some("--status") => Some(write_status(&args[2..])),
        Some("--install") => Some(set_background(true).and_then(|_| install(true))),
        Some("--uninstall") => Some(set_background(false).and_then(|_| install(false))),
        Some("--show-when") => Some(set_show_when(args.get(2))),
        Some("--background") => Some(match args.get(2).map(String::as_str) {
            Some("on") => set_background(true),
            Some("off") => set_background(false),
            _ => Err("usage: --background on|off".into()),
        }),
        Some(c @ ("--shot" | "--click" | "--key" | "--scroll")) => Some(remote::run(c, &args[2..])),
        Some(c @ ("--check" | "--pause" | "--resume")) => Some(control_cli(c, args.get(2))),
        Some(c @ ("--check-update" | "--update")) => Some(update_cli(c == "--update")),
        _ => None,
    };
    if let Some(r) = cli {
        if let Err(e) = r { eprintln!("studiodeck: {e}"); std::process::exit(1) }
        return;
    }
    let force = args.iter().any(|a| a == "--show");
    if !force && !background_enabled() { return } // fully off unless the background toggle is on
    unsafe {
        let name = w(if dev() { "Local\\studiodeck-single-instance-dev" } else { "Local\\studiodeck-single-instance" });
        update::MUTEX.store(CreateMutexW(null(), 0, name.as_ptr()) as usize, std::sync::atomic::Ordering::Relaxed);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Already running (usually the background instance): --show surfaces its window instead of doing nothing.
            let deck = FindWindowW(deck_class().as_ptr(), null());
            if force && !deck.is_null() {
                let mut pid = 0;
                GetWindowThreadProcessId(deck, &mut pid);
                AllowSetForegroundWindow(pid);
                SendMessageW(deck, WM_SHOWDECK, 0, 0);
                SetForegroundWindow(deck);
            }
            return;
        }
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        update::cleanup_old(); // left behind by a self-update

        let inst = GetModuleHandleW(null());
        let class = deck_class();
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
        // hover-bar popup: owned (stays above the deck), layered, click-through, never activates
        let oclass = w("StudioDeckOverlay");
        let owc = WNDCLASSEXW { cbSize: size_of::<WNDCLASSEXW>() as u32, lpfnWndProc: Some(DefWindowProcW), hInstance: inst, lpszClassName: oclass.as_ptr(), ..zeroed() };
        RegisterClassExW(&owc);
        let overlay = CreateWindowExW(WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST, oclass.as_ptr(), null(), WS_POPUP,
            0, 0, 0, 0, hwnd, null_mut(), inst, null());

        let boxed = Box::new(App {
            hwnd, force, dismissed: false, visible: false, tiles: Vec::new(), hover: None, status: Status::default(),
            dpi: GetDpiForWindow(hwnd).max(96), face: pick_face(&["Segoe UI Variable Text", "Segoe UI"]),
            icon_face: pick_face(&["Segoe Fluent Icons", "Segoe MDL2 Assets"]), f_title: null_mut(), f_body: null_mut(), f_icon: null_mut(),
            card: (0.0, 0.0), slots: Vec::new(), drag: None, animating: false, last_frame: Instant::now(), settings,
            overlay, hot: None, hdr: 0, confirm: None, pc_mem: (0.0, 0.0), pending: Vec::new(), shown: force,
            update: None, update_open: false, update_msg: String::new(), tip_want: None, tip: None, back: (null_mut(), 0, 0),
            trace: std::env::var_os("STUDIODECK_TRACE").and_then(|p| std::fs::File::create(p).ok()),
        });
        APP.set(Box::into_raw(boxed));
        // system-menu toggle (title-bar icon / Alt+Space): "Run in background"
        let sm = GetSystemMenu(hwnd, 0);
        let label = w("Run in background (auto-show with 2+ Studios)");
        AppendMenuW(sm, MF_SEPARATOR, 0, null());
        AppendMenuW(sm, MF_STRING | if background_enabled() { MF_CHECKED } else { MF_UNCHECKED }, SC_BACKGROUND, label.as_ptr());
        let label = w("Keep on top");
        AppendMenuW(sm, MF_STRING | if app().topmost_setting() { MF_CHECKED } else { MF_UNCHECKED }, SC_TOPMOST, label.as_ptr());
        // update check: at start, then every 6 h; one retry after a minute, otherwise quiet
        let deck = hwnd as usize;
        std::thread::spawn(move || loop {
            for attempt in 0..2 {
                if let Ok(r) = update::newer() {
                    *UPD.lock().unwrap() = Some(Ok(r));
                    PostMessageW(deck as HWND, WM_UPDATE, 0, 0);
                    break;
                }
                if attempt == 0 { std::thread::sleep(std::time::Duration::from_secs(60)) }
            }
            std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
        });
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

#[cfg(test)]
#[test]
fn iso_times() {
    assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_iso("2026-10-07T05:59:39.5Z"), Some(1_791_352_779_500));
    assert_eq!(parse_iso("2000-03-01T00:00:00.123Z"), Some(951_868_800_123));
    assert_eq!(place_key("LW_Test1.rbxl"), "lw_test1");
}
