# abtop

AI agent monitor for your terminal. Like btop++, but for AI coding agents.

Supports Claude Code, Codex CLI, OpenCode and Kiro CLI sessions.

## Language Policy

English is mandatory for all project-facing work and communication.

- Write all source code, comments, tests, fixtures, documentation, examples, configuration text, scripts, and user-facing strings in English.
- Use English for every GitHub artifact: issue titles and bodies, issue comments, pull request titles and descriptions, review comments, commit messages, branch names, release notes, changelogs, discussions, labels, milestones, and workflow or CI messages.
- Do not use non-English text in repository content or GitHub communication unless it is an exact external identifier, a required protocol value, or a direct quote needed for context.
- When quoting or preserving non-English input, add an English explanation and keep the non-English text as short as possible.
- If a contributor opens an issue, comment, or review in another language, respond in English and continue the thread in English.

## Architecture

```
src/
├── main.rs                 # Entry, terminal setup, event loop, --setup flag
├── app.rs                  # App state, tick logic, key handling, summary generation
├── setup.rs                # StatusLine hook installation (abtop --setup)
├── ui/
│   └── mod.rs              # All panels in single file: header, context, quota,
│                           # tokens, projects, ports, sessions, footer
├── collector/
│   ├── mod.rs              # MultiCollector orchestration, orphan port detection
│   ├── claude.rs           # Claude Code: session discovery, transcript parsing
│   ├── codex.rs            # Codex CLI: session discovery via ps+lsof, JSONL parsing
│   ├── opencode.rs         # OpenCode: session discovery via ps + SQLite DB parsing
│   ├── kiro.rs             # Kiro CLI: session discovery via ps + ~/.kiro/sessions/cli JSON
│   ├── process.rs          # Child process tree (ps) + open ports (lsof) + git stats
│   └── rate_limit.rs       # Rate limit file reading (~/.claude/abtop-rate-limits.json)
└── model/
    ├── mod.rs              # Re-exports
    └── session.rs          # AgentSession, SessionStatus, RateLimitInfo,
                            # ChildProcess, OrphanPort, SubAgent
```

## Layout

```
┌─ ¹context (token rate sparkline + per-session context bars) ─────────┐
│  ▁▃▅▇█▇▅▃▁▃▅▇██                       S1 abtop       ████████ 82%  │
│  token rate (200pt history)            S2 prediction  █████████91%⚠ │
│                                        S3 api-server  ███      22%  │
└──────────────────────────────────────────────────────────────────────┘
┌─ ²quota ─────┐┌─ ³tokens ───┐┌─ projects ───┐┌─ ⁴ports ──────────┐
│ CLAUDE       ││ Total  1.2M ││ abtop        ││ PORT  SESSION  CMD │
│ 5h ████ 35%  ││ Input  402k ││  main +3 ~18 ││ :3000 api-srv node│
│   resets 2h  ││ Output  89k ││              ││ :8080 predict crgo│
│ 7d ██ 12%    ││ Cache  710k ││ prediction   ││                    │
│              ││ ▁▃▅▇█▇▅▃▁▃▅││  feat/x +1~2 ││ ORPHAN PORTS       │
│ CODEX        ││ Turns: 48   ││              ││ :4000 old-prj node│
│ 5h █ 9%     ││ Avg: 25k/t  ││ api-server   ││                    │
│ 7d ██ 14%    ││             ││  main ✓clean ││                    │
└──────────────┘└─────────────┘└──────────────┘└────────────────────┘
┌─ ⁵sessions ─────────────────────────────────────────────────────────┐
│ ►*CC 7336 abtop  ● Work opus  82% 1.2M  48  Edit src/pay.rs       │
│  >CD 8840 pred   ◌ Wait sonn  91% 340k  12  waiting                │
│ ─────────────────────────────────────────────────────────────────── │
│  SESSION 7336 · /Users/graykode/abtop                               │
│  Stripe payment integration...                                      │
│  └─ Edit src/pay.rs                                                 │
│  CHILDREN: 7401 cargo build                                         │
│  SUBAGENTS: explore-data ✓12k · run-tests ●8k                      │
│  MEM 4f · 12/200 │ v2.1.86 · 47m                                   │
└──────────────────────────────────────────────────────────────────────┘
```

