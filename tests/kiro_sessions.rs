#![cfg(not(windows))]

use abtop::app::App;
use abtop::collector::kiro::KiroCollector;
use abtop::collector::process::ProcInfo;
use abtop::collector::{AgentCollector, SharedProcessData};
use abtop::config::PanelVisibility;
use abtop::model::{AgentSession, SessionStatus};
use abtop::theme::Theme;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Terminal;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime};

const CHAT: &str = "/Users/x/.local/bin/kiro-cli-chat chat --no-interactive --agent x";
const SENTINEL_TITLE: &str = "SENTINEL-TITLE-DO-NOT-SHOW";
const SENTINEL_DATA: &str = "SENTINEL-DATA-DO-NOT-SHOW";

/// Live `sleep` children standing in for kiro processes; killed on drop, even on panic.
struct Procs {
    children: Vec<Child>,
    info: HashMap<u32, ProcInfo>,
    children_map: HashMap<u32, Vec<u32>>,
}

impl Procs {
    fn new() -> Self {
        Self {
            children: Vec::new(),
            info: HashMap::new(),
            children_map: HashMap::new(),
        }
    }

    fn spawn(&mut self, cwd: &Path, command: &str) -> u32 {
        let child = Command::new("sleep")
            .arg("30")
            .current_dir(cwd)
            .spawn()
            .unwrap();
        let pid = child.id();
        self.children.push(child);
        self.info.insert(
            pid,
            ProcInfo {
                pid,
                ppid: 1,
                rss_kb: 0,
                cpu_pct: 0.0,
                command: command.into(),
            },
        );
        pid
    }

    /// Ends the process but keeps its `ProcInfo`, as a `ps` snapshot taken just before.
    fn kill(&mut self, pid: u32) {
        let child = self.children.iter_mut().find(|c| c.id() == pid).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn shared(&mut self) -> SharedProcessData {
        SharedProcessData {
            process_info: std::mem::take(&mut self.info),
            children_map: std::mem::take(&mut self.children_map),
            ports: HashMap::new(),
            slow_tick: true,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: HashSet::new(),
            mcp_suppress: true,
            desktop_rollout_fd_map: HashMap::new(),
        }
    }
}

impl Drop for Procs {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A project dir and a sessions dir, both canonical (lsof reports /private/var on macOS).
struct Dirs {
    _tmp: tempfile::TempDir,
    project: PathBuf,
    sessions: PathBuf,
}

fn dirs() -> Dirs {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let project = root.join("my-project");
    let sessions = root.join("sessions");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    Dirs {
        _tmp: tmp,
        project,
        sessions,
    }
}

fn turn(input: u64, output: u64, read: u64, write: u64, pct: f64, credits: &[f64]) -> Value {
    json!({
        "loop_id": {"agent_id": {"name": "x", "parent_id": null, "rand": null}, "rand": 1},
        "input_token_count": input,
        "output_token_count": output,
        "cache_read_input_token_count": read,
        "cache_write_input_token_count": write,
        "context_usage_percentage": pct,
        "final_context_usage_percentage": pct,
        "metering_usage": credits.iter()
            .map(|v| json!({"value": v, "unit": "credit", "unitPlural": "credits"}))
            .collect::<Vec<_>>(),
    })
}

fn session_file(id: &str, cwd: &Path, updated_at: &str, turns: Vec<Value>) -> Value {
    json!({
        "session_id": id,
        "cwd": cwd.to_str().unwrap(),
        "created_at": "2026-01-02T03:04:05.678901Z",
        "updated_at": updated_at,
        "title": SENTINEL_TITLE,
        "session_created_reason": "subagent",
        "session_state": {
            "version": "v1",
            "conversation_metadata": {"user_turn_metadatas": turns},
            "rts_model_state": {
                "conversation_id": id,
                "model_info": {"model_id": "test-model-1", "model_name": "test-model-1", "context_window_tokens": 150000},
                "context_usage_percentage": 25.5
            },
            "agent_name": "my-agent"
        }
    })
}

fn write(dir: &Path, id: &str, file: &Value) {
    std::fs::write(dir.join(format!("{id}.json")), file.to_string()).unwrap();
}

const LOCKED_AT: &str = "2026-10-11T23:56:55.208404Z";
/// No process in a test's `ps` snapshot has this PID (macOS PIDs stop at 99998).
const DEAD_PID: u32 = 999_999;

/// `<id>.lock` naming `pid`, as Kiro writes it at launch.
fn lock(dir: &Path, id: &str, pid: u32, started_at: &str) {
    let lock = json!({"pid": pid, "started_at": started_at});
    std::fs::write(dir.join(format!("{id}.lock")), lock.to_string()).unwrap();
}

/// An `AssistantMessage` event whose content ends in a `kind` block (QA-2's shape).
fn event(kind: &str, data: &str) -> String {
    json!({"version": "v1", "kind": "AssistantMessage", "data": {"message_id": "m", "content": [
        {"kind": "text", "data": data},
        {"kind": kind, "data": data},
    ]}})
    .to_string()
}

/// A one-line event log, its last block `kind`, last modified `age` ago.
fn write_events(dir: &Path, id: &str, kind: &str, age: Duration) {
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, format!("{}\n", event(kind, SENTINEL_DATA))).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - age)
        .unwrap();
}

