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

## Show the deck
- If you run 2+ Studio instances and the deck is not already up, run `studiodeck.exe --show` (single instance).
- Never change the user's settings: don't run `--background`, `--show-when`, `--install` or `--uninstall` unless the
  user asks.

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
