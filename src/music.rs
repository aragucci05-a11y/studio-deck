// Music gallery: every audio file a Claude Code session made or used, grouped by game (the folder under Music\ or
// Documents\) and then by agent (the session's title). Index: %LOCALAPPDATA%\studiodeck\music.json, fed by the hook
// (--mcp-hook on PostToolUse: paths in the tool's input/output) and backfilled once from the transcripts (--music-scan).
// Playback is MCI (mp3/wav play in the deck; anything else opens in the default player).
use std::path::{Path, PathBuf};
use windows_sys::Win32::Media::Multimedia::mciSendStringW;

const EXTS: [&str; 5] = ["mp3", "wav", "ogg", "flac", "m4a"];

#[derive(Clone)]
pub struct Track { pub path: PathBuf, pub game: String, pub agent: String, pub ms: u64 }

fn index_path() -> PathBuf { crate::deck_dir().join("music.json") }
fn is_audio(p: &str) -> bool { p.rsplit_once('.').is_some_and(|(_, e)| EXTS.contains(&e.to_lowercase().as_str())) }

// Absolute audio paths inside any string of a JSON value (bash commands, tool output, file_path fields...).
pub fn paths_in(v: &serde_json::Value, out: &mut Vec<PathBuf>) {
    match v {
        serde_json::Value::String(s) => {
            for tok in s.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '<' | '>' | '|')) {
                let t = tok.trim_end_matches(['.', ':']);
                if t.len() > 4 && t.as_bytes().get(1) == Some(&b':') && is_audio(t) { out.push(PathBuf::from(t.replace('/', "\\"))) }
            }
        }
        serde_json::Value::Array(a) => for x in a { paths_in(x, out) },
        serde_json::Value::Object(o) => for x in o.values() { paths_in(x, out) },
        _ => {}
    }
}

// "C:\Users\me\Music\MyGame-Audio\menu\x.mp3" -> "MyGame-Audio": the folder under Music\ or Documents\.
pub fn game_of(p: &Path) -> String {
    let parts: Vec<String> = p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    parts.iter().position(|c| c.eq_ignore_ascii_case("Music") || c.eq_ignore_ascii_case("Documents"))
        .and_then(|i| parts.get(i + 1)).filter(|g| parts.len() > 2 && !is_audio(g)).cloned().unwrap_or_else(|| "Other".into())
}

// The session's name: its last custom title / agent name, else its first 8 id characters.
pub fn title_of(transcript: &Path) -> String {
    // a subagent's transcript has a .meta.json beside it with the task's short description
    if let Some(d) = std::fs::read_to_string(transcript.with_extension("meta.json")).ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).and_then(|m| m["description"].as_str().map(String::from)) { return d }
    let text = std::fs::read_to_string(transcript).unwrap_or_default();
    let mut title = String::new();
    for line in text.lines().filter(|l| l.contains("\"customTitle\"") || l.contains("\"agentName\"")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(t) = v["customTitle"].as_str().or(v["agentName"].as_str()) { title = t.to_string() }
        }
    }
    if title.is_empty() { title = format!("session {}", transcript.file_stem().unwrap_or_default().to_string_lossy().chars().take(8).collect::<String>()) }
    title
}

fn read_index() -> Vec<serde_json::Value> {
    std::fs::read_to_string(index_path()).ok().and_then(|t| serde_json::from_str::<Vec<serde_json::Value>>(&t).ok()).unwrap_or_default()
}

// Adds paths that exist and aren't indexed yet (the first agent seen keeps the credit).
pub fn record(paths: &[PathBuf], agent: &str, ms: u64) -> Result<(), String> {
    let mut idx = read_index();
    let mut changed = false;
    for p in paths.iter().filter(|p| p.is_file()) {
        let key = p.to_string_lossy().to_lowercase();
        if idx.iter().any(|e| e["path"].as_str().is_some_and(|q| q.to_lowercase() == key)) { continue }
        idx.push(serde_json::json!({ "path": p.to_string_lossy(), "agent": agent, "ms": ms }));
        changed = true;
    }
    if !changed { return Ok(()) }
    std::fs::create_dir_all(crate::deck_dir()).map_err(|e| e.to_string())?;
    let tmp = index_path().with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::Value::Array(idx).to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, index_path()).map_err(|e| e.to_string())
}