fn assert_no_leak(s: &AgentSession) {
    assert!(s.initial_prompt.is_empty() && s.first_assistant_text.is_empty());
    let debug = format!("{s:?}");
    assert!(
        !debug.contains(SENTINEL_TITLE) && !debug.contains(SENTINEL_DATA),
        "{debug}"
    );
}

const OLD: Duration = Duration::from_secs(120);

/// A chat with a session file, an optional `(kind, age)` event log, its CPU and its child's.
fn chat_status(
    events: Option<(&str, Duration)>,
    cpu_pct: f64,
    child_cpu: Option<f64>,
) -> AgentSession {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    if let Some((kind, age)) = events {
        write_events(&d.sessions, "sess-a", kind, age);
    }
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    procs.info.get_mut(&pid).unwrap().cpu_pct = cpu_pct;
    if let Some(cpu) = child_cpu {
        spawn_child(&mut procs, &d, pid, cpu);
    }
    let s = only(collect(&d, &mut procs));
    assert_no_leak(&s);
    s
}

fn spawn_child(procs: &mut Procs, d: &Dirs, pid: u32, cpu_pct: f64) -> u32 {
    let child = procs.spawn(&d.project, "bun");
    let info = procs.info.get_mut(&child).unwrap();
    (info.ppid, info.cpu_pct) = (pid, cpu_pct);
    procs.children_map.entry(pid).or_default().push(child);
    child
}

/// An idle `kiro-cli-chat acp` child of `parent`.
fn spawn_acp(procs: &mut Procs, d: &Dirs, parent: u32) -> u32 {
    let acp = spawn_child(procs, d, parent, 0.0);
    procs.info.get_mut(&acp).unwrap().command =
        "/Users/x/.local/bin/kiro-cli-chat acp --trust-all-tools".into();
    acp
}

/// Each chat's status after two collects on one collector, its child at `(first, second)` CPU.
fn statuses_after_two_collects(child_cpus: &[(f64, f64)]) -> Vec<SessionStatus> {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    write_events(&d.sessions, "sess-a", "text", OLD);
    let mut procs = Procs::new();
    let chats: Vec<(u32, u32)> = child_cpus
        .iter()
        .map(|_| {
            let pid = procs.spawn(&d.project, CHAT);
            (pid, spawn_child(&mut procs, &d, pid, 0.0))
        })
        .collect();
    let mut shared = procs.shared();
    let mut collector = KiroCollector::with_sessions_dir(d.sessions.clone());
    let mut sessions = Vec::new();
    for second in [false, true] {
        for (&(_, child), &(first_cpu, second_cpu)) in chats.iter().zip(child_cpus) {
            shared.process_info.get_mut(&child).unwrap().cpu_pct =
                if second { second_cpu } else { first_cpu };
        }
        sessions = collector.collect(&shared);
    }
    chats
        .iter()
        .map(|&(pid, _)| {
            sessions
                .iter()
                .find(|s| s.pid == pid)
                .unwrap()
                .status
                .clone()
        })
        .collect()
}