Panel rendering priority (top to bottom):
1. **Sessions** — always visible, gets priority allocation (min 5 rows, ideal = 2/session + 7)
2. **Mid-tier** (quota, tokens, projects, ports) — split equally, shown if space allows
3. **Context** — only renders when sessions have ideal height AND surplus >= 5 rows
4. **Header** (1 row) + **Footer** (1 row) — always present

Panel descriptions:
- **¹context**: Left = token rate braille sparkline (200-point history). Right = per-session context % bars with yellow/red warning.
- **²quota**: Claude + Codex + Kiro rate limit gauges side-by-side (5h and 7d windows for Claude and Codex, a 30-day credit window for Kiro, each with a reset countdown). Kiro's monthly credit window is read from `kiro-rate-limits.json` in abtop's cache dir (`~/Library/Caches/abtop` on macOS, `~/.cache/abtop` on Linux). Every 5 minutes, timed from the last attempt, the TUI's `tick` refreshes it: it spawns `kiro-cli acp --agent-engine=v3 --auth-method=cli`, sends JSON-RPC `initialize`, then `_kiro/account/getUsage` (sent before initialize's reply, it fails authentication), and writes `usageBreakdowns[0].percentage` (a number) and `billingCycleReset` + `T00:00:00Z` into the `seven_day` slot with `window_minutes` 43200. An attempt is capped at 20 s, and the child's process group (its process tree on Windows) is killed after each attempt and when abtop exits; a failed, timed-out or malformed reply leaves the last cache. `--once`, `--json` and `tick_no_summaries` only read the cache, which ages while no TUI runs; `hidden_agents = ["kiro"]` turns the fetch off. Known limits: an abtop hard-killed mid-fetch leaves the child until it reads stdin EOF (1-5 s with the real kiro-cli; a wedged one outlives abtop), and every launch fetches once (the throttle is per process). Do not add an OpenCode row unless OpenCode exposes a reliable account-level provider rate-limit source. In a column under 12 cells the rows are compact: no bar, no hint, the countdown's largest unit with its `in`/`还有` prefix when that fits; a label and percent that overflow the column (zh `30天100%` in 7 cells) show the percent alone, so 5h and 30-day rows are told apart by order; a row that fills the column exactly (`5h100%` in 6 cells, `5时100%` in 7, zh `还有 3时` in 8, the `CLAUDE` header in 6) touches the next column. A wide column's hint shows only when it fits whole: the en KIRO hint `kiro-cli login` needs 15 cells.
- **³tokens**: Total token breakdown (in/out/cache) + per-turn sparkline for selected session.
- **projects** (always visible): Per-project git branch + added/modified file counts.
- **⁴ports**: Agent-spawned open ports + orphan ports (from dead sessions). Conflict detection.
- **⁵sessions**: Full-width panel below mid row. Session list table (top) + selected session detail (bottom), separated by divider.

## Data Sources

All read-only from local filesystem + `ps` + `lsof`. No API calls, no auth.

### 1. Claude Code session discovery: process + config-root mapping

Discovery strategy:
1. Find running `claude` processes via `ps`
2. Map PID → open files/directories via `lsof`
3. Infer Claude config roots from open paths that contain `sessions/` and `projects/`
4. Read `{config-root}/sessions/{PID}.json`, falling back to scanning session files for the matching embedded PID
5. Parse `{config-root}/projects/{encoded-path}/{sessionId}.jsonl`

Fallback config roots are still scanned: `~/.claude`, direct home profile roots matching `~/.claude-*` when they contain both `sessions/` and `projects/`, `claude_config_dirs` from `~/.config/abtop/config.toml`, abtop's own `CLAUDE_CONFIG_DIR`, and on Linux any `CLAUDE_CONFIG_DIR` read from `/proc/{pid}/environ`.

Session file format:
```json
{ "pid": 7336, "sessionId": "2f029acc-...", "cwd": "/Users/graykode/abtop", "startedAt": 1774715116826, "kind": "interactive", "entrypoint": "cli" }
```
- ~170 bytes. Created on start, deleted on exit.
- Verify PID alive with shared `ps` data containing a `claude` binary.
- Skip sessions whose PID descends from abtop's own `claude --print` summary children without hiding user-spawned non-interactive sessions.

