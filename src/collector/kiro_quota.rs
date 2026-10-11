//! Kiro quota from `kiro-cli acp`'s free `_kiro/account/getUsage` call.

use crate::collector::write_kiro_cache;
use crate::model::RateLimitInfo;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const KIRO_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
pub const KIRO_FETCH_INTERVAL: Duration = Duration::from_secs(300);

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}"#;
const GET_USAGE: &str = r#"{"jsonrpc":"2.0","id":2,"method":"_kiro/account/getUsage","params":{}}"#;

/// Ask a fresh `kiro-cli acp` for the quota, killing it after the reply or at `timeout`.
/// The child waits in `slot`; whoever takes it out kills and reaps it.
pub fn fetch_kiro_quota(timeout: Duration, slot: &Mutex<Option<Child>>) -> Option<RateLimitInfo> {
    let mut cmd = Command::new("kiro-cli");
    cmd.args(["acp", "--agent-engine=v3", "--auth-method=cli"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // The launcher spawns `kiro-cli-chat acp`; its own group lets kill_tree reach both.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().ok()?;
    let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
    *slot.lock().expect("kiro child slot poisoned") = Some(child);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(stdin.zip(stdout).and_then(|(i, o)| talk(i, o)));
    });
    let info = rx.recv_timeout(timeout).ok().flatten();

    // `kiro-cli acp` is a server and never exits by itself.
    reap(slot);
    info
}

/// Refreshes the Kiro quota cache on a thread, one attempt at a time, at most every
/// `KIRO_FETCH_INTERVAL` from the last attempt.
#[derive(Default)]
pub struct KiroQuotaFetcher {
    last_attempt: Option<Instant>,
    worker: Option<JoinHandle<std::io::Result<()>>>,
    child: Arc<Mutex<Option<Child>>>,
    cache_failed: bool,
}

impl KiroQuotaFetcher {
    /// Starts an attempt when one is due; true on the call that sees one finish.
    pub fn poll(&mut self) -> bool {
        self.poll_at(Instant::now())
    }

    /// `poll` with the clock passed in, so a test can step past the interval.
    pub fn poll_at(&mut self, now: Instant) -> bool {
        let finished = self
            .worker
            .take_if(|w| w.is_finished())
            .map(|w| self.cache_failed = w.join().is_ok_and(|r| r.is_err()))
            .is_some();
        if self.worker.is_none()
            && self
                .last_attempt
                .is_none_or(|t| now.duration_since(t) >= KIRO_FETCH_INTERVAL)
        {
            self.last_attempt = Some(now);
            let child = Arc::clone(&self.child);
            self.worker = Some(std::thread::spawn(move || {
                fetch_kiro_quota(KIRO_FETCH_TIMEOUT, &child)
                    .map_or(Ok(()), |i| write_kiro_cache(&i))
            }));
        }
        finished
    }

    /// True when the last attempt fetched the quota but could not write the cache.
    pub fn cache_failed(&self) -> bool {
        self.cache_failed
    }

    /// True while an attempt's thread has not been seen to finish.
    pub fn in_flight(&self) -> bool {
        self.worker.is_some()
    }
}

impl Drop for KiroQuotaFetcher {
    fn drop(&mut self) {
        // Not joined: the kill ends the worker's wait.
        reap(&self.child);
    }
}

fn talk(mut stdin: ChildStdin, stdout: ChildStdout) -> Option<RateLimitInfo> {
    // A newline-less flood would otherwise buffer in `lines()` until the timeout.
    let mut lines = BufReader::new(stdout.take(4 << 20)).lines();
    let mut reply = |id: u64| {
        lines.by_ref().map_while(Result::ok).find_map(|line| {
            serde_json::from_str::<Value>(&line)
                .ok()
                .filter(|v| v["id"] == id)
        })
    };
    writeln!(stdin, "{INITIALIZE}").ok()?;
    // getUsage sent before initialize has answered fails authentication.
    reply(1)?;
    writeln!(stdin, "{GET_USAGE}").ok()?;
    parse_usage(&reply(2)?)
}

fn parse_usage(reply: &Value) -> Option<RateLimitInfo> {
    let result = &reply["result"];
    if result["success"] != true {
        return None;
    }
    let data = &result["data"];
    let pct = data["usageBreakdowns"][0]["percentage"].as_f64()?;
    let reset = chrono::NaiveDate::parse_from_str(data["billingCycleReset"].as_str()?, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .timestamp();
    Some(RateLimitInfo {
        source: "kiro".into(),
        seven_day_pct: Some(pct),
        seven_day_resets_at: Some(u64::try_from(reset).ok()?),
        seven_day_window_minutes: Some(43_200),
        updated_at: Some(chrono::Utc::now().timestamp() as u64),
        ..Default::default()
    })
}

fn reap(slot: &Mutex<Option<Child>>) {
    // Not an `if let` scrutinee: its guard would hold the lock through kill and wait.
    let child = slot.lock().expect("kiro child slot poisoned").take();
    if let Some(mut child) = child {
        kill_tree(child.id());
        let _ = child.wait();
    }
}

fn kill_tree(pid: u32) {
    // A negative pid names the whole process group.
    #[cfg(unix)]
    let _ = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