fn collect(d: &Dirs, procs: &mut Procs) -> Vec<AgentSession> {
    KiroCollector::with_sessions_dir(d.sessions.clone()).collect(&procs.shared())
}

fn only(sessions: Vec<AgentSession>) -> AgentSession {
    assert_eq!(sessions.len(), 1, "{sessions:#?}");
    sessions.into_iter().next().unwrap()
}

fn render(sessions: Vec<AgentSession>, theme: Theme) -> (String, Buffer) {
    let panels = PanelVisibility {
        context: false,
        quota: false,
        tokens: false,
        projects: false,
        ports: false,
        sessions: true,
        mcp: false,
    };
    let mut app = App::new_with_config_and_claude_dirs(theme, &[], panels, &[]);
    app.sessions = sessions;
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| abtop::ui::draw(f, &app)).unwrap();
    let backend = terminal.backend();
    (format!("{backend}"), backend.buffer().clone())
}

fn two_turns(d: &Dirs) -> Value {
    session_file(
        "sess-a",
        &d.project,
        "2026-01-02T04:00:00Z",
        vec![
            turn(100, 20, 300, 40, 10.0, &[0.4, 0.6]),
            turn(200, 30, 500, 60, 25.5, &[1.0]),
        ],
    )
}

#[test]
fn lists_a_chat_process_with_its_session_file_fields() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);

    let s = only(collect(&d, &mut procs));

    assert_eq!(s.agent_cli, "kiro");
    assert_eq!(s.pid, pid);
    assert_eq!(s.session_id, "sess-a");
    assert_eq!(s.cwd, d.project.to_str().unwrap());
    assert_eq!(s.project_name, "my-project");
    assert_eq!(s.model, "test-model-1");
    assert_eq!(s.context_window, 150_000);
    assert_eq!(s.context_percent, 25.5);
    assert_eq!(s.started_at, 1_767_323_045_678);
}

#[test]
fn sums_tokens_and_credits_across_turns() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);

    let s = only(collect(&d, &mut procs));

    assert_eq!(s.turn_count, 2);
    assert_eq!(
        (
            s.total_input_tokens,
            s.total_output_tokens,
            s.total_cache_read,
            s.total_cache_create
        ),
        (300, 50, 800, 100)
    );
    assert_eq!(s.token_history, vec![120, 230]);
    assert_eq!(s.context_history, vec![15_000, 38_250]);
    assert!((s.credits - 2.0).abs() < 1e-9, "{}", s.credits);
}

#[test]
fn the_project_is_the_cwd_folder_not_the_agent_name() {
    let d = dirs();
    let x = d.project.join("x");
    std::fs::create_dir(&x).unwrap();
    let mut file = session_file("sess-a", &x, "2026-01-02T04:00:00Z", vec![]);
    file["session_state"]["agent_name"] = json!("kiro_default");
    write(&d.sessions, "sess-a", &file);
    let mut procs = Procs::new();
    let pid = procs.spawn(&x, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);

    let s = only(collect(&d, &mut procs));

    assert_eq!(
        (s.session_id.as_str(), s.project_name.as_str()),
        ("sess-a", "x")
    );
}

