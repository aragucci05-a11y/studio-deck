// Self-update from GitHub releases (aragucci05-a11y/studio-deck). Network: only api.github.com + the release's
// asset downloads, via curl.exe (no console window). Only strictly newer versions are offered, so dev builds that are
// ahead never downgrade. The exe is verified against the release's studiodeck.exe.sha256 (Windows BCrypt SHA-256),
// then swapped in: running exe -> <name>.old.exe (Windows allows renaming a running exe), new exe -> same path.
use std::{path::{Path, PathBuf}, sync::atomic::{AtomicUsize, Ordering}};
use windows_sys::Win32::{Foundation::*, Security::Cryptography::*};

const API: &str = "https://api.github.com/repos/aragucci05-a11y/studio-deck/releases/latest";
pub static MUTEX: AtomicUsize = AtomicUsize::new(0); // single-instance mutex, released before a restart

#[derive(Clone)]
pub struct Release { pub tag: String, pub title: String, pub notes: Vec<String>, exe: String, sha: Option<String> }

pub fn semver(s: &str) -> Option<(u64, u64, u64)> {
    let mut p = s.trim().trim_start_matches('v').split('-').next()?.split('.').map(|n| n.parse::<u64>().ok());
    Some((p.next()??, p.next()??, p.next().flatten().unwrap_or(0)))
}
pub fn current() -> (u64, u64, u64) { semver(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0)) }

fn curl(args: &[&str]) -> Option<Vec<u8>> {
    use std::os::windows::process::CommandExt;
    let ua = format!("studiodeck/{}", env!("CARGO_PKG_VERSION"));
    let o = std::process::Command::new("curl.exe").args(["-sSfL", "-m", "120", "-A", &ua]).args(args)
        .creation_flags(0x0800_0000).output().ok()?; // CREATE_NO_WINDOW
    o.status.success().then_some(o.stdout)
}

// Latest release if strictly newer than this build (one try; callers retry).
pub fn newer() -> Result<Option<Release>, String> {
    let j: serde_json::Value = serde_json::from_slice(&curl(&[API]).ok_or("update check failed")?).map_err(|e| e.to_string())?;
    let tag = j["tag_name"].as_str().unwrap_or("").to_string();
    if semver(&tag).is_none_or(|v| v <= current()) { return Ok(None) }
    let asset = |n: &str| j["assets"].as_array()?.iter().find(|a| a["name"] == n)?["browser_download_url"].as_str().map(String::from);
    let notes = j["body"].as_str().unwrap_or("").lines().map(|l| l.trim().trim_start_matches(['#', '-', '*', ' ']).to_string())
        .filter(|l| !l.is_empty()).take(3).collect();
    Ok(Some(Release { title: j["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&tag).to_string(), tag, notes,
        exe: asset("studiodeck.exe").ok_or("release has no studiodeck.exe")?, sha: asset("studiodeck.exe.sha256") }))
}

pub fn sha256(data: &[u8]) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    let st = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, std::ptr::null(), 0, data.as_ptr(), data.len() as u32, out.as_mut_ptr(), 32) };
    (st == 0).then_some(out)
}
fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

// Download + verify + swap. Refuses without a checksum or on mismatch. Returns the installed exe path.
pub fn install(r: &Release) -> Result<PathBuf, String> {
    let sha_url = r.sha.as_deref().ok_or("release has no studiodeck.exe.sha256 - refusing")?;
    let want = String::from_utf8_lossy(&curl(&[sha_url]).ok_or("checksum download failed")?).split_whitespace().next().unwrap_or("").to_lowercase();
    if want.len() != 64 { return Err("bad checksum file - refusing".into()) }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let tmp = exe.with_extension("new.exe");
    let _ = std::fs::remove_file(&tmp);
    curl(&["-o", &tmp.to_string_lossy(), &r.exe]).ok_or("download failed")?;
    let data = std::fs::read(&tmp).map_err(|e| e.to_string())?;
    if sha256(&data).map(|h| hex(&h)).as_deref() != Some(want.as_str()) {
        let _ = std::fs::remove_file(&tmp);
        return Err("checksum mismatch - refusing".into());
    }
    swap(&exe, &tmp)?;
    Ok(exe)
}
fn old_path(exe: &Path) -> PathBuf { exe.with_file_name(format!("{}.old.exe", exe.file_stem().unwrap_or_default().to_string_lossy())) }
fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    let old = old_path(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("rename running exe: {e}"))?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(format!("move new exe: {e}"));
    }
    Ok(())
}
pub fn cleanup_old() { if let Ok(exe) = std::env::current_exe() { let _ = std::fs::remove_file(old_path(&exe)); } }

// Starts the (new) exe at our path with our args, then exits.
pub fn restart() -> ! {
    unsafe { CloseHandle(MUTEX.load(Ordering::Relaxed) as HANDLE) };
    if let Ok(exe) = std::env::current_exe() { let _ = std::process::Command::new(exe).args(std::env::args().skip(1)).spawn(); }
    std::process::exit(0)
}

#[cfg(test)]
#[test]
fn versions() {
    assert_eq!(semver("v0.2.0"), Some((0, 2, 0)));
    assert!(semver("v0.10.0") > semver("v0.9.9"));
    assert_eq!(semver("v1.2.3-beta"), Some((1, 2, 3)));
    assert_eq!(hex(&sha256(b"abc").unwrap()), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}
