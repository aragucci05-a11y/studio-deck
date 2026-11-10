# Studio Deck — instructions for AI agents

Use these when you (an AI coding agent: Claude Code, Cursor, Codex, …) work with Roblox Studio on a machine that
has `studiodeck.exe`. All commands are silent on success (exit 0) and print one line to stderr on failure, so they
cost you almost no context. Do not read or screenshot the deck window yourself — it is for the human.

## Report what you are doing (start and end of a task)
```bash
studiodeck.exe --status <session> <project> '[{"name":"<task>","status":"running","step":"<optional>","studio":"<place>"}]'
studiodeck.exe --status <session> <project> '[{"name":"<task>","status":"done","studio":"<place>"}]'
```
- `<session>`: a short stable id for your session (one file per session; rewriting replaces it).
- `status`: `running` | `waiting` | `blocked` | `failed` | `done`.
- `studio`: a case-insensitive substring of the Studio window title (the place / file name) you are using.
- From PowerShell, pass `-` and pipe the JSON on stdin (inline JSON loses its quotes there).
- Waiting on something? Use `waiting` (or `blocked`) plus `waitingOn` — the tile shows an amber "Waiting" badge and
  "Waiting on: <text> · 4m". Start it with `user` when only the user can unblock you (brighter badge):
  `{"name":"Publish store buttons","status":"waiting","waitingOn":"user: publish in Studio","studio":"MyGame"}`
- Let the wait clear itself with `resolveOn` — the deck rewrites your entry to `"status":"done"` plus
  `"resolved":"Published · 14:02"` once it happens, so read your file back instead of asking the user:
  - `"resolveOn":"published:<universeId>"` — the experience was published after the wait began (public games API, polled every 30 s):
    `{"name":"Ship store buttons","status":"waiting","waitingOn":"user: publish","resolveOn":"published:1234567890","studio":"MyGame"}`
  - `"resolveOn":"file:<path>"` — that file's modified time passes the start of the wait (build / test output).
- Optional `"placeId"` / `"universeId"` on an entry let the deck confirm the user's Publish button ("Published ✓").

## Apps panel (Blender, LMMS, any MCP tool or agent workflow)
Add `"app"` to an entry and it shows in the deck's right-hand Apps panel, one card per app with a live thumbnail of
that app's window (matched by exe name), instead of on a Studio tile:
`{"name":"Rocket mesh","app":"Blender","status":"running","step":"bevel + UV","progress":0.6}`
- `progress`: 0..1 (or 0..100) draws a bar. `window`: exe-name substring for the thumbnail when it differs from the
  app name's first word (e.g. `"app":"claude-in-chrome","window":"chrome"`).
- Claude Code sessions report MCP calls automatically: a user-level `PreToolUse` hook (matcher `mcp__.*`) and a
  `Stop` hook run `studiodeck.exe --mcp-hook`, so every MCP server in use gets a card with the current tool as its
  step. You don't need to report those yourself; still report the task itself with `--status`.

## Images gallery
Every image a Claude Code session sends (SendUserFile) or looks at (Read of a .png/.jpg/…) is copied by the same hook
into the deck's gallery (header button "Images N", newest 200 kept), so deleting your screenshots afterwards is fine.
Other tools can add one with `studiodeck.exe --image <path>`. It opens on a grid of all of them; click one for the big view.

## Music gallery
Every audio file (mp3/wav/ogg/flac/m4a) that shows up in a Claude Code tool call's input or output (Bash, PowerShell,
Write, SendUserFile, LMMS MCP) is indexed by a `PostToolUse` hook, credited to that session's title (and the subagent's
task description). The deck's "Music N" button lists them by game (the folder under Music\ or Documents\), then by
agent; click a card to play/stop. `studiodeck.exe --music-scan` re-indexes from all past transcripts (~40 s).

## Respect "Paused" (before every Studio action batch)
The user can pause agents per Studio from the deck. Before each batch of actions on a Studio run
`studiodeck.exe --check <title>`: exit 0 = go; **exit 3 = paused** — stop, report status `waiting` with
`"waitingOn":"paused by user"`, and re-check every ~30 s. Scripts can use `--pause <title>` / `--resume <title>`.
From PowerShell, pipe it so it waits for the exit code: `studiodeck.exe --check MyPlace_Test1 | Out-Null; $LASTEXITCODE`.

## Show the deck
- If you run 2+ Studio instances and the deck is not already up, run `studiodeck.exe --show` (single instance).
- Never change the user's settings: don't run `--background`, `--show-when`, `--install`, `--uninstall` or `--update`
  unless the user asks. The deck's Save / Publish / Close buttons are for the human, not for you.

## Control Studio without stealing focus
Never use OS-level computer use (real mouse/keyboard) on Studio — it steals focus from the user and from other agents.
- Inside a running playtest: use your Roblox Studio MCP tools (screenshot, mouse, keyboard per Studio instance).
- Editor UI (ribbon, Explorer, dialogs):
  - `studiodeck.exe --shot <title> <out.png> [--client]` — screenshot, works while covered (not while minimised)
  - `studiodeck.exe --click <title> <x> <y> [right|double]` — client pixels, as in `--shot --client`
  - `studiodeck.exe --key <title> <enter|esc|tab|f5|0x41|text>`
  - `studiodeck.exe --scroll <title> <x> <y> <delta>`

## Parallel work
If your task would wait on a Studio another agent is using, start another (disposable) test Studio instead of
queueing. One agent per Studio; report each with its own `studio` value.

## Hung-plugin prompt
While the deck runs it answers Studio's "<plugin> is not responding — stop this plugin?" with **No**, so long MCP
calls are not killed. Still yield regularly in long-running plugin code (`task.wait()`), it keeps Studio responsive.