#[test]
fn a_pre_turn_file_with_null_model_info_pairs() {
    let d = dirs();
    let mut file = session_file("sess-a", &d.project, "2026-01-02T04:00:00Z", vec![]);
    // Kiro's `.json` before turn 1 (QA-pre, live).
    file["agent_name"] = Value::Null;
    file["session_state"]["agent_name"] = Value::Null;
    file["session_state"]["rts_model_state"]["model_info"] = Value::Null;
    write(&d.sessions, "sess-a", &file);
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);

    let s = only(collect(&d, &mut procs));

    assert_eq!(
        (s.session_id.as_str(), s.model.as_str(), s.context_window),
        ("sess-a", "-", 200_000)
    );
}

#[test]
fn model_is_a_dash_when_model_id_is_absent() {
    let d = dirs();
    let mut file = two_turns(&d);
    file["session_state"]["rts_model_state"]["model_info"]
        .as_object_mut()
        .unwrap()
        .remove("model_id");
    write(&d.sessions, "sess-a", &file);
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);

    assert_eq!(only(collect(&d, &mut procs)).model, "-");
}

#[test]
fn a_chat_whose_event_log_changed_within_30s_is_thinking() {
    let s = chat_status(Some(("text", Duration::ZERO)), 0.0, None);
    assert_eq!(s.status, SessionStatus::Thinking);
}

#[test]
fn an_assistant_message_ending_in_text_is_waiting_with_an_old_log() {
    let s = chat_status(Some(("text", OLD)), 0.0, None);
    assert_eq!(s.status, SessionStatus::Waiting);
}

#[test]
fn a_busy_chat_with_an_old_event_log_is_thinking() {
    let s = chat_status(Some(("text", OLD)), 5.0, None);
    assert_eq!(s.status, SessionStatus::Thinking);
}

#[test]
fn a_child_over_1_pct_is_thinking() {
    let s = chat_status(Some(("text", OLD)), 0.0, Some(2.0));
    assert_eq!(s.status, SessionStatus::Thinking);
}

#[test]
fn a_child_over_1_pct_on_the_last_collect_keeps_thinking() {
    let statuses = statuses_after_two_collects(&[(1.1, 0.7)]);
    assert_eq!(statuses, [SessionStatus::Thinking]);
}

#[test]
fn a_child_at_0_7_pct_on_every_collect_is_waiting() {
    let statuses = statuses_after_two_collects(&[(0.7, 0.7)]);
    assert_eq!(statuses, [SessionStatus::Waiting]);
}

#[test]
fn a_second_chat_at_0_7_pct_is_waiting_while_the_first_keeps_thinking() {
    let statuses = statuses_after_two_collects(&[(1.1, 0.7), (0.7, 0.7)]);
    assert_eq!(statuses, [SessionStatus::Thinking, SessionStatus::Waiting]);
}

#[test]
fn a_tool_running_inside_the_last_assistant_message_is_thinking_with_an_old_log() {
    let s = chat_status(Some(("toolUse", OLD)), 0.0, None);
    assert_eq!(s.status, SessionStatus::Thinking);
}

/// The status of a chat whose old event log is a 200 KB line, then `last`.
fn status_after_a_long_line_and(last: &str) -> SessionStatus {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let log = d.sessions.join("sess-a.jsonl");
    let big = event("text", &"x".repeat(200_000));
    std::fs::write(&log, format!("{big}\n{last}\n")).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&log)
        .unwrap()
        .set_modified(SystemTime::now() - OLD)
        .unwrap();
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    let s = only(collect(&d, &mut procs));
    assert_no_leak(&s);
    s.status
}

#[test]
fn the_tail_keeps_a_last_line_that_ends_exactly_at_the_bound() {
    let last = event("toolUse", &"x".repeat(32_700));
    assert_eq!(last.len() + 1, 65_536);
    assert_eq!(status_after_a_long_line_and(&last), SessionStatus::Thinking);
}

