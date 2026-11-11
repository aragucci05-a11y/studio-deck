// Studio Deck (studiodeck.exe): one window with a live DWM thumbnail of every open Roblox Studio, its RAM, and the
// task/agent status Claude Code sessions report for it. Runs in the background, shows itself while
// 2+ Studios are open; tiles reorder by drag (springs, frames only while moving). Win32 + GDI; DWM composites thumbnails.
// v0.2: header (counts, RAM, pin = topmost, Pause all, update pill); hover action bar per tile (Save, Publish, Mute
// (Windows mixer, audio.rs), Pause agents, Close) + tooltips in a click-through layered popup above the thumbnails; pause = control file agents poll
// via --check; "Waiting on" line + resolveOn; toasts; spring-physics drag paced by DwmFlush; self-update (update.rs).
// v0.4: Images gallery (header button): every image Claude sent or looked at in chat. Opens on a scrolling grid of all of
// them; click one (or Enter) for the big view + filmstrip, Esc back to the grid.
// v0.8: Music view has a player bar at the bottom: play/pause, time, and a scrub bar (click or drag to seek). Everything
// plays inside the deck; a format Windows can't decode says so instead of opening another app.
// v0.10: a full-screen (maximised) deck stays full screen and stays above every Studio that opens: new Studios are left
// as they are behind it, and the deck comes back to the front over them for their first minute (no focus taken). It
// reopens maximised after a restart. Raising uses the topmost toggle, which works even when another app has the focus.
// v0.9: keeping its space no longer needs the pin: unpinned, the deck moves new Studios aside the same way, raises itself
// back over a new Studio it still shares space with (no focus taken), and goes on top only while a Studio splash covers it.
// v0.7: pinned (keep on top), the deck keeps its own space: a Studio window that opens over it (or maximises over it in its
// first 20 s) is moved beside it, never activated; and the deck re-asserts its place over an always-on-top Studio window.
// v0.6: the header's update button is always there: "Check for updates" asks GitHub now; a newer release opens the
// update panel (Update downloads, verifies and restarts the deck), else it says "Up to date" for a few seconds.
// v0.5: Music gallery (header button): every track agents made, by game then agent (music.rs); click a card to play/stop.
// v0.3: Apps panel (entries with "app": Blender, LMMS, any MCP server; live window thumbnail, progress), fed by agents or
// automatically by the --mcp-hook Claude Code hook; click an app header to fold it, its thumbnail (or right-click a tile) to enlarge.
#![windows_subsystem = "windows"]

use std::{collections::HashMap, ffi::c_void, mem::zeroed, path::PathBuf, ptr::{null, null_mut}, time::{Instant, SystemTime}};
mod audio;
mod remote;
mod update;
mod gallery;
mod music;
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
    resolve_on: String, resolved: String, src: PathBuf, place_id: String, universe: String,
    // APPS panel: app = the tool/app the agent drives ("blender", "lmms", any MCP server); progress 0..1; window = exe-name
    // substring for the live thumbnail (default: the app name's first word).
    app: String, progress: Option<f64>, window: String }
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
        updated: a.get("at").and_then(|x| x.as_u64()).or(updated),
        waiting_on: s(a, "waitingOn"),
        app: s(a, "app"),
        progress: a.get("progress").and_then(|x| x.as_f64()).map(|p| if p > 1.0 { p / 100.0 } else { p }.clamp(0.0, 1.0)),
        window: s(a, "window").to_lowercase(),
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
        let mut m: Vec<(&Agent, bool)> = self.agents.iter()
            .filter(|a| Self::matches(a, place, title))
            .map(|a| (a, a.updated.is_some_and(|u| now.saturating_sub(u) >= STALE_MS))).collect();
        m.sort_by_key(|(a, stale)| (*stale, rank(&a.status)));
        m.first().map(|&(a, s)| (a, s, m.len() - 1))
    }
}

// Music / modelling apps whose running agents show the deck even below MIN_STUDIOS (entry updated within SHOW_APP_MS).
const SHOW_APPS: [&str; 2] = ["blender", "lmms"];
const SHOW_APP_MS: u64 = 10 * 60 * 1000;
fn app_shows(a: &Agent, now: u64) -> bool {
    let app = a.app.to_lowercase();
    a.status == "running" && SHOW_APPS.iter().any(|s| app.contains(s)) && a.updated.is_none_or(|u| now.saturating_sub(u) < SHOW_APP_MS)
}
fn rank(s: &str) -> u8 { match s { "running" => 0, "blocked" | "waiting" => 1, "failed" | "error" => 2, _ => 3 } }

// ---------- apps panel ----------
// APPS: right-hand column for every fresh status entry that names an "app" (Blender, LMMS, any MCP server), one card
// per app: live DWM thumbnail of that app's window when open, then a row per agent (dot, name · step, progress bar).
struct Group { key: String, label: String, agents: Vec<Agent>, src: HWND, thumb: isize, aspect: f64 }
const APPS_W: i32 = 320;
const MUSIC_CARD: i32 = 300; // the music view's target card width
enum MItem { Game(String), Agent(String), Track(usize) }
const GRID_CARD: i32 = 240; // the gallery grid's target card width (columns are fitted to the window)
const ROW_H: i32 = 26;

