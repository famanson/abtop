#![cfg(not(windows))]

use abtop::app::App;
use abtop::collector::kiro_quota::{fetch_kiro_quota, KiroQuotaFetcher, KIRO_FETCH_INTERVAL};
use abtop::collector::rate_limit::{read_kiro_cache, write_kiro_cache};
use abtop::config::PanelVisibility;
use abtop::locale::t;
use abtop::model::RateLimitInfo;
use abtop::theme::Theme;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

static ENV_LOCK: Mutex<()> = Mutex::new(());
const ENV_VARS: [&str; 4] = ["HOME", "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "PATH"];

const ARGS: &str = "acp --agent-engine=v3 --auth-method=cli";
const USAGE: &str = r#"{"success":true,"data":{"billingCycleReset":"2026-11-01","usageBreakdowns":[{"used":1528.99,"limit":10000,"percentage":15}]}}"#;
/// 2026-11-01T00:00:00Z
const RESET: u64 = 1_793_491_200;

/// HOME pointed at a fresh temp dir with XDG overrides cleared; restored on drop.
struct TempHome {
    saved: Vec<(&'static str, Option<OsString>)>,
    dir: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}

impl TempHome {
    fn new() -> Self {
        // A red test panics while holding the lock; later tests must still run.
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let saved = ENV_VARS.iter().map(|&k| (k, std::env::var_os(k))).collect();
        std::env::set_var("HOME", dir.path());
        std::env::remove_var("XDG_CACHE_HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
        Self {
            saved,
            dir,
            _lock: lock,
        }
    }

    /// A fake `kiro-cli` answering getUsage with `result`, first on PATH.
    fn fake_kiro(&self, result: &str) -> FakeKiro {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let script = format!(
            r#"#!/bin/sh
d='{d}'
echo "$*" >> "$d/calls"
echo $$ > "$d/pid"
while read -r line; do
  if [ -e "$d/flood" ]; then sleep 60 & echo $! > "$d/sleep.pid"; head -c 5242880 /dev/zero | tr '\000' a; wait; fi
  case $line in
  *'"initialize"'*) ( sleep 1.5; touch "$d/ready"; echo '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1}}}}' ) & ;;
  *_kiro/account/getUsage*)
    if [ -e "$d/ready" ]; then r=yes; else r=no; fi
    echo "getUsage ready=$r" >> "$d/calls"
    if [ -e "$d/hang" ]; then sleep 60 & echo $! > "$d/sleep.pid"; wait
    elif [ $r = yes ]; then cat "$d/usage"
    else echo '{{"jsonrpc":"2.0","id":2,"result":{{"success":false,"message":"Authentication failed"}}}}'
    fi ;;
  esac
done
"#
        );
        let bin = dir.path().join("kiro-cli");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            dir.path().join("usage"),
            format!("{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{result}}}\n"),
        )
        .unwrap();
        set_path(dir.path());
        FakeKiro { dir }
    }

    fn cache_dir(&self) -> std::path::PathBuf {
        let cache = if cfg!(target_os = "macos") {
            "Library/Caches"
        } else {
            ".cache"
        };
        self.dir.path().join(cache).join("abtop")
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// PATH with `dir` first and only the system dirs after it, so the real kiro-cli is out of reach.
fn set_path(dir: &Path) {
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", dir.display()));
}

struct FakeKiro {
    dir: tempfile::TempDir,
}

impl FakeKiro {
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn pid(&self, file: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join(file))
            .ok()
            .map(|s| s.trim().to_string())
    }

    #[track_caller]
    fn assert_launched(&self) {
        assert_eq!(self.calls().first().map(String::as_str), Some(ARGS));
    }
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn maps_the_getusage_reply_to_a_monthly_window() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);

    let info = fetch_kiro_quota(Duration::from_secs(5), &Mutex::new(None));

    fake.assert_launched();
    let info = info.expect("a good getUsage reply should give a quota");
    assert_eq!(info.source, "kiro");
    assert_eq!(info.seven_day_pct, Some(15.0));
    assert_eq!(info.seven_day_resets_at, Some(RESET));
    assert_eq!(info.seven_day_window_minutes, Some(43_200));
    assert!(now().abs_diff(info.updated_at.unwrap()) < 10);
    assert_eq!(info.five_hour_pct, None);
}

#[test]
fn sends_getusage_only_after_initialize_has_replied() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);

    let info = fetch_kiro_quota(Duration::from_secs(5), &Mutex::new(None));

    fake.assert_launched();
    assert_eq!(
        fake.calls().get(1).map(String::as_str),
        Some("getUsage ready=yes")
    );
    assert!(info.is_some());
}

