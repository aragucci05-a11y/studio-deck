// Focus-free Studio control for agents: --shot / --click / --key / --scroll.
// Never activates, raises or moves the real cursor: PrintWindow for pixels, posted messages for input.
use std::{mem::zeroed, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    Storage::Xps::PrintWindow,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

pub fn run(cmd: &str, args: &[String]) -> Result<(), String> {
    let title = args.first().ok_or("missing <title>")?;
    let h = find(title)?;
    let num = |i: usize| -> Result<i32, String> { args.get(i).ok_or("missing number")?.parse().map_err(|_| format!("bad number: {}", args[i])) };
    match cmd {
        "--shot" => shot(h, args.get(1).ok_or("missing <out.png>")?, args.iter().any(|a| a == "--client")),
        "--click" => { click(h, num(1)?, num(2)?, args.get(3).map(String::as_str).unwrap_or("")); Ok(()) }
        "--scroll" => { scroll(h, num(1)?, num(2)?, num(3)?); Ok(()) }
        _ => key(h, args.get(1).ok_or("missing <vk-or-text>")?),
    }
}

fn find(title: &str) -> Result<HWND, String> {
    let t = title.to_lowercase();
    crate::find_studios().into_iter().find(|&h| crate::window_title(h).to_lowercase().contains(&t))
        .ok_or_else(|| format!("no Studio window matches \"{title}\""))
}

fn shot(h: HWND, out: &str, client: bool) -> Result<(), String> {
    unsafe {
        if IsIconic(h) != 0 { return Err("window is minimized".into()) }
        let mut r: RECT = zeroed();
        if client { GetClientRect(h, &mut r); } else { GetWindowRect(h, &mut r); }
        let (w, ht) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || ht <= 0 { return Err("window has no area".into()) }
        let mut bi: BITMAPINFO = zeroed();
        bi.bmiHeader = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: w, biHeight: -ht, biPlanes: 1, biBitCount: 32, ..zeroed() };
        let mut bits = null_mut();
        let dc = CreateCompatibleDC(null_mut());
        let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        let old = SelectObject(dc, bmp);
        // PW_RENDERFULLCONTENT (2) captures DX content; PW_CLIENTONLY (1).
        let ok = PrintWindow(h, dc, 2 | client as u32) != 0;
        let px = std::slice::from_raw_parts(bits as *const u8, (w * ht * 4) as usize);
        let rgba: Vec<u8> = px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
        SelectObject(dc, old);
        DeleteObject(bmp);
        DeleteDC(dc);
        if !ok { return Err("PrintWindow failed".into()) }
        let f = std::fs::File::create(out).map_err(|e| e.to_string())?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, ht as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.write_header().and_then(|mut wr| wr.write_image_data(&rgba)).map_err(|e| e.to_string())
    }
}

// Deepest visible child under a client point of `h`; returns it and the point in its client coords.
fn target(h: HWND, x: i32, y: i32) -> (HWND, POINT) {
    let (mut cur, mut pt) = (h, POINT { x, y });
    loop {
        let c = unsafe { ChildWindowFromPointEx(cur, pt, CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT) };
        if c.is_null() || c == cur { return (cur, pt) }
        unsafe { MapWindowPoints(cur, c, &mut pt, 1) };
        cur = c;
    }
}

fn lp(p: POINT) -> LPARAM { ((p.y as u16 as isize) << 16) | p.x as u16 as isize }

fn click(h: HWND, x: i32, y: i32, kind: &str) {
    let (t, p) = target(h, x, y);
    let (down, up, dbl, mk) = if kind == "right" { (WM_RBUTTONDOWN, WM_RBUTTONUP, WM_RBUTTONDBLCLK, 2u32) } else { (WM_LBUTTONDOWN, WM_LBUTTONUP, WM_LBUTTONDBLCLK, 1u32) }; // MK_RBUTTON / MK_LBUTTON
    let post = |m: u32, wp: u32| unsafe { PostMessageW(t, m, wp as WPARAM, lp(p)) };
    post(WM_MOUSEMOVE, 0);
    post(down, mk);
    post(up, 0);
    if kind == "double" { post(dbl, mk); post(up, 0); }
}

fn scroll(h: HWND, x: i32, y: i32, delta: i32) {
    let (t, _) = target(h, x, y);
    let mut sp = POINT { x, y };
    unsafe {
        ClientToScreen(h, &mut sp); // WM_MOUSEWHEEL carries screen coordinates
        PostMessageW(t, WM_MOUSEWHEEL, ((delta as i16 as u16 as usize) << 16) as WPARAM, lp(sp));
    }
}

fn vk_named(k: &str) -> Option<u16> {
    let k = k.to_lowercase();
    if let Some(hex) = k.strip_prefix("0x") { return u16::from_str_radix(hex, 16).ok() }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()).filter(|n| (1..=24).contains(n)) { return Some(0x6F + n) }
    Some(match k.as_str() {
        "enter" | "return" => 0x0D, "esc" | "escape" => 0x1B, "tab" => 0x09, "space" => 0x20, "backspace" => 0x08,
        "delete" | "del" => 0x2E, "left" => 0x25, "up" => 0x26, "right" => 0x27, "down" => 0x28,
        "home" => 0x24, "end" => 0x23, "pageup" => 0x21, "pagedown" => 0x22,
        _ => return None,
    })
}

// Named key / 0xNN -> WM_KEYDOWN/UP; anything else is typed as text with WM_CHAR. Goes to the thread's focus window.
fn key(h: HWND, k: &str) -> Result<(), String> {
    unsafe {
        let tid = GetWindowThreadProcessId(h, null_mut());
        let mut gi: GUITHREADINFO = zeroed();
        gi.cbSize = size_of::<GUITHREADINFO>() as u32;
        let t = if GetGUIThreadInfo(tid, &mut gi) != 0 && !gi.hwndFocus.is_null() { gi.hwndFocus } else { h };
        match vk_named(k) {
            Some(vk) => {
                let sc = (MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as isize) << 16;
                PostMessageW(t, WM_KEYDOWN, vk as WPARAM, 1 | sc);
                PostMessageW(t, WM_KEYUP, vk as WPARAM, 1 | sc | 0xC000_0000);
            }
            None => for c in k.encode_utf16() { PostMessageW(t, WM_CHAR, c as WPARAM, 1); },
        }
        Ok(())
    }
}