### 2. Claude Code transcript: `{config-root}/projects/{encoded-path}/{sessionId}.jsonl`
Path encoding: `/Users/foo/bar` → `-Users-foo-bar`

Key line types:

**`assistant`** (tokens, model, tools):
```json
{
  "type": "assistant",
  "timestamp": "2026-03-28T15:25:55.123Z",
  "message": {
    "model": "claude-opus-4-6",
    "stop_reason": "end_turn",
    "usage": {
      "input_tokens": 2,
      "output_tokens": 5,
      "cache_read_input_tokens": 11313,
      "cache_creation_input_tokens": 4350
    },
    "content": [
      { "type": "text", "text": "..." },
      { "type": "tool_use", "name": "Edit", "input": { "file_path": "src/main.rs", ... } }
    ]
  }
}
```

**`user`** (prompts, version):
```json
{ "type": "user", "timestamp": "...", "version": "2.1.86", "gitBranch": "main", "message": { "role": "user", "content": "..." } }
```

**`last-prompt`** (session tail marker):
```json
{ "type": "last-prompt", "lastPrompt": "...", "sessionId": "..." }
```

- **Size: 1KB–18MB**. Append-only, new line per message.
- **Reading strategy**: On first discovery, scan full file to build cumulative token totals. Then watch file size — on growth, read only new bytes appended since last read (track file offset). This gives both lifetime totals and real-time updates without re-reading.
- **Partial line handling**: new bytes may end mid-JSON-line. Buffer incomplete lines until next read.
- **File rotation**: if file shrinks (session restart), reset offset to 0 and re-scan.

### 3. Codex CLI sessions: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`

Discovery strategy:
1. Find running `codex` processes via `ps`
2. Map PID → open `rollout-*.jsonl` file via `lsof`
3. Parse JSONL for `session_meta`, `token_count` (includes rate_limits), `agent_message` events
4. Detect finished sessions: scan today's directory for JSONL < 5 min old not owned by running process

Rate limits extracted from `token_count` events:
```json
{
  "rate_limits": {
    "limit_id": "codex",
    "primary": { "used_percent": 9.0, "window_minutes": 300, "resets_at": 1774686045 },
    "secondary": { "used_percent": 14.0, "window_minutes": 10080, "resets_at": 1775186466 },
    "plan_type": "plus"
  }
}
```

### 4. OpenCode sessions: `~/.local/share/opencode/opencode.db`
- Discover running `opencode` processes via shared `ps` data.
- Read recent sessions from OpenCode's SQLite DB through `sqlite3 -readonly -json`.
- Match live PIDs to DB sessions by process cwd. OpenCode does not expose a PID/session mapping, so when multiple DB rows share one cwd, only live PIDs should be assigned and older rows should not be shown as live duplicates.
- OpenCode contributes session/token/project/port data, but not quota data.