#[test]
fn write_kiro_cache_round_trips_through_read_kiro_cache() {
    let home = TempHome::new();
    let written = RateLimitInfo {
        source: "kiro".into(),
        seven_day_pct: Some(15.0),
        seven_day_resets_at: Some(RESET),
        seven_day_window_minutes: Some(43_200),
        updated_at: Some(1_793_480_000),
        ..Default::default()
    };

    write_kiro_cache(&written).unwrap();

    let info = read_kiro_cache().expect("the written cache should read back");
    assert_eq!(info.source, "kiro");
    assert_eq!(info.seven_day_pct, Some(15.0));
    assert_eq!(info.seven_day_resets_at, Some(RESET));
    assert_eq!(info.seven_day_window_minutes, Some(43_200));
    assert_eq!(info.updated_at, Some(1_793_480_000));
    assert_eq!(info.five_hour_pct, None);
    let files: Vec<_> = std::fs::read_dir(home.cache_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(files, ["kiro-rate-limits.json"]);
}

#[test]
fn a_failed_rename_is_reported_and_leaves_no_tmp_file() {
    let home = TempHome::new();
    std::fs::create_dir_all(home.cache_dir().join("kiro-rate-limits.json")).unwrap();

    assert!(write_kiro_cache(&RateLimitInfo {
        source: "kiro".into(),
        seven_day_pct: Some(15.0),
        ..Default::default()
    })
    .is_err());

    let files: Vec<_> = std::fs::read_dir(home.cache_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        !files.iter().any(|f| f.to_string_lossy().contains(".tmp")),
        "{files:?}"
    );
}

#[test]
fn a_bad_reply_yields_none_and_asks_anyway() {
    let home = TempHome::new();
    let bad = [
        r#"{"success":false,"message":"token expired"}"#.to_string(),
        USAGE.replace(r#"[{"used":1528.99,"limit":10000,"percentage":15}]"#, "[]"),
        USAGE.replace(r#""percentage":15"#, r#""percentage":"15""#),
        USAGE.replace("2026-11-01", "soon"),
    ];
    for result in &bad {
        let fake = home.fake_kiro(result);

        let info = fetch_kiro_quota(Duration::from_secs(5), &Mutex::new(None));

        fake.assert_launched();
        assert_eq!(
            fake.calls().get(1).map(String::as_str),
            Some("getUsage ready=yes"),
            "{result}"
        );
        assert!(info.is_none(), "{result} gave {info:?}");
    }

    let empty = tempfile::tempdir().unwrap();
    set_path(empty.path());
    let start = Instant::now();
    let info = fetch_kiro_quota(Duration::from_secs(5), &Mutex::new(None));
    assert!(info.is_none());
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn a_hung_kiro_cli_is_killed_with_its_children_at_the_timeout() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);
    std::fs::write(fake.dir.path().join("hang"), "").unwrap();
    let slot = Mutex::new(None);

    let start = Instant::now();
    let info = fetch_kiro_quota(Duration::from_secs(3), &slot);
    let took = start.elapsed();

    fake.assert_launched();
    assert!(info.is_none());
    assert!(took < Duration::from_secs(5), "{took:?}");
    let pids = [fake.pid("pid").unwrap(), fake.pid("sleep.pid").unwrap()];
    let deadline = Instant::now() + Duration::from_secs(2);
    while pids.iter().any(|p| alive(p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pids.iter().any(|p| alive(p)), "still alive: {pids:?}");
    assert!(slot.lock().unwrap().is_none());
}

#[test]
fn a_newline_less_flood_ends_the_attempt_at_the_cap() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);
    std::fs::write(fake.dir.path().join("flood"), "").unwrap();
    let slot = Mutex::new(None);

    let start = Instant::now();
    let info = fetch_kiro_quota(Duration::from_secs(5), &slot);
    let took = start.elapsed();

    assert!(info.is_none());
    assert!(took < Duration::from_secs(3), "{took:?}");
    let pids = [fake.pid("pid").unwrap(), fake.pid("sleep.pid").unwrap()];
    let deadline = Instant::now() + Duration::from_secs(2);
    while pids.iter().any(|p| alive(p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pids.iter().any(|p| alive(p)), "still alive: {pids:?}");
    assert!(slot.lock().unwrap().is_none());
}

#[test]
fn a_hung_kiro_cli_is_killed_without_kill_on_path() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);
    std::fs::write(fake.dir.path().join("hang"), "").unwrap();
    let tools = tempfile::tempdir().unwrap();
    for tool in ["sleep", "touch"] {
        let bin = ["/bin", "/usr/bin"]
            .map(|d| Path::new(d).join(tool))
            .into_iter()
            .find(|p| p.exists())
            .unwrap();
        std::os::unix::fs::symlink(bin, tools.path().join(tool)).unwrap();
    }
    std::env::set_var(
        "PATH",
        format!("{}:{}", fake.dir.path().display(), tools.path().display()),
    );
    let slot = Arc::new(Mutex::new(None));
    let (tx, rx) = std::sync::mpsc::channel();

    let start = Instant::now();
    let fetch_slot = Arc::clone(&slot);
    std::thread::spawn(move || {
        let _ = tx.send(fetch_kiro_quota(Duration::from_secs(3), &fetch_slot));
    });
    let info = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("fetch did not return");
    let took = start.elapsed();
    set_path(fake.dir.path());

    assert!(info.is_none());
    assert!(took < Duration::from_secs(4), "{took:?}");
    let pids = [fake.pid("pid").unwrap(), fake.pid("sleep.pid").unwrap()];
    let deadline = Instant::now() + Duration::from_secs(2);
    while pids.iter().any(|p| alive(p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pids.iter().any(|p| alive(p)), "still alive: {pids:?}");
    assert!(slot.lock().unwrap().is_none());
}

#[test]
fn holding_the_child_slot_holds_back_the_fetchs_kill_until_released() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);
    std::fs::write(fake.dir.path().join("hang"), "").unwrap();
    let slot = Arc::new(Mutex::new(None));
    let (tx, rx) = std::sync::mpsc::channel();

    let fetch_slot = Arc::clone(&slot);
    std::thread::spawn(move || {
        let _ = tx.send(fetch_kiro_quota(Duration::from_secs(3), &fetch_slot));
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while fake.pid("sleep.pid").is_none() {
        assert!(Instant::now() < deadline, "getUsage never hung");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut guard = slot.lock().unwrap();
    let pid = fake.pid("pid").unwrap();
    assert_eq!(
        guard.as_ref().map(|c| c.id().to_string()),
        Some(pid.clone())
    );
    assert!(rx.recv_timeout(Duration::from_secs(3)).is_err());
    assert!(
        matches!(guard.as_mut().unwrap().try_wait(), Ok(None)),
        "killed while the slot was held"
    );
    drop(guard);

    let info = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("fetch did not return once the slot was released");
    assert!(info.is_none());
    let pids = [pid, fake.pid("sleep.pid").unwrap()];
    let deadline = Instant::now() + Duration::from_secs(2);
    while pids.iter().any(|p| alive(p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pids.iter().any(|p| alive(p)), "still alive: {pids:?}");
    assert!(slot.lock().unwrap().is_none());
}

#[track_caller]
fn poll_until_done(fetcher: &mut KiroQuotaFetcher, within: Duration) {
    let deadline = Instant::now() + within;
    while !fetcher.poll() {
        assert!(
            Instant::now() < deadline,
            "no attempt finished within {within:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn launches(fake: &FakeKiro) -> usize {
    fake.calls().iter().filter(|c| *c == ARGS).count()
}

#[test]
fn polls_once_and_not_again_inside_five_minutes() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);
    let mut fetcher = KiroQuotaFetcher::default();

    assert!(!fetcher.poll());
    poll_until_done(&mut fetcher, Duration::from_secs(5));

    fake.assert_launched();
    let info = read_kiro_cache().expect("a finished fetch should write the cache");
    assert_eq!(info.seven_day_pct, Some(15.0));
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(200));
        assert!(!fetcher.poll());
    }
    assert_eq!(launches(&fake), 1, "{:?}", fake.calls());
}

#[test]
fn polls_again_once_the_interval_has_passed() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);
    let mut fetcher = KiroQuotaFetcher::default();
    let t0 = Instant::now();

    fetcher.poll_at(t0);
    poll_until_done(&mut fetcher, Duration::from_secs(5));
    assert!(!fetcher.poll_at(t0 + KIRO_FETCH_INTERVAL - Duration::from_secs(1)));
    assert!(
        !fetcher.in_flight(),
        "an attempt started inside the interval"
    );
    assert_eq!(launches(&fake), 1, "{:?}", fake.calls());
    fetcher.poll_at(t0 + KIRO_FETCH_INTERVAL);
    poll_until_done(&mut fetcher, Duration::from_secs(5));

    assert_eq!(launches(&fake), 2, "{:?}", fake.calls());
}

