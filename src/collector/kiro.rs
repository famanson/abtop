use super::{process, AgentCollector, SharedProcessData};
use crate::model::{AgentSession, LaunchSurface, SessionStatus};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// Collector for Kiro CLI (`kiro-cli chat`) sessions, and the `kiro-cli-chat acp`
/// sessions an app such as KiroCrew hosts.
///
/// Discovery: live `kiro-cli-chat chat` processes, each paired with the `<id>.json`
/// in `~/.kiro/sessions/cli` whose `<id>.lock` names the chat or a descendant, and
/// live `acp` processes no chat runs, each listed only once a lock claims it.
/// Thinking while the sibling `<id>.jsonl` changed or the chat or a child was over
/// 1 % CPU in the last 30 s, or the log's last message ends in a `toolUse`, else
/// Waiting. Only the log's mtime and the last line's block kinds from a 64 KiB tail
/// are read; never `title` or event data.
pub struct KiroCollector {
    sessions_dir: PathBuf,
    /// PID -> cwd, so macOS does not run one `lsof` per chat on every tick.
    process_cwds: HashMap<u32, String>,
    /// PID -> ms first seen, the start time of a chat with no session file.
    first_seen: HashMap<u32, u64>,
    /// PID -> last collect it was busy; a streaming child dips under 1 % for seconds.
    last_busy: HashMap<u32, Instant>,
    /// Session files, reread on the slow tick so ~/.kiro is not parsed every 2 s.
    cached_entries: Option<Vec<SessionEntry>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SessionFile {
    created_at: String,
    session_state: SessionState,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SessionState {
    conversation_metadata: ConversationMetadata,
    rts_model_state: ModelState,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ConversationMetadata {
    user_turn_metadatas: Vec<Turn>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Turn {
    input_token_count: u64,
    output_token_count: u64,
    cache_read_input_token_count: u64,
    cache_write_input_token_count: u64,
    context_usage_percentage: f64,
    metering_usage: Vec<Metering>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Metering {
    value: f64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ModelState {
    model_info: Option<ModelInfo>,
    /// Null until the first turn ends.
    context_usage_percentage: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ModelInfo {
    model_id: Option<String>,
    context_window_tokens: u64,
}

/// One line of the `.jsonl` event log: only the kinds are read.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Event {
    kind: String,
    data: EventData,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EventData {
    content: Vec<Block>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Block {
    kind: String,
}

/// `<id>.lock`: the pid is the chat's own, its `acp` child's or a hosted `acp`'s; missing reads as pid 0.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Lock {
    pid: u32,
    started_at: String,
}

struct SessionEntry {
    id: String,
    lock: Lock,
    file: SessionFile,
}

impl KiroCollector {
    pub fn new() -> Self {
        Self::with_sessions_dir(
            dirs::home_dir()
                .unwrap_or_default()
                .join(".kiro")
                .join("sessions")
                .join("cli"),
        )
    }

    pub fn with_sessions_dir(sessions_dir: PathBuf) -> Self {
        Self {
            sessions_dir,
            process_cwds: HashMap::new(),
            first_seen: HashMap::new(),
            last_busy: HashMap::new(),
            cached_entries: None,
        }
    }

    fn refresh_process_cwds(&mut self, pids: &[u32], force: bool) {
        let live: HashSet<u32> = pids.iter().copied().collect();
        self.process_cwds.retain(|pid, _| live.contains(pid));
        self.first_seen.retain(|pid, _| live.contains(pid));
        self.last_busy.retain(|pid, _| live.contains(pid));
        let now = chrono::Utc::now().timestamp_millis() as u64;
        for &pid in pids {
            self.first_seen.entry(pid).or_insert(now);
            if force || !self.process_cwds.contains_key(&pid) {
                // A failed lookup keeps the last cwd, or "" (a stub) if there never was one.
                match process::get_process_cwd(pid) {
                    Some(cwd) => {
                        self.process_cwds.insert(pid, cwd);
                    }
                    None => {
                        self.process_cwds.entry(pid).or_default();
                    }
                }
            }
        }
    }

    /// Session files, newest lock first; `None` when the dir cannot be read.
    fn read_entries(&self) -> Option<Vec<SessionEntry>> {
        let mut entries: Vec<SessionEntry> = fs::read_dir(&self.sessions_dir)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension()? != "json" {
                    return None;
                }
                let file: SessionFile =
                    serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
                let lock = fs::read_to_string(path.with_extension("lock"))
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                Some(SessionEntry {
                    id: path.file_stem()?.to_string_lossy().into_owned(),
                    lock,
                    file,
                })
            })
            .collect();
        entries.sort_by_cached_key(|e| std::cmp::Reverse(parse_ms(&e.lock.started_at)));
        Some(entries)
    }

    fn session(
        &self,
        pid: u32,
        cwd: &str,
        entry: Option<&SessionEntry>,
        launch_surface: LaunchSurface,
        shared: &SharedProcessData,
    ) -> AgentSession {
        let state = entry.map(|e| &e.file.session_state);
        let turns: &[Turn] = state
            .map(|s| s.conversation_metadata.user_turn_metadatas.as_slice())
            .unwrap_or_default();
        let model_info = state.and_then(|s| s.rts_model_state.model_info.as_ref());
        let context_window = model_info
            .map(|m| m.context_window_tokens)
            .filter(|&w| w > 0)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);

        let log = entry.map(|e| self.sessions_dir.join(format!("{}.jsonl", e.id)));
        let log_is_fresh = log
            .as_ref()
            .and_then(|l| fs::metadata(l).ok())
            .and_then(|m| m.modified().ok())
            .is_some_and(|t| t.elapsed().is_ok_and(|d| d < Duration::from_secs(30)));
        let busy = self
            .last_busy
            .get(&pid)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(30));
        // A running tool (`sleep 45`) moves neither the log nor the CPU; its block is last.
        let (status, task) = if log_is_fresh
            || busy
            || log.as_deref().and_then(last_event).is_some_and(|e| {
                e.kind == "AssistantMessage"
                    && e.data.content.last().is_some_and(|b| b.kind == "toolUse")
            }) {
            (SessionStatus::Thinking, "thinking...")
        } else {
            (SessionStatus::Waiting, "waiting for input")
        };
        let current_tasks = vec![task.to_string()];

        let project_name = process::last_path_segment(cwd).unwrap_or("?").to_string();

        AgentSession {
            agent_cli: "kiro",
            launch_surface,
            pid,
            session_id: entry
                .map(|e| e.id.clone())
                .unwrap_or_else(|| format!("kiro-{pid}")),
            cwd: cwd.to_string(),
            project_name,
            started_at: entry
                .and_then(|e| parse_ms(&e.file.created_at))
                .unwrap_or(self.first_seen[&pid]),
            status,
            model: model_info
                .and_then(|m| m.model_id.clone())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "-".to_string()),
            effort: String::new(),
            context_percent: state
                .and_then(|s| s.rts_model_state.context_usage_percentage)
                .unwrap_or(0.0),
            total_input_tokens: turns.iter().map(|t| t.input_token_count).sum(),
            total_output_tokens: turns.iter().map(|t| t.output_token_count).sum(),
            total_cache_read: turns.iter().map(|t| t.cache_read_input_token_count).sum(),
            total_cache_create: turns.iter().map(|t| t.cache_write_input_token_count).sum(),
            turn_count: turns.len() as u32,
            current_tasks,
            mem_mb: shared.process_info.get(&pid).map_or(0, |p| p.rss_kb / 1024),
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: turns
                .iter()
                .map(|t| t.input_token_count + t.output_token_count)
                .collect(),
            context_history: turns
                .iter()
                .map(|t| {
                    (t.context_usage_percentage * context_window as f64 / 100.0).round() as u64
                })
                .collect(),
            compaction_count: 0,
            context_window,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children: process::children_of(
                pid,
                &shared.children_map,
                &shared.process_info,
                &shared.ports,
            ),
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
            chat_messages: vec![],
            tool_calls: vec![],
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: vec![],
            credits: turns
                .iter()
                .flat_map(|t| &t.metering_usage)
                .map(|m| m.value)
                .sum(),
            config_root: super::abbrev_path(&self.sessions_dir),
        }
    }
}

impl Default for KiroCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentCollector for KiroCollector {
    fn collect(&mut self, shared: &SharedProcessData) -> Vec<AgentSession> {
        let (info, kids) = (&shared.process_info, &shared.children_map);
        let (chats, hosted) = find_session_pids(info);
        if chats.is_empty() && hosted.is_empty() {
            self.refresh_process_cwds(&[], false);
            return vec![];
        }
        // A failed read keeps the last good files rather than stubbing every chat.
        if shared.slow_tick || self.cached_entries.is_none() {
            if let Some(entries) = self.read_entries() {
                self.cached_entries = Some(entries);
            }
        }

        // A file belongs to the live process its lock pid is or descends from; the newer lock wins.
        let owners: HashSet<u32> = chats.iter().chain(&hosted).copied().collect();
        let mut file_by_pid: HashMap<u32, usize> = HashMap::new();
        for (i, entry) in self.cached_entries.iter().flatten().enumerate() {
            if let Some(pid) = claimant(entry.lock.pid, &owners, info) {
                file_by_pid.entry(pid).or_insert(i);
            }
        }
        // An `acp` no lock names is idle or abtop's own quota fetch, not a session.
        let hosted: HashSet<u32> = hosted
            .into_iter()
            .filter(|pid| file_by_pid.contains_key(pid))
            .collect();
        let pids: Vec<u32> = chats.into_iter().chain(hosted.iter().copied()).collect();

        self.refresh_process_cwds(&pids, shared.slow_tick);
        for &pid in &pids {
            if info[&pid].cpu_pct > 1.0 || process::has_active_descendant(pid, kids, info, 1.0) {
                self.last_busy.insert(pid, Instant::now());
            }
        }
        let entries = self.cached_entries.as_deref().unwrap_or_default();
        pids.iter()
            .map(|&pid| {
                let surface = if hosted.contains(&pid) {
                    LaunchSurface::App
                } else {
                    LaunchSurface::Cli
                };
                let entry = file_by_pid.get(&pid).map(|&i| &entries[i]);
                self.session(pid, &self.process_cwds[&pid], entry, surface, shared)
            })
            .collect()
    }
}

/// `kiro-cli-chat chat …` processes, then the `kiro-cli-chat acp` ones no chat runs:
/// those an app such as KiroCrew hosts, and abtop's own quota fetch. The subcommand
/// may be token 2 when the binary path holds a space, as `cmd_has_binary` allows.
fn find_session_pids(info: &HashMap<u32, process::ProcInfo>) -> (Vec<u32>, Vec<u32>) {
    let with_subcommand = |name: &str| -> Vec<u32> {
        info.iter()
            .filter(|(_, p)| {
                process::cmd_has_binary(&p.command, "kiro-cli-chat")
                    && p.command
                        .split_whitespace()
                        .skip(1)
                        .take(2)
                        .any(|t| t == name)
            })
            .map(|(pid, _)| *pid)
            .collect()
    };
    let chats = with_subcommand("chat");
    let chat_set: HashSet<u32> = chats.iter().copied().collect();
    let hosted = with_subcommand("acp")
        .into_iter()
        .filter(|&pid| claimant(pid, &chat_set, info).is_none())
        .collect();
    (chats, hosted)
}

/// The first of `pid` and its ancestors that is one of `owners`.
fn claimant(
    mut pid: u32,
    owners: &HashSet<u32>,
    info: &HashMap<u32, process::ProcInfo>,
) -> Option<u32> {
    let mut visited = HashSet::new();
    while pid > 1 && !owners.contains(&pid) && visited.insert(pid) {
        pid = info.get(&pid).map_or(0, |p| p.ppid);
    }
    owners.contains(&pid).then_some(pid)
}

fn parse_ms(rfc3339: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|t| t.timestamp_millis() as u64)
}

/// The event log's last line, from one tail read of 64 KiB and the newline before
/// it. A last line longer than 64 KiB gives `None`.
fn last_event(path: &Path) -> Option<Event> {
    const MAX_TAIL: u64 = 64 * 1024;
    let mut file = File::open(path).ok()?;
    let start = file.metadata().ok()?.len().saturating_sub(MAX_TAIL + 1);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_TAIL + 1).read_to_end(&mut buf).ok()?;
    let body = buf.trim_ascii_end();
    let line_start = match body.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None if start == 0 => 0,
        None => return None,
    };
    serde_json::from_slice(&body[line_start..]).ok()
}