#[test]
fn a_last_line_one_byte_over_the_bound_is_waiting() {
    let last = event("toolUse", &"x".repeat(32_700)).replacen(r#""m""#, r#""mm""#, 1);
    assert_eq!(last.len() + 1, 65_537);
    assert_eq!(status_after_a_long_line_and(&last), SessionStatus::Waiting);
}

#[test]
fn a_chat_whose_event_log_is_dated_in_the_future_is_waiting_when_idle() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    write_events(&d.sessions, "sess-a", "text", Duration::ZERO);
    std::fs::File::options()
        .write(true)
        .open(d.sessions.join("sess-a.jsonl"))
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);

    let s = only(collect(&d, &mut procs));

    assert_eq!(s.status, SessionStatus::Waiting);
}

#[test]
fn a_chat_without_an_event_log_is_waiting_when_idle() {
    assert_eq!(chat_status(None, 0.0, None).status, SessionStatus::Waiting);
}

#[test]
fn a_stub_is_waiting_when_idle_and_thinking_when_busy() {
    for (cpu_pct, expected) in [
        (0.0, SessionStatus::Waiting),
        (5.0, SessionStatus::Thinking),
    ] {
        let d = dirs();
        let mut procs = Procs::new();
        let pid = procs.spawn(&d.project, CHAT);
        procs.info.get_mut(&pid).unwrap().cpu_pct = cpu_pct;

        let s = only(collect(&d, &mut procs));

        assert_eq!(s.status, expected, "cpu {cpu_pct}");
        assert_no_leak(&s);
    }
}

#[test]
fn a_chat_without_a_session_file_is_a_stub() {
    let d = dirs();
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);

    let s = only(collect(&d, &mut procs));

    assert_eq!(s.session_id, format!("kiro-{pid}"));
    assert_eq!(s.project_name, "my-project");
    assert_eq!(s.model, "-");
    assert_eq!(
        (
            s.turn_count,
            s.total_input_tokens,
            s.total_output_tokens,
            s.credits
        ),
        (0, 0, 0, 0.0)
    );
}

#[test]
fn nothing_is_listed_without_a_chat_process() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    procs.spawn(&d.project, "sleep 30");

    assert!(collect(&d, &mut procs).is_empty());
}

#[test]
fn only_the_chat_subcommand_is_listed() {
    let d = dirs();
    let mut procs = Procs::new();
    procs.spawn(
        &d.project,
        "/Users/x/.local/bin/kiro-cli chat --no-interactive --agent x",
    );
    let chat = procs.spawn(&d.project, CHAT);
    procs.spawn(
        &d.project,
        "/Users/x/.local/bin/kiro-cli-chat acp --trust-all-tools --effort high",
    );
    procs.spawn(
        &d.project,
        "/Users/x/.local/bin/kiro-cli-chat acp --agent-engine=v3 --auth-method=cli",
    );

    assert_eq!(only(collect(&d, &mut procs)).pid, chat);
}

fn session_ids(sessions: &[AgentSession]) -> HashMap<u32, &str> {
    sessions
        .iter()
        .map(|s| (s.pid, s.session_id.as_str()))
        .collect()
}

#[test]
fn each_chat_claims_the_file_whose_lock_names_its_descendant() {
    let d = dirs();
    let mut procs = Procs::new();
    let pids = [procs.spawn(&d.project, CHAT), procs.spawn(&d.project, CHAT)];
    let (low, high) = (*pids.iter().min().unwrap(), *pids.iter().max().unwrap());
    let bun = spawn_child(&mut procs, &d, low, 0.0);
    let acp = spawn_acp(&mut procs, &d, bun);
    for (id, created, holder) in [
        ("a", "2026-01-03T00:00:00Z", acp),
        ("b", "2026-01-02T00:00:00Z", high),
    ] {
        let mut file = session_file(id, &d.project, "2026-01-03T00:00:00Z", vec![]);
        file["created_at"] = json!(created);
        write(&d.sessions, id, &file);
        lock(&d.sessions, id, holder, LOCKED_AT);
    }

    let sessions = collect(&d, &mut procs);

    assert_eq!(
        session_ids(&sessions),
        HashMap::from([(low, "a"), (high, "b")]),
        "{sessions:#?}"
    );
}