// Visible, un-minimised top-level windows: (hwnd, lowercase exe file name, client area).
fn top_windows(skip: HWND) -> Vec<(HWND, String, i64)> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let v = unsafe { &mut *(lp as *mut Vec<HWND>) };
        if unsafe { IsWindowVisible(h) != 0 && IsIconic(h) == 0 && GetWindow(h, GW_OWNER).is_null() && GetWindowTextLengthW(h) > 0
            && GetWindowLongPtrW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW == 0 } { v.push(h) }
        1
    }
    let mut all: Vec<HWND> = Vec::new();
    unsafe { EnumWindows(Some(cb), &mut all as *mut _ as LPARAM) };
    all.into_iter().filter(|&h| h != skip).map(|h| {
        let mut r: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(h, &mut r) };
        (h, process_path(window_pid(h)).to_lowercase().rsplit('\\').next().unwrap_or("").to_string(), r.right as i64 * r.bottom as i64)
    }).collect()
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
    write_session(session, &doc)
}
fn session_path(session: &str) -> PathBuf {
    status_dir().join(format!("{}.json", session.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect::<String>()))
}
fn write_session(session: &str, doc: &serde_json::Value) -> Result<(), String> {
    let path = session_path(session);
    std::fs::create_dir_all(status_dir()).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, doc.to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// --mcp-hook: Claude Code hook (event JSON on stdin), no agent cooperation needed. PreToolUse on mcp__<server>__<tool>
// upserts entry {app: server, step: tool, status: running} in status/mcp-<session>.json; Stop marks the session done.
// ponytail: read-modify-write without a lock; two parallel tool calls in one session can drop a step (next call fixes it).
fn mcp_hook() -> Result<(), String> {
    let text = std::io::read_to_string(std::io::stdin()).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    // Music an agent made or used -> music gallery, credited to the session (and subagent) that touched it first
    if s("hook_event_name") == "PostToolUse" {
        let mut found = Vec::new();
        music::paths_in(&v["tool_input"], &mut found);
        music::paths_in(&v["tool_response"], &mut found);
        if found.is_empty() { return Ok(()) }
        let mut agent = music::title_of(std::path::Path::new(&s("transcript_path")));
        if let Some(sub) = v["agent_type"].as_str() { agent = format!("{agent} \u{00B7} {sub}") }
        return music::record(&found, &agent, now_ms());
    }
    // Images shown in chat -> gallery (SendUserFile files, or an image file Claude Reads)
    if let t @ ("Read" | "SendUserFile") = s("tool_name").as_str() {
        let cwd = PathBuf::from(s("cwd"));
        let input = &v["tool_input"];
        let files: Vec<&str> = if t == "Read" { input["file_path"].as_str().into_iter().collect() }
            else { input["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str()).collect() };
        for f in files { gallery::add(&cwd.join(f))? }
        return Ok(());
    }
    let session = format!("mcp-{}", s("session_id"));
    let mut doc = std::fs::read_to_string(session_path(&session)).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let now = now_ms();
    if s("hook_event_name") == "Stop" {
        let Some(doc) = doc.as_mut() else { return Ok(()) };
        for a in doc["agents"].as_array_mut().into_iter().flatten() { a["status"] = "done".into(); a["at"] = now.into() }
        doc["updated"] = now.into();
        return write_session(&session, doc);
    }
    let tool = s("tool_name");
    let Some((server, step)) = tool.strip_prefix("mcp__").map(|t| t.split_once("__").unwrap_or((t, ""))) else { return Ok(()) };
    if server.starts_with("ccd_") || server == "visualize" { return Ok(()) } // the desktop app's own UI servers
    let project = std::path::Path::new(&s("cwd")).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let doc = doc.get_or_insert_with(|| serde_json::json!({ "session": session, "project": project, "agents": [] }));
    if !doc["agents"].is_array() { doc["agents"] = serde_json::json!([]) }
    let list = doc["agents"].as_array_mut().unwrap();
    let i = list.iter().position(|a| a["app"] == server).unwrap_or_else(|| { list.push(serde_json::json!({ "app": server, "calls": 0 })); list.len() - 1 });
    let a = &mut list[i];
    a["name"] = project.into();
    a["status"] = "running".into();
    a["step"] = step.replace('_', " ").into();
    a["at"] = now.into();
    a["calls"] = (a["calls"].as_u64().unwrap_or(0) + 1).into();
    doc["updated"] = now.into();
    write_session(&session, doc)
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
    muted: bool, local: bool, toast: Option<(String, u32, u64)>,
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
enum Act { Save, Publish, Mute, Pause, Close }
const BAR: [Act; 5] = [Act::Save, Act::Publish, Act::Mute, Act::Pause, Act::Close];
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
    // checking = a check the user asked for is running; uptodate_until = show "Up to date" until then (ms)
    checking: bool, uptodate_until: u64,
    // seen = when each Studio window first appeared (0 = already open when the deck started); see make_room
    seen: Option<HashMap<usize, u64>>,
    // splash_top = unpinned, but on top for now because a Studio loading splash (always-on-top) overlaps the deck
    splash_top: bool,
    // TOOLTIPS: tip_want = (text, anchor) under the pointer; shown as `tip` (text, label rect) after 500 ms (TIP_TIMER).
    tip_want: Option<(String, RECT, (i32, i32))>, tip: Option<(String, RECT)>,
    // shown = the user opened it (--show, shortcut) or touched it (click, drag, resize): stays up until closed.
    shown: bool, back: (HBITMAP, i32, i32), trace: Option<std::fs::File>,
    groups: Vec<Group>,
    // ZOOM: the DWM thumbnail (Studio tile or app window) shown enlarged over the whole deck; collapsed = app cards
    // folded to their header row.
    zoom: Option<isize>, collapsed: std::collections::HashSet<String>,
    // GALLERY (view open = gallery): gal = images newest first, gal_i = selected, gal_seen = newest ms already viewed;
    // thumbs / big = decoded HBITMAPs (w, h) from the gallery worker; asked = requests in flight or done.
    // grid = the gallery shows the grid of all images (else the big view); gal_top = the grid's scroll in px.
    grid: bool, gal_top: i32,
    // MUSIC view (open = music): tracks from music.rs, scrolled by mus_top; playing = the track MCI is playing.
    music: bool, tracks: Vec<music::Track>, tracks_at: Option<SystemTime>, mus_top: i32, playing: Option<PathBuf>,
    // the loaded track's player: paused by the user; scrub = Some(ms) while the scrub bar is dragged; mus_msg = a short note (until ms)
    paused: bool, scrub: Option<u64>, mus_len: u64, mus_msg: Option<(String, u64)>,
    gallery: bool, gal: Vec<PathBuf>, gal_i: usize, gal_seen: u64, thumbs: HashMap<PathBuf, (HBITMAP, i32, i32)>,
    big: Option<(PathBuf, HBITMAP, i32, i32)>, asked: std::collections::HashSet<(PathBuf, bool)>, gal_tx: std::sync::mpsc::Sender<(PathBuf, (u32, u32), bool)>,
}
const WATCH_TIMER: usize = 3;
const TIP_TIMER: usize = 4;
const PLAYER_TIMER: usize = 5; // repaints the player bar (5 Hz) while the music view is open
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
const H_GAL: u8 = 6;
const H_MUS: u8 = 7;

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
        let now = now_ms();
        let first = self.seen.is_none();
        let seen = self.seen.get_or_insert_with(HashMap::new);
        seen.retain(|h, _| found.contains(&(*h as HWND)));
        for &h in &found { seen.entry(h as usize).or_insert(if first { 0 } else { now }); }
        let min = min_studios();
        // A window the user opened or touched stays until closed. An auto-shown one hides below `min` Studios,
        // unless an agent is waiting or a toast is up.
        let stay = self.visible && (self.status.agents.iter().any(|a| is_waiting(&a.status)) || self.tiles.iter().any(|t| t.toast.is_some())
            || self.groups.iter().any(|g| g.agents.iter().any(|a| a.status == "running")));
        // Exception to the 2-Studio rule: an agent is making music or a model right now (until the user closes the deck).
        self.status.refresh();
        let now = now_ms();
        let live = self.status.agents.iter().any(|a| app_shows(a, now));
        if found.len() < min && !live { self.dismissed = false }
        let want = want_visible(found.len(), min, self.force || self.shown, stay, self.dismissed) || (live && !self.dismissed);
        if want != self.visible { self.set_visible(want) }
        if !self.visible { return }
        let new_studio = found.iter().any(|h| !self.tiles.iter().any(|t| t.src == *h));
        self.apply_topmost(new_studio, &found);
        self.keep_space(&found, now);
        self.refresh_groups();
        self.refresh_gallery();
        self.refresh_music();
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
                    slot: (0.0, 0.0), cur: (0.0, 0.0), placed: false, lift: 0.0, toff: (0.0, 0.0, 0.0, 0.0), paused: false, muted: audio::is_muted(window_pid(h)), local: false, toast: None,
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
            if t.muted { audio::set_muted(window_pid(t.src), true) } // sessions Studio opened since
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
        if self.gallery || self.music {
            // a gallery covers the tiles: only the header's numbers change each second (closing it lays the tiles out)
            let mut r: RECT = unsafe { zeroed() };
            unsafe { GetClientRect(self.hwnd, &mut r) };
            r.bottom = self.px(HEADER);
            unsafe { InvalidateRect(self.hwnd, &r, 0) };
        } else { self.layout(false) }
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
        let want = (self.topmost_setting() || self.splash_top) && !modal;
        if want != self.is_top() || (reassert && want) { self.set_top(want) }
    }
    // The deck keeps its space (pinned or not): Studio windows that appeared in the last 20 s and overlap the deck move beside
    // it (the wider free side of the deck's monitor, restored first if maximised; nothing is activated). Unpinned, a new
    // Studio that still overlaps (no room beside) gets the deck raised back over it, z-order only. If a visible
    // always-on-top Studio window (its loading splash) overlaps the deck, the deck goes on top of that band; unpinned only
    // until the splash is gone.
    fn keep_space(&mut self, studios: &[HWND], now: u64) {
        let rect = |h: HWND| { let mut r: RECT = unsafe { zeroed() }; unsafe { GetWindowRect(h, &mut r) }; r };
        let overlaps = |a: &RECT, b: &RECT| a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
        let deck = rect(self.hwnd);
        let seen = self.seen.as_ref().map(|s| s.clone()).unwrap_or_default();
        let full = unsafe { IsZoomed(self.hwnd) } != 0;
        let mut cover = false; // a Studio opened in the last minute shares the deck's space
        for &h in studios {
            let born = seen.get(&(h as usize)).copied().unwrap_or(0);
            if born == 0 || unsafe { IsIconic(h) } != 0 || !overlaps(&rect(h), &deck) { continue }
            let age = now.saturating_sub(born);
            // full screen: the deck keeps the screen; new Studios stay as they are, behind it
            if full { cover |= age <= 60_000; continue }
            if age > 20_000 { continue }
            let mut mi: MONITORINFO = unsafe { zeroed() };
            mi.cbSize = size_of::<MONITORINFO>() as u32;
            unsafe { GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut mi) };
            let wa = mi.rcWork;
            let (left, right) = (deck.left - wa.left, wa.right - deck.right);
            let gap = self.px(8);
            let (x, w) = if right >= left { (deck.right + gap, right - gap) } else { (wa.left, left - gap) };
            if w >= self.px(640) {
                unsafe {
                    if IsZoomed(h) != 0 { ShowWindow(h, SW_SHOWNOACTIVATE); }
                    SetWindowPos(h, null_mut(), x, wa.top, w, wa.bottom - wa.top, SWP_NOZORDER | SWP_NOACTIVATE);
                }
            } else {
                cover = true; // no room beside the deck on this monitor: stay over it instead
            }
        }
        if cover && !self.topmost_setting() { self.raise() }
        let splash = studios.iter().any(|&h| process_windows(window_pid(h)).into_iter().any(|w| {
            (unsafe { GetWindowLongPtrW(w, GWL_EXSTYLE) } as u32 & WS_EX_TOPMOST) != 0 && overlaps(&rect(w), &deck)
        }));
        if self.topmost_setting() { if splash { self.set_top(true) } return }
        if splash != self.splash_top {
            self.splash_top = splash;
            self.set_top(splash);
        }
    }
    // To the front of the normal windows without taking focus. A plain HWND_TOP is ignored while another app has the
    // foreground; going topmost and straight back is not.
    fn raise(&self) {
        let f = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
        unsafe {
            SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, f);
            if !self.topmost_setting() && !self.splash_top { SetWindowPos(self.hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, f); }
        }
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
                for g in self.groups.drain(..) { if g.thumb != 0 { DwmUnregisterThumbnail(g.thumb); } }
            }
        }
    }

    // Rebuilds the apps panel from fresh entries with an "app"; keeps a group's thumbnail while its window is the same.
    fn refresh_groups(&mut self) {
        let now = now_ms();
        let mut groups: Vec<Group> = Vec::new();
        for a in self.status.agents.iter().filter(|a| !a.app.is_empty() && a.updated.is_none_or(|u| now.saturating_sub(u) < STALE_MS)) {
            let key = a.app.to_lowercase();
            match groups.iter_mut().find(|g| g.key == key) {
                Some(g) => g.agents.push(a.clone()),
                None => groups.push(Group { key, label: a.app.clone(), agents: vec![a.clone()], src: null_mut(), thumb: 0, aspect: 16.0 / 9.0 }),
            }
        }
        groups.sort_by(|a, b| a.key.cmp(&b.key));
        let wins = if groups.is_empty() { Vec::new() } else { top_windows(self.hwnd) };
        for g in &mut groups {
            g.agents.sort_by_key(|a| (rank(&a.status), std::cmp::Reverse(a.updated)));
            let exe = g.agents.iter().find(|a| !a.window.is_empty()).map(|a| a.window.clone())
                .unwrap_or_else(|| g.key.split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or("").to_string());
            g.src = wins.iter().filter(|w| !exe.is_empty() && w.1.contains(&exe)).max_by_key(|w| w.2).map_or(null_mut(), |w| w.0);
            if g.src.is_null() { continue }
            let mut rc: RECT = unsafe { zeroed() };
            unsafe { GetClientRect(g.src, &mut rc) };
            if rc.right > 0 && rc.bottom > 0 { g.aspect = rc.right as f64 / rc.bottom as f64 }
            if let Some(o) = self.groups.iter_mut().find(|o| o.key == g.key && o.src == g.src && o.thumb != 0) { g.thumb = std::mem::take(&mut o.thumb) }
            else if unsafe { DwmRegisterThumbnail(self.hwnd, g.src, &mut g.thumb) } != 0 { g.thumb = 0 }
        }
        for o in &self.groups { if o.thumb != 0 { unsafe { DwmUnregisterThumbnail(o.thumb) }; } }
        self.groups = groups;
    }
    // Re-lists the gallery (a few dozen names); keeps the selected image selected and frees bitmaps of pruned ones.
    fn refresh_gallery(&mut self) {
        let list = gallery::list();
        if list == self.gal { return }
        let cur = self.gal.get(self.gal_i).cloned();
        self.thumbs.retain(|p, (b, _, _)| { let keep = list.contains(p); if !keep { unsafe { DeleteObject(*b) }; } keep });
        self.asked.retain(|(p, _)| list.contains(p));
        self.gal_i = cur.and_then(|c| list.iter().position(|p| *p == c)).unwrap_or(0);
        self.gal = list;
        if self.gallery { self.gal_seen = self.gal.first().map_or(0, |p| gallery::parts(p).1) }
        if self.gal.is_empty() && self.gallery { self.set_gallery(false) } else if self.gallery { self.gal_request() }
    }
    fn set_gallery(&mut self, on: bool) {
        self.gallery = on && !self.gal.is_empty();
        self.gal_seen = self.gal.first().map_or(0, |p| gallery::parts(p).1);
        if self.gallery { (self.zoom, self.grid) = (None, true); self.gal_show() }
        else if let Some((p, b, _, _)) = self.big.take() { unsafe { DeleteObject(b) }; self.asked.remove(&(p, true)); }
        self.apply_thumbs();
    }
    fn gal_go(&mut self, i: isize) {
        self.gal_i = i.clamp(0, self.gal.len() as isize - 1) as usize;
        self.gal_show();
    }
    // After the selection or view changed: keep the selection in sight on the grid, decode what's now visible, repaint.
    fn gal_show(&mut self) {
        if self.grid {
            let (area, content, cols, _) = self.grid_layout();
            let ((_, ch), gap) = self.grid_cell(cols);
            let y = (self.gal_i / cols) as i32 * (ch + gap);
            let h = area.bottom - area.top;
            self.gal_top = self.gal_top.min(y).max(y + ch - h).clamp(0, (content - h).max(0));
        }
        self.gal_request();
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }
    fn gal_scroll(&mut self, rows: i32) {
        let (area, content, cols, _) = self.grid_layout();
        let ((_, ch), gap) = self.grid_cell(cols);
        self.gal_top = (self.gal_top + rows * (ch + gap)).clamp(0, (content - (area.bottom - area.top)).max(0));
        self.gal_request();
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }
    fn grid_area(&self) -> RECT {
        let mut cr: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut cr) };
        RECT { left: self.px(16), top: self.px(self.header_h()), right: cr.right - self.px(16), bottom: cr.bottom - self.px(16) }
    }
    // Card size (w, h) and gap for `cols` columns across the grid area.
    fn grid_cell(&self, cols: usize) -> ((i32, i32), i32) {
        let (a, gap) = (self.grid_area(), self.px(10));
        let cw = (a.right - a.left - gap * (cols as i32 - 1)) / cols as i32;
        ((cw, cw * 10 / 16), gap)
    }
    // Grid view: area, content height, columns, and the cards (index, rect) that are at least partly in sight.
    fn grid_layout(&self) -> (RECT, i32, usize, Vec<(usize, RECT)>) {
        let a = self.grid_area();
        let gap = self.px(10);
        let cols = ((a.right - a.left + gap) / (self.px(GRID_CARD) + gap)).max(1) as usize;
        let ((cw, ch), _) = self.grid_cell(cols);
        let n = self.gal.len();
        let content = n.div_ceil(cols) as i32 * (ch + gap) - gap;
        let cells = (0..n).filter_map(|i| {
            let (x, y) = (a.left + (i % cols) as i32 * (cw + gap), a.top + (i / cols) as i32 * (ch + gap) - self.gal_top);
            (y + ch > a.top && y < a.bottom).then_some((i, RECT { left: x, top: y, right: x + cw, bottom: y + ch }))
        }).collect();
        (a, content, cols, cells)
    }
    // Asks the worker for the selected image (at the big view's size: repaints are then a plain copy) and the filmstrip's thumbnails not decoded yet.
    fn gal_request(&mut self) {
        let mut want: Vec<(PathBuf, (u32, u32), bool)> = Vec::new();
        let thumb = (self.px(GRID_CARD + 80) as u32, self.px((GRID_CARD + 80) * 10 / 16) as u32); // one size for grid cards and filmstrip
        let cells = if self.grid { self.grid_layout().3 } else {
            let (main, strip) = self.gal_layout();
            if let Some(p) = self.gal.get(self.gal_i) && self.big.as_ref().is_none_or(|b| b.0 != *p) { want.push((p.clone(), ((main.right - main.left).max(1) as u32, (main.bottom - main.top).max(1) as u32), true)) }
            strip
        };
        for (i, _) in cells { if !self.thumbs.contains_key(&self.gal[i]) { want.push((self.gal[i].clone(), thumb, false)) } }
        for (p, fit, big) in want { if self.asked.insert((p.clone(), big)) { let _ = self.gal_tx.send((p, fit, big)); } }
    }
    fn gal_decoded(&mut self) {
        let done = std::mem::take(&mut *gallery::DONE.lock().unwrap());
        for d in done {
            let bmp = d.bmp as HBITMAP;
            if !d.big { self.thumbs.insert(d.path, (bmp, d.w, d.h)); continue }
            self.asked.remove(&(d.path.clone(), true));
            if self.gallery && self.gal.get(self.gal_i) == Some(&d.path) {
                if let Some((p, b, _, _)) = self.big.replace((d.path, bmp, d.w, d.h)) { unsafe { DeleteObject(b) }; self.asked.remove(&(p, true)); }
                self.asked.insert((self.big.as_ref().unwrap().0.clone(), true));
            } else if !bmp.is_null() { unsafe { DeleteObject(bmp) }; }
        }
        if self.gallery { unsafe { InvalidateRect(self.hwnd, null(), 0) }; }
    }
    // Big view (above a caption line) and the filmstrip cells (index, rect), centred on the selection.
    fn gal_layout(&self) -> (RECT, Vec<(usize, RECT)>) {
        let mut cr: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut cr) };
        let (cw, ch, gap) = (self.px(132), self.px(80), self.px(10));
        let bottom = cr.bottom - self.px(16);
        let n = self.gal.len();
        let k = (((cr.right - self.px(32) + gap) / (cw + gap)).max(1) as usize).min(n);
        let start = self.gal_i.saturating_sub(k / 2).min(n - k);
        let x0 = (cr.right - (k as i32 * (cw + gap) - gap)) / 2;
        let strip = (0..k).map(|j| (start + j, RECT { left: x0 + j as i32 * (cw + gap), top: bottom - ch, right: x0 + j as i32 * (cw + gap) + cw, bottom })).collect();
        (RECT { left: self.px(16), top: self.px(self.header_h()), right: cr.right - self.px(16), bottom: bottom - ch - self.px(44) }, strip)
    }
    fn apps_w(&self) -> i32 { if self.groups.is_empty() { 0 } else { self.px(APPS_W) } }
    // Per group: card rect, thumbnail rect (if its window is open), top of the first agent row.
    fn group_rects(&self) -> Vec<(RECT, Option<RECT>, i32)> {
        let mut cr: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut cr) };
        let (l, r) = (cr.right - self.apps_w(), cr.right - self.px(16));
        let (il, ir) = (l + self.px(12), r - self.px(12));
        let mut y = self.px(self.header_h());
        self.groups.iter().map(|g| {
            let top = y;
            y += self.px(36);
            let open = !self.collapsed.contains(&g.key);
            let thumb = (open && g.thumb != 0).then(|| {
                let h = (((ir - il) as f64 / g.aspect) as i32).min(self.px(180));
                let w = (h as f64 * g.aspect) as i32;
                let t = RECT { left: il + (ir - il - w) / 2, top: y, right: il + (ir - il - w) / 2 + w, bottom: y + h };
                y += h + self.px(8);
                t
            });
            let rows = y;
            if open { y += self.px(ROW_H) * g.agents.len() as i32 + self.px(8) }
            let card = RECT { left: l, top, right: r, bottom: y };
            y += self.px(12);
            (card, thumb, rows)
        }).collect()
    }

    // Auto grid: try every column count, keep the one that gives the biggest thumbnails.
    // Sets each tile's slot; snap = jump there (resize), otherwise glide.
    fn layout(&mut self, snap: bool) {
        if self.zoom.is_some_and(|z| !self.tiles.iter().any(|t| t.thumb == z) && !self.groups.iter().any(|g| g.thumb == z)) { self.zoom = None } // window gone
        let mut cr: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut cr) };
        let n = self.tiles.len();
        if n == 0 { self.apply_thumbs(); return }
        let (g, pad, cap) = (self.pxf(16.0), self.pxf(8.0), self.pxf(48.0 + 18.0 * self.extra_line() as i32 as f64));
        let top = self.pxf(self.header_h() as f64 - 8.0); // header replaces most of the top gutter
        let (cw, ch) = ((cr.right - self.apps_w()) as f64, cr.bottom as f64 - top);
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
    // Enlarged view: the client area under the header, fitted to `aspect`.
    fn zoom_rect(&self, aspect: f64) -> RECT {
        let mut cr: RECT = unsafe { zeroed() };
        unsafe { GetClientRect(self.hwnd, &mut cr) };
        let (l, t, r, b) = (self.px(16), self.px(self.header_h()), cr.right - self.px(16), cr.bottom - self.px(16));
        let w = ((r - l) as f64).min((b - t) as f64 * aspect) as i32;
        let h = (w as f64 / aspect) as i32;
        RECT { left: l + (r - l - w) / 2, top: t + (b - t - h) / 2, right: l + (r - l - w) / 2 + w, bottom: t + (b - t - h) / 2 + h }
    }
    fn set_zoom(&mut self, z: Option<isize>) { self.zoom = z; self.apply_thumbs() }
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
            let mut p = p;
            if let Some(z) = self.zoom { (p.rcDestination, p.opacity, p.fVisible) = (self.zoom_rect(t.aspect), 255, (z == t.thumb) as BOOL) }
            if self.gallery || self.music { p.fVisible = 0 }
            unsafe { DwmUpdateThumbnailProperties(t.thumb, &p) };
        }
        for (g, (card, thumb, _)) in self.groups.iter().zip(self.group_rects()) {
            if g.thumb == 0 { continue }
            let mut p = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY,
                rcDestination: thumb.unwrap_or(card), rcSource: unsafe { zeroed() }, opacity: 255, fVisible: thumb.is_some() as BOOL, fSourceClientAreaOnly: 1,
            };
            if let Some(z) = self.zoom { (p.rcDestination, p.fVisible) = (self.zoom_rect(g.aspect), (z == g.thumb) as BOOL) }
            if self.gallery || self.music { p.fVisible = 0 }
            unsafe { DwmUpdateThumbnailProperties(g.thumb, &p) };
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
        if self.dragging().is_some() || self.zoom.is_some() || self.gallery || self.music { return None }
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
            None => (pad, vec![h - 2 * pad; BAR.len()], self.px(2), pad),
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
                    // Segoe MDL2 / Fluent: Save, Upload, Volume|Mute, Pause|Play, Cancel
                    let g = match BAR[k] { Act::Save => "\u{E74E}", Act::Publish => "\u{E898}", Act::Mute => if t.muted { "\u{E74F}" } else { "\u{E767}" }, Act::Pause => if t.paused { "\u{E768}" } else { "\u{E769}" }, Act::Close => "\u{E711}" };
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
    fn update_label(&self) -> Option<String> {
        Some(match &self.update {
            Some(r) => format!("Update {}", r.tag),
            None if self.checking => "Checking\u{2026}".into(),
            None if now_ms() < self.uptodate_until => format!("Up to date  v{}", env!("CARGO_PKG_VERSION")),
            None => "Check for updates".into(),
        })
    }
    // The update button with no update known: ask GitHub now (the answer comes back through UPD + WM_UPDATE).
    fn check_now(&mut self) {
        if self.checking { return }
        self.checking = true;
        let deck = self.hwnd as usize;
        std::thread::spawn(move || {
            let r = update::newer();
            *UPD.lock().unwrap() = Some(Ok(r.unwrap_or(None)));
            unsafe { PostMessageW(deck as HWND, WM_UPDATE, 0, 0) };
        });
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }
    fn update_btn(&self) -> Option<RECT> {
        let p = self.pin_btn();
        self.update_label().map(|l| RECT { left: p.left - self.px(8) - self.text_w(&l, self.f_body) - self.px(24), right: p.left - self.px(8), ..p })
    }
    fn gal_label(&self) -> String { format!("Images  {}", self.gal.len()) }
    fn gal_new(&self) -> bool { self.gal.first().is_some_and(|p| gallery::parts(p).1 > self.gal_seen) }
    fn gal_btn(&self) -> Option<RECT> {
        let p = self.update_btn().unwrap_or(self.pin_btn());
        (!self.gal.is_empty()).then(|| RECT { left: p.left - self.px(8) - self.text_w(&self.gal_label(), self.f_body) - self.px(24), right: p.left - self.px(8), ..p })
    }
    fn mus_label(&self) -> String { format!("Music  {}", self.tracks.len()) }
    fn mus_btn(&self) -> Option<RECT> {
        let p = self.gal_btn().or(self.update_btn()).unwrap_or(self.pin_btn());
        (!self.tracks.is_empty()).then(|| RECT { left: p.left - self.px(8) - self.text_w(&self.mus_label(), self.f_body) - self.px(24), right: p.left - self.px(8), ..p })
    }
    // Re-reads the music index when it changed; notices a track that finished playing.
    fn refresh_music(&mut self) {
        let at = std::fs::metadata(crate::deck_dir().join("music.json")).and_then(|m| m.modified()).ok();
        if at != self.tracks_at { (self.tracks, self.tracks_at) = (music::load(), at); if self.music { unsafe { InvalidateRect(self.hwnd, null(), 0) }; } }
        // finished on its own (a paused or scrubbed track reads "stopped" too, so only when neither)
        if self.playing.is_some() && !self.paused && self.scrub.is_none() && music::mode() == "stopped" { self.unload() }
    }
    fn unload(&mut self) {
        music::stop();
        (self.playing, self.paused, self.scrub) = (None, false, None);
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }
    // Card click: a new track loads and plays; the loaded one pauses / resumes.
    fn play_track(&mut self, p: PathBuf) {
        if self.playing.as_ref() == Some(&p) {
            self.paused = !self.paused;
            if self.paused { music::pause() } else { music::resume() }
        } else if music::play(&p) {
            (self.mus_len, self.playing, self.paused) = (music::length(), Some(p), false);
        } else {
            self.unload();
            let ext = p.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
            self.mus_msg = Some((format!("Windows can't play this {ext} file"), now_ms() + 4000));
        }
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }
    // Player bar under the track list while a track is loaded (or a note is up): whole bar, play/pause button, scrub track.
    fn player(&self) -> Option<(RECT, RECT, RECT)> {
        if self.playing.is_none() && self.mus_msg.as_ref().is_none_or(|m| now_ms() > m.1) { return None }
        let a = self.grid_area();
        let bar = RECT { left: a.left, top: a.bottom - self.px(64), right: a.right, bottom: a.bottom };
        let btn = RECT { left: bar.left + self.px(10), top: bar.top + self.px(10), right: bar.left + self.px(54), bottom: bar.bottom - self.px(10) };
        let track = RECT { left: btn.right + self.px(16), top: bar.top + self.px(38), right: bar.right - self.px(130), bottom: bar.top + self.px(56) };
        Some((bar, btn, track))
    }
    fn scrub_ms(&self, x: i32) -> u64 {
        let Some((_, _, t)) = self.player() else { return 0 };
        ((x - t.left) as f64 / (t.right - t.left).max(1) as f64).clamp(0.0, 1.0).mul_add(self.mus_len as f64, 0.0) as u64
    }
    fn invalidate_player(&self) {
        if let Some((bar, _, _)) = self.player() { unsafe { InvalidateRect(self.hwnd, &bar, 0) }; }
    }
    fn set_music(&mut self, on: bool) {
        self.music = on && !self.tracks.is_empty();
        unsafe { if self.music { SetTimer(self.hwnd, PLAYER_TIMER, 200, None); } else { KillTimer(self.hwnd, PLAYER_TIMER); } }
        if self.music { (self.zoom, self.gallery) = (None, false) }
        self.apply_thumbs();
    }
    // Music view: area, content height, and the visible items: game headings, agent sub-headings, track cards.
    fn music_layout(&self) -> (RECT, i32, Vec<(MItem, RECT)>) {
        let mut a = self.grid_area();
        if let Some((bar, _, _)) = self.player() { a.bottom = bar.top - self.px(10) }
        let gap = self.px(10);
        let cols = ((a.right - a.left + gap) / (self.px(MUSIC_CARD) + gap)).max(1);
        let cw = (a.right - a.left - gap * (cols - 1)) / cols;
        let ch = self.px(54);
        let (mut y, mut col, mut items) = (0, 0, Vec::new());
        let mut place = |item: MItem, x: i32, y: i32, w: i32, h: i32| items.push((item, RECT { left: x, top: y, right: x + w, bottom: y + h }));
        for (i, t) in self.tracks.iter().enumerate() {
            let prev = i.checked_sub(1).map(|j| &self.tracks[j]);
            let new_game = prev.is_none_or(|q| q.game != t.game);
            if new_game || prev.is_some_and(|q| q.agent != t.agent) {
                if col > 0 { y += ch + gap; col = 0 }
                if new_game { if i > 0 { y += self.px(10) } place(MItem::Game(t.game.clone()), a.left, y, a.right - a.left, self.px(34)); y += self.px(38) }
                place(MItem::Agent(t.agent.clone()), a.left, y, a.right - a.left, self.px(24)); y += self.px(28);
            }
            place(MItem::Track(i), a.left + col * (cw + gap), y, cw, ch);
            col += 1;
            if col == cols { y += ch + gap; col = 0 }
        }
        let content = if col > 0 { y + ch } else { y - gap };
        let (top, bottom) = (a.top, a.bottom);
        let items = items.into_iter().map(|(it, r)| (it, RECT { top: r.top + top - self.mus_top, bottom: r.bottom + top - self.mus_top, ..r }))
            .filter(|(_, r)| r.bottom > top && r.top < bottom).collect();
        (a, content, items)
    }
    fn mus_scroll(&mut self, rows: i32) {
        let (a, content, _) = self.music_layout();
        self.mus_top = (self.mus_top + rows * self.px(64)).clamp(0, (content - (a.bottom - a.top)).max(0));
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
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
        else if self.gal_btn().is_some_and(|r| inside(&r, x, y)) { H_GAL }
        else if self.mus_btn().is_some_and(|r| inside(&r, x, y)) { H_MUS }
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
        if self.gallery || self.music || self.zoom.is_some() { return None }
        let s = |t: &str| t.to_string();
        match self.hdr {
            H_PAUSE => return Some((s(if self.all_paused() { "Let all agents continue" } else { "Pause all agents" }), self.header_btn())),
            H_PIN => return Some((s("Keep Studio Deck on top"), self.pin_btn())),
            H_UPD => return self.update_btn().map(|b| (self.update.as_ref().map_or("Check GitHub for a newer Studio Deck".into(), |r| format!("Update to {}", r.tag)), b)),
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
                    Act::Mute => if t.muted { "Unmute this Studio" } else { "Mute this Studio" },
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
                Act::Mute => {
                    let on = !self.tiles[b.tile].muted;
                    audio::set_muted(window_pid(src), on);
                    self.tiles[b.tile].muted = on;
                    log(if on { "mute" } else { "unmute" }, &place);
                }
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
        let asked = std::mem::take(&mut self.checking); // the user pressed the button: no snooze, open the panel or say so
        self.update = r.filter(|r| asked || !snoozed(r));
        if asked && self.update.is_some() { (self.update_open, self.update_msg) = (true, String::new()) }
        if asked && self.update.is_none() { self.uptodate_until = now_ms() + 4000 }
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
            H_UPD if self.update.is_none() => return self.check_now(),
            H_UPD => { self.update_open = !self.update_open; self.update_msg.clear(); return self.layout(false) }
            H_LATER => return self.snooze_update(),
            H_DO => return self.start_update(),
            H_GAL => { self.music = false; return self.set_gallery(!self.gallery) }
            H_MUS => return self.set_music(!self.music),
            _ => {}
        }
        if let (Some(b), Some(k)) = (self.bar(), self.hot) { self.press(b, k); return }
        if self.confirm.take().is_some() { self.render_overlay() }
        if self.music {
            if let Some((bar, btn, track)) = self.player() && inside(&bar, x, y) {
                if self.playing.is_none() { return }
                if inside(&btn, x, y) { let p = self.playing.clone().unwrap(); return self.play_track(p) }
                // the scrub track, with some slack above and below so it's easy to grab
                if x >= track.left - self.px(8) && x <= track.right + self.px(8) && y >= track.top - self.px(14) {
                    let ms = self.scrub_ms(x);
                    self.scrub = Some(ms);
                    music::seek(ms, false);
                    unsafe { SetCapture(self.hwnd) };
                    self.invalidate_player();
                }
                return;
            }
            if let Some((MItem::Track(i), _)) = self.music_layout().2.into_iter().find(|(it, r)| matches!(it, MItem::Track(_)) && inside(r, x, y)) {
                let p = self.tracks[i].path.clone();
                self.play_track(p);
            }
            return;
        }
        if self.gallery && self.grid {
            if let Some((i, _)) = self.grid_layout().3.into_iter().find(|(_, r)| inside(r, x, y)) { self.grid = false; self.gal_go(i as isize) }
            return;
        }
        if self.gallery {
            let (main, strip) = self.gal_layout();
            if let Some(&(i, _)) = strip.iter().find(|(_, r)| inside(r, x, y)) { return self.gal_go(i as isize) }
            if inside(&main, x, y) { return self.gal_go(self.gal_i as isize + if x < (main.left + main.right) / 2 { -1 } else { 1 }) }
            return;
        }
        // enlarged: any click returns; app card: header folds it, thumbnail enlarges it
        if self.zoom.is_some() { return self.set_zoom(None) }
        let hit = self.groups.iter().zip(self.group_rects()).find(|(_, (c, _, _))| inside(c, x, y)).map(|(g, (c, t, _))| (g.key.clone(), g.thumb, y < c.top + self.px(36), t.is_some_and(|t| inside(&t, x, y))));
        if let Some((key, thumb, head, on_thumb)) = hit {
            if head { if !self.collapsed.remove(&key) { self.collapsed.insert(key); } return self.apply_thumbs() }
            if on_thumb { return self.set_zoom(Some(thumb)) }
            return;
        }
        let Some(i) = self.hit(x, y) else { return };
        let t = &self.tiles[i];
        self.drag = Some(Drag { src: t.src, down: (x, y), grab: (x as f64 - t.cur.0, y as f64 - t.cur.1), moved: false, samples: Default::default() });
        unsafe { SetCapture(self.hwnd) };
    }

    // Lifted tile: scale springs to LIFT_SCALE (~0.25 s), position follows the (rubber-banded) pointer through a
    // stiff critically-damped spring; neighbours retarget with a small stagger.
    fn mouse_move(&mut self, x: i32, y: i32) {
        if self.scrub.is_some() {
            let ms = self.scrub_ms(x);
            self.scrub = Some(ms);
            music::seek(ms, false);
            return self.invalidate_player();
        }
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
        if let Some(ms) = self.scrub.take() {
            unsafe { ReleaseCapture() };
            music::seek(ms, !self.paused); // let go: carry on playing from there (a paused track stays paused there)
            return self.invalidate_player();
        }
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
            // chosen on purpose: the deck stops coming back over this Studio
            if let Some(seen) = self.seen.as_mut() { seen.insert(src as usize, 0); }
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
        if unsafe { IsWindowVisible(self.hwnd) } != 0 && unsafe { IsIconic(self.hwnd) } == 0 { self.settings["max"] = (unsafe { IsZoomed(self.hwnd) } != 0).into() }
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
            let gb = self.gal_btn();
            let mb = self.mus_btn();
            text(&summary, RECT { left: self.px(20), top: self.px(4), right: mb.or(gb).or(ub).map_or(pin.left, |u| u.left) - self.px(8), bottom: self.px(HEADER) }, self.f_body, MUTED, DT_LEFT | DT_VCENTER);
            if let Some(m) = mb {
                round(m, match (self.music, self.hdr == H_MUS) { (true, true) => rgb(40, 150, 255), (true, false) => rgb(10, 132, 255), (false, true) => CARD_HOVER, _ => CARD }, m.bottom - m.top);
                text(&self.mus_label(), m, self.f_body, TEXT, DT_CENTER | DT_VCENTER);
            }
            if let Some(g) = gb {
                // blue while open or holding images not seen yet
                let lit = self.gallery || self.gal_new();
                round(g, match (lit, self.hdr == H_GAL) { (true, true) => rgb(40, 150, 255), (true, false) => rgb(10, 132, 255), (false, true) => CARD_HOVER, _ => CARD }, g.bottom - g.top);
                text(&self.gal_label(), g, self.f_body, TEXT, DT_CENTER | DT_VCENTER);
            }
            if n > 0 {
                round(hb, if self.hdr == H_PAUSE { CARD_HOVER } else { CARD }, hb.bottom - hb.top);
                text(if self.all_paused() { "Resume all" } else { "Pause all" }, hb, self.f_body, TEXT, DT_CENTER | DT_VCENTER);
            }
            // pin: Segoe MDL2/Fluent "Pinned" (on) / "Pin" (off)
            if self.hdr == H_PIN { round(pin, CARD_HOVER, pin.bottom - pin.top) }
            let on = self.topmost_setting();
            text(if on { "\u{E840}" } else { "\u{E718}" }, pin, self.f_icon, if on { TEXT } else { DIM }, DT_CENTER | DT_VCENTER);
            if let (Some(u), Some(l)) = (ub, self.update_label()) {
                let fill = match (self.update.is_some(), self.hdr == H_UPD) { (true, true) => rgb(20, 92, 170), (true, false) => rgb(14, 70, 132), (false, true) => CARD_HOVER, _ => CARD };
                round(u, fill, u.bottom - u.top);
                text(&l, u, self.f_body, TEXT, DT_CENTER | DT_VCENTER);
            }
            if self.update_open && let Some(r) = &self.update {
                let p = RECT { left: self.px(16), top: self.px(HEADER), right: hb.right, bottom: self.px(HEADER + PANEL - 8) };
                round(p, CARD, self.px(14));
                let line = |i: i32, s: &str, f: HFONT, c: u32| text(s, RECT { left: p.left + self.px(16), top: p.top + self.px(10 + 18 * i), right: p.right - self.px(220), bottom: p.top + self.px(28 + 18 * i) }, f, c, DT_LEFT | DT_VCENTER);
                let head = if r.title == r.tag { format!("Studio Deck {}", r.tag) } else { format!("Studio Deck {}  \u{00B7}  {}", r.tag, r.title) };
                line(0, &head, self.f_title, TEXT);
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
            if self.zoom.is_some() { return } // enlarged thumbnail covers the rest
            if self.music {
                let (area, content, items) = self.music_layout();
                let saved = SaveDC(dc);
                IntersectClipRect(dc, area.left, area.top, area.right, area.bottom);
                for (it, r) in items {
                    if RectVisible(dc, &r) == 0 { continue } // outside this repaint (usually just the header's numbers)
                    match it {
                        MItem::Game(g) => text(&g, r, self.f_title, TEXT, DT_LEFT | DT_VCENTER),
                        MItem::Agent(a) => text(&a, r, self.f_body, rgb(120, 180, 255), DT_LEFT | DT_VCENTER),
                        MItem::Track(i) => {
                            let t = &self.tracks[i];
                            let on = self.playing.as_ref() == Some(&t.path);
                            round(r, if on { rgb(10, 132, 255) } else { CARD }, self.px(8));
                            let icon = RECT { left: r.left + self.px(8), right: r.left + self.px(36), ..r };
                            // Segoe MDL2/Fluent: Pause on the playing card, Play elsewhere
                            text(if on && !self.paused { "\u{E769}" } else { "\u{E768}" }, icon, self.f_icon, if on { TEXT } else { rgb(120, 180, 255) }, DT_CENTER | DT_VCENTER);
                            let name = t.path.file_stem().unwrap_or_default().to_string_lossy().replace('_', " ");
                            let sub = t.path.parent().and_then(|d| d.file_name()).map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
                            let (l, rt) = (r.left + self.px(42), r.right - self.px(8));
                            text(&name, RECT { left: l, top: r.top + self.px(6), right: rt, bottom: r.top + self.px(28) }, self.f_title, TEXT, DT_LEFT | DT_VCENTER);
                            let when = if t.ms > 0 { format!("   \u{00B7}   {}", local_hhmm(t.ms)) } else { String::new() };
                            text(&format!("{sub}{when}"), RECT { left: l, top: r.top + self.px(28), right: rt, bottom: r.bottom - self.px(6) }, self.f_body, if on { TEXT } else { MUTED }, DT_LEFT | DT_VCENTER);
                        }
                    }
                }
                RestoreDC(dc, saved);
                let h = area.bottom - area.top;
                if content > h {
                    let bar = (h * h / content).max(self.px(24));
                    let y = area.top + ((h - bar) as i64 * self.mus_top as i64 / (content - h).max(1) as i64) as i32;
                    round(RECT { left: area.right + self.px(5), top: y, right: area.right + self.px(10), bottom: y + bar }, DIM, self.px(4));
                }
                if let Some((bar, btn, track)) = self.player() {
                    round(bar, CARD_HOVER, self.px(12));
                    match (&self.playing, &self.mus_msg) {
                        (Some(p), _) => {
                            round(btn, rgb(10, 132, 255), btn.bottom - btn.top);
                            text(if self.paused { "\u{E768}" } else { "\u{E769}" }, btn, self.f_icon, TEXT, DT_CENTER | DT_VCENTER);
                            let pos = self.scrub.unwrap_or_else(music::position).min(self.mus_len);
                            let name = p.file_stem().unwrap_or_default().to_string_lossy().replace('_', " ");
                            text(&name, RECT { left: track.left, top: bar.top + self.px(8), right: track.right, bottom: bar.top + self.px(32) }, self.f_title, TEXT, DT_LEFT | DT_VCENTER);
                            text(&format!("{} / {}", music::clock(pos), music::clock(self.mus_len)), RECT { left: track.right + self.px(12), right: bar.right - self.px(12), ..bar }, self.f_body, TEXT, DT_RIGHT | DT_VCENTER);
                            let mid = (track.top + track.bottom) / 2;
                            let line = RECT { left: track.left, top: mid - self.px(3), right: track.right, bottom: mid + self.px(3) };
                            round(line, DIM, self.px(6));
                            let x = track.left + ((track.right - track.left) as f64 * pos as f64 / self.mus_len.max(1) as f64) as i32;
                            round(RECT { right: x.max(line.left + self.px(6)), ..line }, rgb(10, 132, 255), self.px(6));
                            let k = self.px(8);
                            round(RECT { left: x - k, top: mid - k, right: x + k, bottom: mid + k }, TEXT, 2 * k);
                        }
                        (None, Some((m, _))) => text(m, bar, self.f_body, AMBER, DT_CENTER | DT_VCENTER),
                        _ => {}
                    }
                }
                return;
            }
            if self.gallery {
                let blit = |bmp: HBITMAP, bw: i32, bh: i32, r: RECT| {
                    let s = ((r.right - r.left) as f64 / bw as f64).min((r.bottom - r.top) as f64 / bh as f64);
                    let (w, h) = ((bw as f64 * s) as i32, (bh as f64 * s) as i32);
                    let (x, y) = (r.left + (r.right - r.left - w) / 2, r.top + (r.bottom - r.top - h) / 2);
                    let m = CreateCompatibleDC(dc);
                    let o = SelectObject(m, bmp);
                    SetStretchBltMode(dc, HALFTONE);
                    SetBrushOrgEx(dc, 0, 0, null_mut());
                    StretchBlt(dc, x, y, w, h, m, 0, 0, bw, bh, SRCCOPY);
                    SelectObject(m, o);
                    DeleteDC(m);
                };
                if self.grid {
                    let (area, content, _, cells) = self.grid_layout();
                    let saved = SaveDC(dc);
                    IntersectClipRect(dc, area.left - self.px(4), area.top, area.right + self.px(4), area.bottom);
                    for (i, r) in cells {
                        if RectVisible(dc, &RECT { left: r.left - 4, top: r.top - 4, right: r.right + 4, bottom: r.bottom + 4 }) == 0 { continue }
                        if i == self.gal_i { round(RECT { left: r.left - self.px(3), top: r.top - self.px(3), right: r.right + self.px(3), bottom: r.bottom + self.px(3) }, rgb(10, 132, 255), self.px(10)) }
                        round(r, CARD, self.px(8));
                        match self.thumbs.get(&self.gal[i]) {
                            Some(&(b, w, h)) if !b.is_null() => blit(b, w, h, RECT { left: r.left + self.px(4), top: r.top + self.px(4), right: r.right - self.px(4), bottom: r.bottom - self.px(4) }),
                            _ => text("\u{2026}", r, self.f_body, DIM, DT_CENTER | DT_VCENTER),
                        }
                    }
                    RestoreDC(dc, saved);
                    // scroll position, when there's more than fits
                    let h = area.bottom - area.top;
                    if content > h {
                        let bar = (h * h / content).max(self.px(24));
                        let y = area.top + ((h - bar) as i64 * self.gal_top as i64 / (content - h).max(1) as i64) as i32;
                        round(RECT { left: area.right + self.px(5), top: y, right: area.right + self.px(10), bottom: y + bar }, DIM, self.px(4));
                    }
                    return;
                }
                let (main, strip) = self.gal_layout();
                let Some(cur) = self.gal.get(self.gal_i) else { return };
                match (&self.big, self.thumbs.get(cur)) {
                    (Some((p, b, w, h)), _) if p == cur => blit(*b, *w, *h, main),
                    (_, Some(&(b, w, h))) if !b.is_null() => blit(b, w, h, main), // until the full one decodes
                    _ => text("Loading\u{2026}", main, self.f_body, DIM, DT_CENTER | DT_VCENTER),
                }
                let (name, ms) = gallery::parts(cur);
                let cap = RECT { left: main.left, top: main.bottom + self.px(4), right: main.right, bottom: main.bottom + self.px(30) };
                text(&format!("{name}   \u{00B7}   {}   \u{00B7}   {} / {}   \u{00B7}   Esc: all images", local_hhmm(ms), self.gal_i + 1, self.gal.len()), cap, self.f_body, MUTED, DT_CENTER | DT_VCENTER);
                for (i, r) in strip {
                    if i == self.gal_i { round(RECT { left: r.left - self.px(3), top: r.top - self.px(3), right: r.right + self.px(3), bottom: r.bottom + self.px(3) }, rgb(10, 132, 255), self.px(10)) }
                    round(r, CARD, self.px(8));
                    if let Some(&(b, w, h)) = self.thumbs.get(&self.gal[i]) && !b.is_null() {
                        blit(b, w, h, RECT { left: r.left + self.px(4), top: r.top + self.px(4), right: r.right - self.px(4), bottom: r.bottom - self.px(4) });
                    }
                }
                return;
            }
            // apps panel
            for (g, (card, _, rows)) in self.groups.iter().zip(self.group_rects()) {
                round(card, CARD, self.px(14));
                let head = RECT { left: card.left + self.px(12), top: card.top + self.px(6), right: card.right - self.px(12), bottom: card.top + self.px(32) };
                text("\u{25CF}", head, self.f_body, dot_color(&g.agents[0].status), DT_LEFT | DT_VCENTER);
                let n = g.agents.len();
                text(&format!("{n} agent{}", if n == 1 { "" } else { "s" }), head, self.f_body, MUTED, DT_RIGHT | DT_VCENTER);
                text(&g.label, RECT { left: head.left + self.px(16), right: head.right - self.px(64), ..head }, self.f_title, TEXT, DT_LEFT | DT_VCENTER);
                for (k, a) in g.agents.iter().enumerate().filter(|_| !self.collapsed.contains(&g.key)) {
                    let y = rows + self.px(ROW_H) * k as i32;
                    let row = RECT { left: head.left, top: y, right: head.right, bottom: y + self.px(20) };
                    text("\u{25CF}", row, self.f_body, dot_color(&a.status), DT_LEFT | DT_VCENTER);
                    let right = match a.progress {
                        Some(p) => format!("{:.0}%", p * 100.0),
                        None => { let m = now_ms().saturating_sub(a.updated.unwrap_or(0)) / 60_000; if m == 0 { "now".into() } else { format!("{m}m") } }
                    };
                    text(&right, row, self.f_body, DIM, DT_RIGHT | DT_VCENTER);
                    let what = if is_waiting(&a.status) && !a.waiting_on.is_empty() { format!("Waiting on: {}", a.waiting_on) } else { a.step.clone() };
                    let s = if what.is_empty() { a.name.clone() } else { format!("{}  \u{00B7}  {what}", a.name) };
                    text(&s, RECT { left: row.left + self.px(16), right: row.right - self.px(40), ..row }, self.f_body, if a.status == "done" { DIM } else { MUTED }, DT_LEFT | DT_VCENTER);
                    if let Some(p) = a.progress {
                        let bar = RECT { left: row.left + self.px(16), top: row.bottom + self.px(1), right: row.right, bottom: row.bottom + self.px(4) };
                        round(bar, rgb(52, 52, 56), self.px(3));
                        round(RECT { right: bar.left + ((bar.right - bar.left) as f64 * p) as i32, ..bar }, dot_color(&a.status), self.px(3));
                    }
                }
            }
            if self.tiles.is_empty() {
                text("No Studios open", RECT { right: cr.right - self.apps_w(), ..cr }, self.f_body, MUTED, DT_CENTER | DT_VCENTER);
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
// background mode shows the deck while at least this many Studios are open (setting "showWhen", default 2).
// Never below 2: with one Studio there is nothing to switch between, so the deck stays hidden (an old "showWhen":1 counts as 2).
const MIN_STUDIOS: usize = 2;
fn min_studios() -> usize { read_settings().get("showWhen").and_then(|n| n.as_u64()).map(|n| n.clamp(MIN_STUDIOS as u64, 9) as usize).unwrap_or(MIN_STUDIOS) }
// Below MIN_STUDIOS nothing shows it, not even --show or a user-touched window (it comes back at 2+).
// Between that and `min`, an opened/touched window or a waiting agent/toast keeps it up; at `min`+ it auto-shows unless dismissed.
fn want_visible(studios: usize, min: usize, opened: bool, stay: bool, dismissed: bool) -> bool {
    if studios < MIN_STUDIOS { false } else if opened { true } else if studios < min && !stay { false } else { !dismissed }
}
fn set_show_when(arg: Option<&String>) -> Result<(), String> {
    let n: u64 = arg.and_then(|a| a.parse().ok()).filter(|n| (2..=9).contains(n)).ok_or("usage: --show-when <2-9>")?;
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
            WM_TIMER if wp == PLAYER_TIMER => {
                // only while something moves: a playing track's position, or a note that's due to go
                if a.mus_msg.as_ref().is_some_and(|m| now_ms() > m.1) { a.mus_msg = None; InvalidateRect(h, null(), 0); }
                if a.playing.is_some() && !a.paused && a.scrub.is_none() {
                    if music::mode() == "stopped" { a.unload() } else { a.invalidate_player() }
                }
                0
            }
            WM_TOAST => {
                let q: Vec<_> = TOASTS.lock().unwrap().drain(..).collect();
                for (src, text, c) in q { a.toast(src as HWND, &text, c) }
                0
            }
            WM_SHOWDECK => {
                // a second `--show` launch: show until the user closes it (closing hides; background keeps running)
                (a.shown, a.dismissed) = (true, false);
                if !a.visible { a.tick(); } // shows it only with 2+ Studios open
                if !a.visible { return 0 }
                if IsIconic(h) != 0 { ShowWindow(h, SW_RESTORE); }
                SetForegroundWindow(h);
                0
            }
            WM_TIMER => { a.tick(); 0 }
            WM_SIZE => {
                // full screen on/off is remembered right away (the window can be killed before it hides)
                if (wp == SIZE_MAXIMIZED as usize) != (a.settings["max"].as_bool() == Some(true)) && wp != SIZE_MINIMIZED as usize { a.save_settings() }
                a.layout(true);
                0
            }
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
            // right-click a Studio tile: enlarge it (any click or Esc returns)
            gallery::WM_GALLERY => { a.gal_decoded(); 0 }
            // grid: arrows move the selection (up/down a row), Enter opens it, wheel scrolls; big view: arrows/wheel step,
            // Esc back to the grid; Esc on the grid closes the gallery
            WM_KEYDOWN if a.gallery && [VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN, VK_RETURN, VK_ESCAPE, VK_HOME, VK_END].contains(&(wp as u16)) => {
                let i = a.gal_i as isize;
                let row = if a.grid { a.grid_layout().2 as isize } else { 1 };
                match wp as u16 {
                    VK_ESCAPE if a.grid => a.set_gallery(false),
                    VK_ESCAPE => { a.grid = true; a.gal_show() }
                    VK_RETURN => { a.grid = false; a.gal_show() }
                    VK_LEFT => a.gal_go(i - 1), VK_RIGHT => a.gal_go(i + 1),
                    VK_UP => a.gal_go(i - row), VK_DOWN => a.gal_go(i + row),
                    VK_HOME => a.gal_go(0), _ => a.gal_go(isize::MAX),
                }
                0
            }
            WM_MOUSEWHEEL if a.gallery => {
                let step = -(((wp >> 16) as i16) as i32).signum();
                if a.grid { a.gal_scroll(step) } else { a.gal_go(a.gal_i as isize + step as isize) }
                0
            }
            WM_RBUTTONDOWN if a.gallery || a.music => 0,
            WM_KEYDOWN if a.music && wp as u16 == VK_ESCAPE => { a.set_music(false); 0 }
            WM_MOUSEWHEEL if a.music => { a.mus_scroll(-(((wp >> 16) as i16) as i32).signum()); 0 }
            WM_RBUTTONDOWN => {
                let (x, y) = xy(lp);
                let z = if a.zoom.is_some() { None } else { a.hit(x, y).map(|i| a.tiles[i].thumb) };
                a.set_zoom(z);
                0
            }
            WM_KEYDOWN if wp == VK_ESCAPE as usize && a.zoom.is_some() => { a.set_zoom(None); 0 }
            WM_CAPTURECHANGED if a.drag.is_some() => { a.mouse_up(i32::MIN, i32::MIN); 0 }
            WM_MOUSEMOVE => {
                let (x, y) = xy(lp);
                if a.drag.is_some() || a.scrub.is_some() { a.mouse_move(x, y); return 0 }
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
        Some("--mcp-hook") => Some(mcp_hook()),
        Some("--music-scan") => Some(music::scan()),
        Some("--image") => Some(args.get(2).ok_or_else(|| "usage: --image <path>".to_string()).and_then(|f| gallery::add(std::path::Path::new(f)))),
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
        // created maximised (still hidden) when it was full screen last time; showing it later keeps that
        let max = if settings["max"].as_bool() == Some(true) { WS_MAXIMIZE } else { 0 };
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW | max, r.left, r.top,
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
            update: None, update_open: false, update_msg: String::new(), checking: false, uptodate_until: 0, seen: None, splash_top: false, tip_want: None, tip: None, back: (null_mut(), 0, 0), groups: Vec::new(), zoom: None, collapsed: Default::default(),
            music: false, tracks: Vec::new(), tracks_at: None, mus_top: 0, playing: None, paused: false, scrub: None, mus_len: 0, mus_msg: None,
            grid: true, gal_top: 0, gallery: false, gal: gallery::list(), gal_i: 0, gal_seen: 0, thumbs: HashMap::new(), big: None, asked: Default::default(), gal_tx: gallery::worker(hwnd),
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

#[cfg(test)]
mod tests {
    use super::want_visible as v;
    #[test]
    fn music_and_modelling_show() {
        let a = |app: &str, status: &str, ago: u64| super::Agent { app: app.into(), status: status.into(), updated: Some(1_000_000_000 - ago),
            name: String::new(), step: String::new(), studio: None, waiting_on: String::new(), since: 0, resolve_on: String::new(), resolved: String::new(),
            src: Default::default(), place_id: String::new(), universe: String::new(), progress: None, window: String::new() };
        let now = 1_000_000_000;
        assert!(super::app_shows(&a("Blender", "running", 0), now));
        assert!(super::app_shows(&a("lmms", "running", 60_000), now));
        assert!(!super::app_shows(&a("lmms", "done", 0), now));             // finished
        assert!(!super::app_shows(&a("Blender", "running", 11 * 60_000), now)); // gone quiet
        assert!(!super::app_shows(&a("claude-in-chrome", "running", 0), now)); // not music/modelling
    }
    #[test]
    fn needs_two_studios() {
        for opened in [false, true] { for stay in [false, true] {
            assert!(!v(0, 2, opened, stay, false));
            assert!(!v(1, 2, opened, stay, false)); // one Studio: never, even --show / touched / waiting agent
        } }
        assert!(v(2, 2, false, false, false));  // auto-shows at 2
        assert!(!v(2, 2, false, false, true));  // unless the user closed it
        assert!(v(2, 3, true, false, false));   // opened by the user, below a higher showWhen
        assert!(v(2, 3, false, true, false));   // waiting agent keeps it up
        assert!(!v(2, 3, false, false, false));
    }
}