#[test]
fn a_rejected_reply_leaves_the_last_cache_in_place() {
    let home = TempHome::new();
    write_kiro_cache(&RateLimitInfo {
        source: "kiro".into(),
        seven_day_pct: Some(7.0),
        seven_day_resets_at: Some(RESET),
        seven_day_window_minutes: Some(43_200),
        updated_at: Some(now()),
        ..Default::default()
    })
    .unwrap();
    let fake = home.fake_kiro(r#"{"success":false,"message":"token expired"}"#);
    let mut fetcher = KiroQuotaFetcher::default();

    fetcher.poll();
    poll_until_done(&mut fetcher, Duration::from_secs(5));

    assert!(
        fake.calls().iter().any(|c| c == "getUsage ready=yes"),
        "{:?}",
        fake.calls()
    );
    assert_eq!(read_kiro_cache().unwrap().seven_day_pct, Some(7.0));
}

#[test]
fn a_rejected_reply_after_a_failed_cache_write_clears_cache_failed() {
    let home = TempHome::new();
    let cache = home.cache_dir().join("kiro-rate-limits.json");
    std::fs::create_dir_all(&cache).unwrap();
    let _first = home.fake_kiro(USAGE);
    let mut fetcher = KiroQuotaFetcher::default();
    let t0 = Instant::now();

    fetcher.poll_at(t0);
    poll_until_done(&mut fetcher, Duration::from_secs(5));
    assert!(fetcher.cache_failed());

    std::fs::remove_dir(&cache).unwrap();
    let _second = home.fake_kiro(r#"{"success":false,"message":"token expired"}"#);
    fetcher.poll_at(t0 + KIRO_FETCH_INTERVAL);
    poll_until_done(&mut fetcher, Duration::from_secs(5));

    assert!(!fetcher.cache_failed());
}

#[test]
fn a_missing_kiro_cli_finishes_the_attempt_without_a_cache() {
    let home = TempHome::new();
    let empty = tempfile::tempdir().unwrap();
    set_path(empty.path());
    let mut fetcher = KiroQuotaFetcher::default();

    fetcher.poll();
    poll_until_done(&mut fetcher, Duration::from_secs(1));

    assert!(!home.cache_dir().join("kiro-rate-limits.json").exists());
    // Long enough for an unthrottled retry to fail and finish.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !fetcher.poll(),
        "a failed attempt should still throttle the next"
    );
}

#[test]
fn dropping_the_fetcher_kills_a_fetch_in_flight() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);
    std::fs::write(fake.dir.path().join("hang"), "").unwrap();
    let mut fetcher = KiroQuotaFetcher::default();

    fetcher.poll();
    let deadline = Instant::now() + Duration::from_secs(4);
    while fake.pid("sleep.pid").is_none() {
        assert!(
            Instant::now() < deadline,
            "the fetch never reached getUsage"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(fetcher);

    let pids = [fake.pid("pid").unwrap(), fake.pid("sleep.pid").unwrap()];
    let deadline = Instant::now() + Duration::from_secs(2);
    while pids.iter().any(|p| alive(p)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pids.iter().any(|p| alive(p)), "still alive: {pids:?}");
}

#[test]
fn the_tui_tick_fetches_but_headless_and_hidden_kiro_do_not() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);
    // No Claude sessions, so `tick` has no summary to spawn `claude --print` for.
    let others = ["claude", "codex", "opencode"].map(String::from);

    let mut app = quota_only_app(&others);
    app.tick_no_summaries();
    std::thread::sleep(Duration::from_millis(400));
    app.tick_no_summaries();
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());

    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        app.tick();
        let panel = quota_panel(&render(&app));
        if column(&panel, 2).iter().any(|r| r.contains("85%")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no 85% in the KIRO column:\n{panel}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(app);

    let mut hidden = quota_only_app(&[others.as_slice(), &["kiro".into()]].concat());
    hidden.tick();
    std::thread::sleep(Duration::from_millis(400));
    hidden.tick();
    assert_eq!(launches(&fake), 1, "{:?}", fake.calls());
}