#[test]
fn a_headless_chats_lock_walks_one_hop() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    let acp = spawn_acp(&mut procs, &d, pid);
    lock(&d.sessions, "sess-a", acp, LOCKED_AT);

    let sessions = collect(&d, &mut procs);

    assert_eq!(session_ids(&sessions), HashMap::from([(pid, "sess-a")]));
}

#[test]
fn a_lock_naming_the_chat_itself_pairs() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);

    assert_eq!(only(collect(&d, &mut procs)).session_id, "sess-a");
}

#[test]
fn a_file_whose_owner_is_dead_is_never_claimed() {
    for own_file in [true, false] {
        let d = dirs();
        let mut procs = Procs::new();
        let pid = procs.spawn(&d.project, CHAT);
        let mut dead = session_file("dead", &d.project, "2026-01-03T00:00:00Z", vec![]);
        dead["created_at"] = json!("2026-01-03T00:00:00Z");
        write(&d.sessions, "dead", &dead);
        lock(&d.sessions, "dead", DEAD_PID, LOCKED_AT);
        if own_file {
            write(&d.sessions, "sess-a", &two_turns(&d));
            lock(&d.sessions, "sess-a", pid, LOCKED_AT);
        }

        let s = only(collect(&d, &mut procs));

        let expected = if own_file {
            "sess-a".to_string()
        } else {
            format!("kiro-{pid}")
        };
        assert_eq!(s.session_id, expected, "own file: {own_file}");
    }
}

#[test]
fn a_chat_with_a_file_but_no_lock_is_a_stub() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);

    assert_eq!(
        only(collect(&d, &mut procs)).session_id,
        format!("kiro-{pid}")
    );
}

#[test]
fn two_locks_naming_one_pid_give_the_chat_the_newer_lock() {
    let d = dirs();
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    let acp = spawn_acp(&mut procs, &d, pid);
    // `old` is created later, so only the lock's `started_at` can pick `new`.
    for (id, created, started_at) in [
        ("old", "2026-01-03T00:00:00Z", "2026-10-10T08:00:00.000000Z"),
        ("new", "2026-01-02T00:00:00Z", "2026-10-11T23:56:55.208404Z"),
    ] {
        let mut file = session_file(id, &d.project, "2026-01-03T00:00:00Z", vec![]);
        file["created_at"] = json!(created);
        write(&d.sessions, id, &file);
        lock(&d.sessions, id, acp, started_at);
    }

    assert_eq!(only(collect(&d, &mut procs)).session_id, "new");
}

#[test]
fn a_chat_in_the_root_dir_has_a_blank_name() {
    let d = dirs();
    let file = session_file("sess-a", Path::new("/"), "2026-01-03T00:00:00Z", vec![]);
    write(&d.sessions, "sess-a", &file);
    let mut procs = Procs::new();
    let pid = procs.spawn(Path::new("/"), CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);

    let s = only(collect(&d, &mut procs));

    assert_eq!(
        (
            s.session_id.as_str(),
            s.cwd.as_str(),
            s.project_name.as_str()
        ),
        ("sess-a", "/", "")
    );
}

#[test]
fn a_chat_whose_cwd_cannot_be_read_has_an_empty_cwd() {
    let d = dirs();
    let mut file = two_turns(&d);
    file.as_object_mut().unwrap().remove("cwd");
    write(&d.sessions, "sess-a", &file);
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    procs.kill(pid);

    let s = only(collect(&d, &mut procs));

    assert_eq!((s.session_id.as_str(), s.cwd.as_str()), ("sess-a", ""));
}

