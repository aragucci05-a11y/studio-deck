<p align="center"><img src="studiodeck.ico" width="96" alt="Studio Deck icon"></p>

<h1 align="center">Studio Deck</h1>

<p align="center">A tiny Windows panel that shows every open Roblox Studio as a live tile — with the task and status of the AI agent working in it — and lets agents drive multiple Studios in the background without stealing your focus.</p>

---

Running several Roblox Studio instances at once (one per AI agent, test place, or branch) gets messy fast: windows
pile up, agents fight over focus, and you can't tell which Studio is doing what. Studio Deck puts them all in one
calm window.

- **Live viewports.** Every open Studio as a tile, using DWM live thumbnails (composited by Windows — no screen
  capture, ~0% CPU).
- **Agent status under each tile.** Place name, RAM, a status dot and the current task, written by your agents
  (Claude Code, Cursor, Codex, scripts…) with one silent command.
- **Off unless you want it.** Opens with `--show`; optional background mode (a toggle, default off) waits invisibly
  and appears while 2+ Studios are open — no tray icon, nothing resident when off.
- **iPhone-style tiles.** Drag to reorder with spring animations; click to bring that Studio forward.
- **Focus-free control.** Screenshot a covered Studio window, click, type and scroll in it — without moving your
  cursor or changing the foreground window. Several agents can work in several Studios in parallel.
- **Never lose a busy plugin.** Studio's "<plugin> is not responding — stop this plugin?" box is answered
  **No** automatically in the background, so long-running MCP/test plugins aren't killed.
- **Tiny.** ~370 KB exe, ~2 MB private memory, 0% CPU when idle. Pure Rust + Win32, no web view.

## Install

Download `studiodeck.exe` from [Releases](../../releases), or build it:

```
cargo build --release        # -> target/release/studiodeck.exe
```

`studiodeck.exe --show` opens the window (closing it quits). Nothing stays resident unless you turn on
**background mode** — `studiodeck.exe --background on|off`, or the window's system menu (title-bar icon /
Alt+Space) → *Run in background*. With it on, `studiodeck.exe` waits invisibly (no tray icon) and appears while 2+
Studios are open; turning it off quits it within a second. Optional autostart: `--install` (also turns background
on; `--uninstall` turns both off). `--show-when <n>` sets how many open Studios make it appear (default 2;
`1` = whenever Studio is open).
Quit: `taskkill /im studiodeck.exe`.

## For AI agents

Point your agent at [AGENTS.md](AGENTS.md) (Claude Code, Cursor and Codex read it automatically when it's in the
workspace; otherwise add one line to your agent's global instructions telling it to follow it).

## Report agent status

Each agent/session writes its own status file — silent, instant, safe to call often:

```bash
studiodeck.exe --status my-session MyGame '[{"name":"Fix fog shader","status":"running","step":"running tests","studio":"MyPlace_Test1"}]'
```

- `status`: `running` (green) · `waiting` / `blocked` (amber) · `failed` (red) · anything else, e.g. `done` (grey).
- `studio`: case-insensitive substring of the Studio window title / place name — that tile shows the agent.
- Pass `-` instead of the JSON to read it from stdin (handy from PowerShell).
- Files live in `%LOCALAPPDATA%\studiodeck\status\<session>.json`; older than 30 min = dimmed, older than 2 h = ignored.

**Claude Code users:** add one line to `~/.claude/CLAUDE.md` so every session uses it:

```
When running 2+ Roblox Studio instances, start studiodeck.exe (no args) and at task start/end run
`studiodeck.exe --status <session> <project> '[{"name":"<task>","status":"running|done","studio":"<place>"}]'`.
```

## Control Studio without stealing focus

`<title>` = a case-insensitive substring of the Studio window title.

| Command | What it does |
|---|---|
| `--shot <title> <out.png> [--client]` | PNG of the window via `PrintWindow` — works while covered, includes the 3D viewport (not while minimised) |
| `--click <title> <x> <y> [right\|double]` | posts a click at client pixels (as in `--shot --client`) |
| `--key <title> <enter\|esc\|tab\|f5\|0x41\|text>` | posts named keys or types text |
| `--scroll <title> <x> <y> <delta>` | posts a mouse-wheel message |

Works for the editor UI (ribbon tabs, Explorer search, typing). Inside a running playtest, prefer an MCP server for
Roblox Studio (screenshots and input per Studio instance) — it is already focus-free.
Never use OS-level "computer use" on Studio when several agents share the machine: it moves the real cursor and
steals focus.

## Tips for multi-Studio agent setups

- Turn off **Studio Settings → Script Editor → Enable Script Analysis** if your place has large data modules: it can
  leak ~1 GB/min while idle ([DevForum thread](https://devforum.roblox.com/t/studio-leaks-1-gbmin-of-memory-while-idle-when-script-analysis-is-enabled-grows-to-100-gb-then-crashes/4743637)).
  It only takes effect when Studio starts.
- Give each agent its own disposable test place (e.g. built with Rojo) instead of sharing one Studio; restart a
  test Studio when its memory climbs.

## Requirements

Windows 10 or 11 (Windows 11 adds the Mica backdrop and rounded corners). Building needs Rust 1.85+ (edition 2024)
and the MSVC toolchain. `make_icon.py` (Python + Pillow) regenerates the icon.

## License

MIT — see [LICENSE](LICENSE). Not affiliated with Roblox Corporation.