### 4a. Kiro CLI sessions: `~/.kiro/sessions/cli/<id>.json`
- Discover running `kiro-cli-chat chat …` processes via shared `ps` data. The `acp` child and abtop's own quota `kiro-cli acp` are not chats.
- Status is Thinking when the `<id>.jsonl` mtime is under 30 s old, its last line is an `AssistantMessage` whose last block is `toolUse`, or within the last 30 s the chat's CPU or a descendant's was over 1 %. Otherwise it is Waiting. Only metadata and the last line's `kind`s (from a 64 KiB tail) are read: never the `title` or any event's text.
- Session files are read on the slow tick, and on every tick until a read succeeds. A directory read that fails keeps the last list; a file that fails to parse is left out until the next slow tick.
- Each file pairs with the live chat its `<id>.lock` names (`{"pid":<acp pid>,"started_at":…}`): the pid's parent chain is walked to a `kiro-cli-chat chat` PID, one hop headless, two under the TUI's bun. The cwd lookup (`lsof`, cached by PID) pairs nothing: it feeds the cwd column, the git stats and the project name, the cwd's last segment.
- A failed cwd lookup keeps the chat's last good cwd. A chat with no file whose lock walks to it is a stub `kiro-<pid>`; Kiro writes `.json` and `.lock` at launch, so this lasts until the slow tick after launch. A pre-turn `.json` (`agent_name`, `model_info` null) parses; its model reads `-`.
- Credits are summed from each turn's `metering_usage`, beside the per-turn token counts. The detail footer shows them when they are positive.
- Quota comes from the Kiro cache file (see ²quota).
- Known limit: a lock left by a killed tree names a dead pid and is never claimed. If that pid is reused under a live chat, the chat owns two files and the newer lock `started_at` wins, wrong only when the reused pid is a tool child of an older chat, for that child's life. `started_at` is not compared with a process start time: the `ps` data has none.
- Known limit: a Kiro build that writes no `.lock` shows every chat as a stub.
- Known limit: a last line over 64 KiB reads Waiting during a sleeping tool.
- Known limit: a chat in `/` has no cwd segment, so its project is blank.
- Known limit: on Windows, a binary path quoted with two spaces is not recognised by the two-token scan. On every platform, kill refuses, not kills, a `kiro-cli-chat chat` whose first argument is `acp`.

### 5. Subagents: `~/.claude/projects/{path}/{sessionId}/subagents/`
- `agent-{hash}.jsonl` — same JSONL format as main transcript
- `agent-{hash}.meta.json` — `{ "agentType": "general-purpose", "description": "..." }`

### 6. Process tree: `ps` + `lsof`
```bash
ps -eo pid,ppid,rss,%cpu,command    # All processes
lsof -i -P -n -sTCP:LISTEN         # Open ports
```
- Build parent→children map from ppid
- Map listening PID → parent agent PID → session

### 7. Git status per project
```bash
git -C {cwd} status --porcelain     # added/modified file counts
```

### 8. Memory status
- Path: `~/.claude/projects/{encoded-path}/memory/`
- Count files in directory + lines in `MEMORY.md`

### 9. Rate limit (Claude Code)

NOT in transcript JSONL. Collected via StatusLine mechanism.

`abtop --setup` automates this: creates a script at `~/.claude/abtop-statusline.sh` that writes rate limit JSON to `~/.claude/abtop-rate-limits.json`, and registers it in `~/.claude/settings.json`.

File format read by abtop:
```json
{
  "source": "claude",
  "five_hour": { "used_percentage": 35.0, "resets_at": 1774715000 },
  "seven_day": { "used_percentage": 12.0, "resets_at": 1775320000 },
  "updated_at": 1774714400
}
```
- Rejects stale data (> 10 minutes old).
- `rate_limits` only present for Pro/Max subscribers.
- Account-level metric, shared across all sessions.
- Show "—" when not configured or data unavailable.

### 10. Other files
- `~/.claude/stats-cache.json` — daily aggregates. Only updated on `/stats`, NOT real-time.
- `~/.claude/history.jsonl` — prompt history with sessionId.

## Session Status Detection

```
● Working  = PID alive + transcript mtime < 30s ago
◌ Waiting  = PID alive + transcript mtime > 30s ago
✗ Error    = PID alive + last assistant has error content
✓ Done     = PID dead (detected via kill(pid, 0) failure)
```

**Done detection**: session files are deleted on normal exit, but may linger briefly or survive crashes. When PID is dead but file exists, show as Done and clean up on next tick.

**PID reuse risk**: verify PID is still the expected agent process (Claude, Codex, or OpenCode) by checking `ps -p {pid} -o command=`. Don't trust PID alone.

Current task (2nd line under each session):
- Working → last `tool_use` name + first arg (e.g. `Edit src/main.rs`)
- Waiting → "waiting for user input"
- Error → last error message (truncated)
- Done → "finished {duration} ago"

**Known limitations** (all heuristic):
- Cannot distinguish model-thinking vs tool-executing vs rate-limit-waiting vs permission-prompt
- "Waiting" may be wrong if a long-running tool (cargo build, npm test) is running
- Status is best-effort, not authoritative

## Session Summary Generation