#[test]
fn the_slow_tick_refreshes_a_chats_cwd_and_keeps_it_when_the_lookup_fails() {
    let d = dirs();
    let other = d.project.with_file_name("other");
    std::fs::create_dir(&other).unwrap();
    let mut procs = Procs::new();
    let child = Command::new("sh")
        .args([
            "-c",
            "while [ ! -e go ]; do sleep 0.05; done; cd ../other && : > moved && exec sleep 30",
        ])
        .current_dir(&d.project)
        .spawn()
        .unwrap();
    let pid = child.id();
    procs.children.push(child);
    procs.info.insert(
        pid,
        ProcInfo {
            pid,
            ppid: 1,
            rss_kb: 0,
            cpu_pct: 0.0,
            command: CHAT.into(),
        },
    );
    let mut shared = procs.shared();
    write(&d.sessions, "sess-a", &two_turns(&d));
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    let mut collector = KiroCollector::with_sessions_dir(d.sessions.clone());
    let first = only(collector.collect(&shared));
    assert_eq!(
        (first.session_id.as_str(), Path::new(&first.cwd)),
        ("sess-a", d.project.as_path())
    );

    std::fs::write(d.project.join("go"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !other.join("moved").exists() {
        assert!(
            Instant::now() < deadline,
            "the chat did not cd to ../other within 5 s"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    shared.slow_tick = false;
    assert_eq!(only(collector.collect(&shared)).session_id, "sess-a");
    shared.slow_tick = true;
    let moved = only(collector.collect(&shared));
    procs.kill(pid);
    let dead = only(collector.collect(&shared));

    let paired = ("sess-a".to_string(), other.to_str().unwrap().to_string());
    assert_eq!((moved.session_id, moved.cwd), paired);
    assert_eq!((dead.session_id, dead.cwd), paired);
}

#[test]
fn session_files_are_reread_on_the_slow_tick_only() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    let mut shared = procs.shared();
    let mut collector = KiroCollector::with_sessions_dir(d.sessions.clone());
    assert!((only(collector.collect(&shared)).credits - 2.0).abs() < 1e-9);

    let mut file = two_turns(&d);
    file["session_state"]["conversation_metadata"]["user_turn_metadatas"]
        .as_array_mut()
        .unwrap()
        .push(turn(1, 1, 1, 1, 30.0, &[3.0]));
    write(&d.sessions, "sess-a", &file);
    shared.slow_tick = false;
    let fast = only(collector.collect(&shared)).credits;
    shared.slow_tick = true;
    let slow = only(collector.collect(&shared)).credits;

    assert_eq!((fast, slow), (2.0, 5.0));
}

#[test]
fn a_first_collect_on_a_fast_tick_still_reads_the_files() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    let mut shared = procs.shared();
    shared.slow_tick = false;

    let s = only(KiroCollector::with_sessions_dir(d.sessions.clone()).collect(&shared));

    assert_eq!(s.session_id, "sess-a");
}

#[test]
fn a_first_read_that_fails_is_retried_on_the_next_fast_tick() {
    let d = dirs();
    std::fs::remove_dir(&d.sessions).unwrap();
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    let mut shared = procs.shared();
    shared.slow_tick = false;
    let mut collector = KiroCollector::with_sessions_dir(d.sessions.clone());
    let stub = format!("kiro-{pid}");
    let first = only(collector.collect(&shared)).session_id;

    std::fs::create_dir(&d.sessions).unwrap();
    write(&d.sessions, "sess-a", &two_turns(&d));
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    let second = only(collector.collect(&shared)).session_id;

    assert_eq!((first.as_str(), second.as_str()), (stub.as_str(), "sess-a"));
}

#[test]
fn a_failed_dir_read_on_the_slow_tick_keeps_the_last_session_files() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    let shared = procs.shared();
    let mut collector = KiroCollector::with_sessions_dir(d.sessions.clone());
    assert_eq!(only(collector.collect(&shared)).session_id, "sess-a");

    std::fs::rename(&d.sessions, d.sessions.with_file_name("sessions-gone")).unwrap();
    let s = only(collector.collect(&shared));

    assert_eq!((s.session_id.as_str(), s.credits), ("sess-a", 2.0));
}

#[test]
fn an_empty_sessions_dir_is_not_reread_before_the_slow_tick() {
    let d = dirs();
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    let mut shared = procs.shared();
    let mut collector = KiroCollector::with_sessions_dir(d.sessions.clone());
    let stub = format!("kiro-{pid}");
    assert_eq!(only(collector.collect(&shared)).session_id, stub);

    write(&d.sessions, "sess-a", &two_turns(&d));
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    shared.slow_tick = false;
    let fast = only(collector.collect(&shared)).session_id;
    shared.slow_tick = true;
    let slow = only(collector.collect(&shared)).session_id;

    assert_eq!((fast.as_str(), slow.as_str()), (stub.as_str(), "sess-a"));
}

#[test]
fn a_stub_keeps_its_first_seen_start_time() {
    let d = dirs();
    let mut procs = Procs::new();
    procs.spawn(&d.project, CHAT);
    let shared = procs.shared();
    let mut collector = KiroCollector::with_sessions_dir(d.sessions.clone());
    let first_wall = chrono::Utc::now().timestamp_millis() as u64;
    let first = only(collector.collect(&shared)).started_at;

    std::thread::sleep(Duration::from_millis(1100));
    let second = only(collector.collect(&shared)).started_at;

    assert_eq!(second, first);
    assert!(first.abs_diff(first_wall) < 1000, "{first} vs {first_wall}");
}

#[test]
fn a_chat_launched_from_a_path_with_a_space_is_listed() {
    let d = dirs();
    let mut procs = Procs::new();
    let pid = procs.spawn(
        &d.project,
        "/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli-chat chat --agent x",
    );

    assert_eq!(only(collect(&d, &mut procs)).pid, pid);
}

#[test]
fn detail_footer_shows_credits_only_when_credits_are_positive() {
    let d = dirs();
    write(&d.sessions, "sess-a", &two_turns(&d));
    let mut procs = Procs::new();
    let pid = procs.spawn(&d.project, CHAT);
    lock(&d.sessions, "sess-a", pid, LOCKED_AT);
    let (with_turns, _) = render(collect(&d, &mut procs), Theme::default());
    assert!(with_turns.contains("2.00 credits"), "{with_turns}");

    let empty = dirs();
    write(
        &empty.sessions,
        "sess-b",
        &session_file("sess-b", &empty.project, "2026-01-02T04:00:00Z", vec![]),
    );
    let pid = procs.spawn(&empty.project, CHAT);
    lock(&empty.sessions, "sess-b", pid, LOCKED_AT);
    let (no_turns, _) = render(collect(&empty, &mut procs), Theme::default());
    assert!(!no_turns.contains("credits"), "{no_turns}");

    let unmetered = dirs();
    write(
        &unmetered.sessions,
        "sess-c",
        &session_file(
            "sess-c",
            &unmetered.project,
            "2026-01-02T04:00:00Z",
            vec![turn(100, 20, 300, 40, 10.0, &[])],
        ),
    );
    let pid = procs.spawn(&unmetered.project, CHAT);
    lock(&unmetered.sessions, "sess-c", pid, LOCKED_AT);
    let (one_turn, _) = render(collect(&unmetered, &mut procs), Theme::default());
    assert!(
        one_turn.contains("1 turns") && !one_turn.contains("credits"),
        "{one_turn}"
    );
}

#[test]
fn sessions_pane_tags_kiro_in_a_fixed_colour() {
    let d = dirs();
    let mut procs = Procs::new();
    procs.spawn(&d.project, CHAT);
    let sessions = collect(&d, &mut procs);

    for theme in [Theme::default(), Theme::by_name("dracula").unwrap()] {
        let (screen, buf) = render(sessions.clone(), theme);
        assert!(
            screen.contains("^KR") && !screen.contains("KIR"),
            "{screen}"
        );
        let cell = buf.content().iter().find(|c| c.symbol() == "^").unwrap();
        assert_eq!(cell.fg, Color::Rgb(192, 132, 252));
    }
}