#[test]
fn a_landed_fetch_shows_on_the_next_tick() {
    let home = TempHome::new();
    write_kiro_cache(&RateLimitInfo {
        source: "kiro".into(),
        seven_day_pct: Some(7.0),
        seven_day_resets_at: Some(RESET),
        seven_day_window_minutes: Some(43_200),
        updated_at: Some(now()),
        ..Default::default()
    })
    .unwrap();
    let fake = home.fake_kiro(USAGE);
    let mut app = quota_only_app(&["claude", "codex", "opencode"].map(String::from));

    app.tick();
    let mut panel = quota_panel(&render(&app));
    assert!(
        column(&panel, 2).iter().any(|r| r.contains("93%")),
        "{panel}"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while read_kiro_cache().and_then(|i| i.seven_day_pct) != Some(15.0) {
        assert!(
            Instant::now() < deadline,
            "no fetch landed: {:?}",
            fake.calls()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let shown = (0..4).any(|_| {
        app.tick();
        panel = quota_panel(&render(&app));
        let found = column(&panel, 2).iter().any(|r| r.contains("85%"));
        if !found {
            std::thread::sleep(Duration::from_millis(100));
        }
        found
    });
    assert!(shown, "no 85% in the KIRO column:\n{panel}");
}

#[test]
fn a_fetch_that_cannot_write_its_cache_says_so_in_the_kiro_column() {
    let home = TempHome::new();
    std::fs::create_dir_all(home.cache_dir().join("kiro-rate-limits.json")).unwrap();
    let fake = home.fake_kiro(USAGE);
    let mut app = quota_only_app(&["claude", "codex", "opencode"].map(String::from));
    let (failed, no_data) = (t("quota.kiro_cache_failed"), t("quota.no_data"));

    let deadline = Instant::now() + Duration::from_secs(5);
    let kiro = loop {
        app.tick();
        let kiro = column(&quota_panel(&render(&app)), 2).join("\n");
        if kiro.contains(&failed) {
            break kiro;
        }
        assert!(
            Instant::now() < deadline,
            "no {failed:?} in the KIRO column: {:?}\n{kiro}",
            fake.calls()
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(kiro.contains(&no_data), "{kiro}");
    assert!(!kiro.contains(&t("quota.kiro_setup")), "{kiro}");
}

#[test]
fn once_spawns_no_kiro_cli() {
    let home = TempHome::new();
    let fake = home.fake_kiro(USAGE);

    let out = Command::new(env!("CARGO_BIN_EXE_abtop"))
        .arg("--once")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

fn quota_only_app(hidden: &[String]) -> App {
    let panels = PanelVisibility {
        context: false,
        quota: true,
        tokens: false,
        projects: false,
        ports: false,
        sessions: true,
        mcp: false,
    };
    App::new_with_config_and_claude_dirs(Theme::default(), hidden, panels, &[])
}

fn render(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|f| abtop::ui::draw(f, app)).unwrap();
    format!("{}", terminal.backend())
}

/// The quota panel's rows below its title, borders included.
fn quota_panel(screen: &str) -> String {
    let lines: Vec<Vec<char>> = screen.lines().map(|l| l.chars().collect()).collect();
    let top = lines
        .iter()
        .position(|l| l.iter().collect::<String>().contains("quota"))
        .unwrap_or_else(|| panic!("no quota panel in:\n{screen}"));
    let title = lines[top].iter().collect::<String>();
    let title_col = title[..title.find("quota").unwrap()].chars().count();
    let left = lines[top][..title_col]
        .iter()
        .rposition(|&c| c == '╭')
        .unwrap();
    let right = left + lines[top][left..].iter().position(|&c| c == '╮').unwrap();
    // Rows hold wide chars as one char over two cells, so cut by cell, not char index.
    let panel: Vec<String> = lines[top + 1..]
        .iter()
        .take_while(|l| l[left] != '╰')
        .map(|l| {
            let mut cell = 0;
            l.iter()
                .filter(|c| {
                    let at = cell;
                    cell += c.width().unwrap_or(0);
                    (left..=right).contains(&at)
                })
                .collect()
        })
        .collect();
    panel.join("\n")
}

/// Column `idx` of `quota_panel`'s rows, cut by cell as the panel splits them.
fn column(panel: &str, idx: usize) -> Vec<String> {
    let inner = panel.lines().next().unwrap().width() - 2;
    let start = 1 + idx * (inner / 3);
    let end = if idx == 2 {
        1 + inner
    } else {
        start + inner / 3
    };
    panel
        .lines()
        .map(|l| {
            let mut cell = 0;
            l.chars()
                .filter(|c| {
                    let at = cell;
                    cell += c.width().unwrap_or(0);
                    (start..end).contains(&at)
                })
                .collect()
        })
        .collect()
}