Each session gets a one-line summary title generated via `claude --print`:
- Spawned as background process with 10s timeout
- Rejects generic/empty output; falls back to sanitized first prompt (28 chars)
- Cached to `~/.cache/abtop/summaries.json` (persists across runs)
- Max 3 concurrent summary jobs, max 2 retries per session

## Context Window Calculation

Not provided in data files. Derive:
- **Window size**: hardcode by model name
  - `claude-opus-4-6` → 200,000 (default)
  - `claude-opus-4-6[1m]` → 1,000,000
  - `claude-sonnet-4-6` → 200,000
  - `claude-haiku-4-5` → 200,000
- **Current usage**: last `assistant` line's `input_tokens + cache_read_input_tokens`. `cache_creation_input_tokens` is intentionally excluded — on compaction turns the same tokens can be reported as both `cache_creation` *and* `cache_read`, and summing all three double-counts (#54). Matches Claude Code's own statusline and the Codex collector.
- **Percentage**: current_usage / window_size * 100
- **Warning**: yellow at 80%, red at 90%, ⚠ icon at 90%+

## Orphan Port Detection

Tracks child processes that have open ports. When a parent session dies but the child process remains alive and listening:
- Added to `orphan_ports` list automatically
- Displayed in ports panel under "ORPHAN PORTS" section
- Can be killed via `X` (Shift+X) with safety checks (fresh port scan + PID command verification before SIGKILL)

## Key Bindings

| Key | Action |
|-----|--------|
| `↑`/`↓` or `k`/`j` | Select session in list |
| `Enter` | Jump to session terminal (cmux / tmux / iTerm2) |
| `x` | Kill selected session (SIGKILL) |
| `X` | Kill all orphan ports |
| `q` | Quit |
| `r` | Force refresh |

## Tech Stack

- **Rust** (2021 edition)
- **ratatui** + **crossterm** for TUI
- **serde** + **serde_json** for JSON/JSONL parsing
- **chrono** for timestamp formatting
- **dirs** for home directory resolution
- **Polling intervals** (staggered to avoid freezes):
  - Session scan + transcript tail: every 2s
  - Process tree (ps): every 2s
  - Port scan (lsof) + git status + rate limits: every 10s (5 ticks)

## Commit Convention

```
<type>: <description>
```
Types: `feat`, `fix`, `refactor`, `docs`, `chore`

## Commands

```bash
cargo build                    # Build
cargo run                      # Run TUI
cargo run -- --once            # Print snapshot and exit
cargo run -- --setup           # Install StatusLine hook for rate limit collection
cargo run -- --exit-on-jump    # Quit after Enter-jumping to a session terminal (for popup overlays)
cargo test                     # Tests
cargo clippy                   # Lint
```

## Release Process

1. Pick the target semver version and update both `Cargo.toml` and `Cargo.lock`.
2. Verify the package locally:
   ```bash
   cargo test
   cargo clippy -- -D warnings
   cargo build --release
   cargo publish --dry-run
   ```
3. Commit and merge or push the version bump to `main`:
   ```bash
   git add Cargo.toml Cargo.lock
   git commit -m "chore: bump version to X.Y.Z"
   git push origin main
   ```
4. From a clean, up-to-date `main`, create and push an annotated release tag:
   ```bash
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push origin vX.Y.Z
   ```
5. Watch the tag-triggered workflows:
   ```bash
   gh run list --workflow Release --limit 5
   gh run list --workflow "Publish to crates.io" --limit 5
   ```
6. `release.yml` builds platform binaries, creates the GitHub Release, and updates the Homebrew formula.
7. `publish.yml` runs `cargo publish` to crates.io automatically.

**Do NOT run `cargo publish` or `gh release create` manually** — the CI workflows handle both.
**Do NOT push the tag before the version bump is on `main`.**
**Do NOT reuse a release tag after a failed publish; bump to a new patch version instead.**

## Non-Goals (v0.1)

- Gemini/Cursor support
- Cost estimation
- Remote/SSH monitoring
- Notifications/alerts

## Terminal Jump (`Enter`)

`Enter` focuses the terminal running the selected session's agent process.
The logic lives in `src/jump/` as a registry of `TerminalJumper` adapters
(one file per backend). `jumpers()` is the single ordered source of truth;
`resolve()` walks it and the first applicable adapter wins.