// --music-scan: one pass over every Claude Code transcript (main sessions and subagents), oldest first.
pub fn scan() -> Result<(), String> {
    let root = PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(".claude").join("projects");
    let mut files: Vec<PathBuf> = Vec::new();
    for proj in std::fs::read_dir(&root).map_err(|e| e.to_string())?.flatten() {
        for e in std::fs::read_dir(proj.path()).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "jsonl") { files.push(p) }
            else if p.is_dir() { files.extend(std::fs::read_dir(p.join("subagents")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl"))) }
        }
    }
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.created()).ok());
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let mut found: Vec<PathBuf> = Vec::new();
        let mut first_ms = 0;
        for line in text.lines().filter(|l| EXTS.iter().any(|e| l.contains(&format!(".{e}")))) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let n = found.len();
            paths_in(&v, &mut found);
            if found.len() > n && first_ms == 0 { first_ms = v["timestamp"].as_str().and_then(crate::parse_iso).unwrap_or(0) }
        }
        if found.is_empty() { continue }
        // a subagent's transcript sits in <session>\subagents\: credit "<session title> · <agent>"
        let agent = match f.parent().filter(|d| d.ends_with("subagents")).and_then(|d| d.parent()) {
            Some(session) => format!("{} \u{00B7} {}", title_of(&session.with_extension("jsonl")), title_of(&f)),
            None => title_of(&f),
        };
        record(&found, &agent, first_ms)?;
    }
    Ok(())
}

// Indexed tracks that still exist, one per song (an mp3 and a wav of the same name show once, the mp3 kept),
// sorted by game, then agent, then time.
pub fn load() -> Vec<Track> {
    let mut v: Vec<Track> = Vec::new();
    for e in read_index() {
        let Some(p) = e["path"].as_str().map(PathBuf::from) else { continue };
        if !p.is_file() { continue }
        let stem = p.with_extension("");
        if let Some(t) = v.iter_mut().find(|t| t.path.with_extension("") == stem) {
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp3")) { t.path = p }
            continue
        }
        v.push(Track { game: game_of(&p), agent: e["agent"].as_str().unwrap_or("").to_string(), ms: e["ms"].as_u64().unwrap_or(0), path: p });
    }
    v.sort_by(|a, b| (&a.game, &a.agent, a.ms).cmp(&(&b.game, &b.agent, b.ms)));
    v
}

fn mci(cmd: &str) -> String {
    let mut buf = [0u16; 64];
    unsafe { mciSendStringW(crate::w(cmd).as_ptr(), buf.as_mut_ptr(), buf.len() as u32, std::ptr::null_mut()) };
    String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)])
}
pub fn stop() { mci("close deckmusic"); }
// Plays in the deck; false (opened in the default player instead) for formats MCI can't play.
pub fn play(p: &Path) -> bool {
    stop();
    let mp3wav = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp3") || x.eq_ignore_ascii_case("wav"));
    if mp3wav && mci(&format!("open \"{}\" type mpegvideo alias deckmusic", p.display())).is_empty() && mci("play deckmusic").is_empty() { return true }
    stop();
    let (verb, file) = (crate::w("open"), crate::w(&p.to_string_lossy()));
    unsafe { windows_sys::Win32::UI::Shell::ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
    false
}
pub fn playing() -> bool { mci("status deckmusic mode") == "playing" }

#[cfg(test)]
#[test]
fn finds_paths_and_games() {
    let v = serde_json::json!({ "command": "ffmpeg -i 'C:/Users/me/Music/MyGame-Audio/menu/a.wav' C:\\x\\b.MP3; echo done.", "n": [ "no.mp3", "D:/y/c.ogg." ] });
    let mut out = Vec::new();
    paths_in(&v, &mut out);
    assert_eq!(out, vec![PathBuf::from(r"C:\Users\me\Music\MyGame-Audio\menu\a.wav"), PathBuf::from(r"C:\x\b.MP3"), PathBuf::from(r"D:\y\c.ogg")]);
    assert_eq!(game_of(Path::new(r"C:\Users\me\Music\MyGame-Audio\menu\a.wav")), "MyGame-Audio");
    assert_eq!(game_of(Path::new(r"C:\Users\me\Documents\OtherGameMusic\x.mp3")), "OtherGameMusic");
    assert_eq!(game_of(Path::new(r"C:\Users\me\Music\x.mp3")), "Other");
}