Each adapter returns a three-way `JumpAttempt`:
- `NotApplicable` — not this backend's terminal; try the next adapter.
- `Jumped` — focused successfully; stop.
- `Failed(msg)` — this backend owns the process but the focus command errored;
  stop and surface `"<backend>: <msg>"` in the status line.

Order (most specific first), mutually exclusive by controlling tty:

1. **cmux** (`jump/cmux.rs`) — reads `CMUX_WORKSPACE_ID` (a UUID cmux exports
   into every surface, inherited by the agent) from the process environment via
   `ps eww`, then `cmux select-workspace --workspace <uuid>`.
2. **tmux** (`jump/tmux.rs`) — only when abtop itself runs inside tmux (`$TMUX`).
   Maps PID → pane via `tmux list-panes -a -F '#{pane_pid} #{session_name}:#{window_index}.#{pane_index}'`
   + process-tree descent, then `switch-client` / `select-window` / `select-pane`.
   PID in no pane → `NotApplicable` (lets another backend try).
3. **iTerm2** (`jump/iterm2.rs`) — resolves the PID's controlling tty (`ps -o tty=`),
   then AppleScript selects the session whose `tty` matches and brings its
   window/app to the front. First call triggers a one-time macOS Automation
   permission prompt; until granted, `osascript` exits non-zero → `Failed`.

Parsing/registry logic is unit-tested in `jump/mod.rs`; the thin `ps`/`osascript`/
`tmux` I/O wrappers are verified manually.

## Privacy

abtop reads transcripts, prompts, tool inputs, and memory files. These may contain secrets.
- **`--once` output**: redact file contents from tool_use inputs. Show tool name + file path only, not content.
- **TUI mode**: show tool name + first arg (file path), never show file contents or prompt text in session list.
- **No network**: abtop never sends data anywhere. All local reads.
- **Exceptions**: summary generation calls `claude --print` and the Kiro quota fetch sends getUsage to `kiro-cli acp`, both locally (no network by abtop itself, but claude and kiro-cli may use their APIs).

## Gotchas

- **Transcript size**: 1KB–18MB. On first load, full scan for totals. After that, track file offset and read only new bytes. Buffer partial lines.
- **Session file deletion**: files disappear when Claude exits. Handle `NotFound` between scan and read.
- **stats-cache.json is stale**: only updated on `/stats` command. Don't use for live data.
- **Context window not in data**: must hardcode per model. Will break if Anthropic/OpenAI add new models.
- **Rate limit is account-level**: shared across all sessions. Don't show per-session.
- **Path encoding**: `/Users/foo/bar` → `-Users-foo-bar`. Used for transcript directory names.
- **Path encoding collision**: `-Users-foo-bar-baz` could be `/Users/foo/bar-baz` or `/Users/foo-bar/baz`. Use session JSON's `cwd` as source of truth.
- **lsof can be slow**: on macOS with many open files. Cache results, poll every 10s.
- **Child process tree**: `pgrep -P` only gets direct children. Build full tree from `ps -eo ppid`.
- **Port detection race**: a port can close between lsof and display. Show stale data gracefully.
- **Subagent directory may not exist**: only created when Agent tool is used. Check existence before scanning.
- **Undocumented internals**: all data sources are Claude Code/Codex implementation details, not stable APIs. Schema may change without notice. Defensive parsing with `serde(default)` everywhere.
- **Terminal size**: minimum 80x24. Panels degrade gracefully when small (context panel hidden first).
- **PID reuse in port cache**: invalidate cached ports when the set of tracked PIDs changes.
- **Rate limit staleness**: reject rate limit data older than 10 minutes.
- **`/clear` + multi-PID same cwd**: after `/clear`, Claude Code mints a new `sessionId` + `.jsonl` without rewriting `sessions/{PID}.json`. abtop overrides the stale sid by picking the newest transcript in the project dir, but this heuristic can't disambiguate ownership when two live `claude` PIDs share a cwd — so the override is disabled in that case and both sessions keep their original sid until exit. Use separate worktrees if live tracking is needed on both simultaneously.
