use abtop::app::App;
#[cfg(not(windows))]
use abtop::collector::read_kiro_cache;
use abtop::config::PanelVisibility;
use abtop::locale::t;
use abtop::model::RateLimitInfo;
use abtop::theme::Theme;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::ffi::OsString;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

static ENV_LOCK: Mutex<()> = Mutex::new(());
const ENV_VARS: [&str; 3] = ["HOME", "XDG_CACHE_HOME", "XDG_CONFIG_HOME"];

/// HOME pointed at a fresh temp dir with XDG overrides cleared; restored on drop.
struct TempHome {
    saved: Vec<(&'static str, Option<OsString>)>,
    #[cfg_attr(windows, allow(dead_code))]
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

    #[cfg(not(windows))]
    fn write_kiro_cache(&self, json: &str) {
        let cache = if cfg!(target_os = "macos") {
            "Library/Caches"
        } else {
            ".cache"
        };
        let path = self.dir.path().join(cache).join("abtop");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("kiro-rate-limits.json"), json).unwrap();
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

fn quota_only_app(hidden_agents: &[String]) -> App {
    let panels = PanelVisibility {
        context: false,
        quota: true,
        tokens: false,
        projects: false,
        ports: false,
        sessions: true,
        mcp: false,
    };
    App::new_with_config_and_claude_dirs(Theme::default(), hidden_agents, panels, &[])
}

fn render(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|f| abtop::ui::draw(f, app)).unwrap();
    format!("{}", terminal.backend())
}

fn render_with_only_quota_mid_panel() -> String {
    render(&quota_only_app(&[]))
}

// `dirs::cache_dir()` ignores HOME on Windows, so TempHome cannot redirect the cache there.
#[cfg(not(windows))]
#[test]
fn reads_kiro_cache_monthly_window_into_seven_day_slot() {
    let home = TempHome::new();
    home.write_kiro_cache(
        r#"{"source":"kiro","seven_day":{"used_percentage":15,"resets_at":1793491200,"window_minutes":43200},"updated_at":1793480000}"#,
    );

    let info = read_kiro_cache().expect("kiro cache should be read");

    assert_eq!(info.source, "kiro");
    assert_eq!(info.seven_day_pct, Some(15.0));
    assert_eq!(info.seven_day_resets_at, Some(1_793_491_200));
    assert_eq!(info.seven_day_window_minutes, Some(43_200));
    assert_eq!(info.five_hour_pct, None);
}

#[cfg(not(windows))]
#[test]
fn kiro_cache_reports_source_kiro_whatever_the_file_says() {
    let home = TempHome::new();
    home.write_kiro_cache(
        r#"{"source":"claude","seven_day":{"used_percentage":15,"resets_at":1793491200,"window_minutes":43200},"updated_at":1793480000}"#,
    );

    let info = read_kiro_cache().expect("kiro cache should be read");

    assert_eq!(info.source, "kiro");
    assert_eq!(info.seven_day_pct, Some(15.0));
    assert_eq!(info.seven_day_resets_at, Some(1_793_491_200));
}

#[cfg(not(windows))]
#[test]
fn kiro_cache_reaches_the_quota_panel_after_a_tick() {
    let home = TempHome::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let resets_at = now + 20 * 86_400 + 5 * 3_600 + 30;
    home.write_kiro_cache(&format!(
        r#"{{"source":"kiro","seven_day":{{"used_percentage":15,"resets_at":{resets_at},"window_minutes":43200}},"updated_at":{now}}}"#
    ));
    let mut app = quota_only_app(&[]);
    app.tick_no_summaries();

    let panel = quota_panel(&render(&app));
    let kiro = column(&panel, 2);

    assert!(kiro[0].contains("KIRO"), "{kiro:?}:\n{panel}");
    let bar = &kiro[1];
    assert!(bar.contains(&format!("30{}", t("time.d"))), "{bar:?}");
    assert!(bar.contains('■') && bar.contains("85%"), "{bar:?}");
    let in_20d_5h = format!("{} 20{} 5{}", t("quota.in"), t("time.d"), t("time.h"));
    assert!(kiro[2].contains(&in_20d_5h), "{:?}", kiro[2]);
}

/// All panels on, with a Claude 5h/7d and a Codex 7d window reset at `now`-relative times.
fn default_layout_app(now: u64) -> App {
    let mut app = App::new_with_config_and_claude_dirs(
        Theme::default(),
        &[],
        PanelVisibility::default(),
        &[],
    );
    app.rate_limits = vec![
        RateLimitInfo {
            source: "claude".into(),
            five_hour_pct: Some(20.0),
            five_hour_resets_at: Some(now + 72 * 60 + 30),
            five_hour_window_minutes: Some(300),
            seven_day_pct: Some(17.0),
            seven_day_resets_at: Some(now + 6 * 86_400 + 13 * 3_600 + 30),
            seven_day_window_minutes: Some(10_080),
            updated_at: Some(now),
        },
        RateLimitInfo {
            source: "codex".into(),
            seven_day_pct: Some(40.0),
            seven_day_resets_at: Some(now + 12 * 3_600 + 30 * 60 + 30),
            seven_day_window_minutes: Some(10_080),
            updated_at: Some(now),
            ..Default::default()
        },
    ];
    app
}

fn draw_120x40(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|f| abtop::ui::draw(f, app)).unwrap();
    format!("{}", terminal.backend())
}

#[test]
fn quota_rows_keep_percent_and_countdown_at_the_default_120x40_layout() {
    let _home = TempHome::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let panel = quota_panel(&draw_120x40(&default_layout_app(now)));

    let (h, d) = (t("time.h"), t("time.d"));
    // A row that fills the 7-cell column drops the space: en `5h 80%`, zh `5时80%`.
    let sp = if h.width() == 1 { " " } else { "" };
    for row in [
        format!("12{h}"),
        format!("5{h}{sp}80%"),
        format!("7{d}{sp}83%"),
        format!("1{h}"),
        format!("6{d}"),
    ] {
        assert!(panel.contains(&row), "no {row:?} in:\n{panel}");
    }
    for clipped in [format!("1{h} 1"), format!("6{d} 1")] {
        assert!(!panel.contains(&clipped), "{clipped:?} in:\n{panel}");
    }
    assert!(
        !panel.contains('■'),
        "bar between label and percent:\n{panel}"
    );
}

#[test]
fn compact_rows_share_the_column_edge() {
    let _home = TempHome::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let panel = quota_panel(&draw_120x40(&default_layout_app(now)));

    let h = t("time.h");
    let sp = if h.width() == 1 { " " } else { "" };
    let lines: Vec<&str> = panel.lines().collect();
    let header = lines
        .iter()
        .position(|l| l.contains("CLAUDE"))
        .unwrap_or_else(|| panic!("no header in:\n{panel}"));
    let (window, reset) = (lines[header + 1], lines[header + 2]);
    assert!(window.contains(&format!("5{h}{sp}80%")), "\n{panel}");
    assert!(reset.contains(&format!("1{h}")), "\n{panel}");
    let pad = |l: &str| {
        l.trim_start_matches('│')
            .chars()
            .take_while(|&c| c == ' ')
            .count()
    };
    let pads = [pad(lines[header]), pad(window), pad(reset)];
    assert!(
        pads[0] == pads[1] && pads[1] == pads[2],
        "{pads:?}:\n{panel}"
    );
}

#[test]
fn compact_columns_without_data_show_a_dash_and_no_hint() {
    let _home = TempHome::new();
    let app = App::new_with_config_and_claude_dirs(
        Theme::default(),
        &[],
        PanelVisibility::default(),
        &[],
    );
    let panel = quota_panel(&draw_120x40(&app));
    let lines: Vec<&str> = panel.lines().collect();

    let header = lines
        .iter()
        .position(|l| l.contains("CLAUDE"))
        .unwrap_or_else(|| panic!("no header in:\n{panel}"));
    for idx in 0..3 {
        let row = &column(&panel, idx)[header + 1];
        assert_eq!(row.trim_end(), "—", "column {idx}:\n{panel}");
    }
    for key in ["quota.abtop_setup", "quota.run_codex", "quota.kiro_setup"] {
        let hint: String = t(key).chars().take(4).collect();
        assert!(!panel.contains(&hint), "{hint:?} in:\n{panel}");
    }
    assert!(!panel.contains(&t("quota.no_data")), "\n{panel}");
}

/// The locale is fixed once per process from LANG, so each language runs in a child.
#[test]
fn quota_rows_fit_a_7_cell_column_in_en_and_zh() {
    if std::env::var_os("ABTOP_LOCALE_CHILD").is_some() {
        let _home = TempHome::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut app = App::new_with_config_and_claude_dirs(
            Theme::default(),
            &[],
            PanelVisibility::default(),
            &[],
        );
        app.rate_limits = vec![
            RateLimitInfo {
                source: "claude".into(),
                five_hour_pct: Some(0.0),
                five_hour_resets_at: Some(now + 72 * 60 + 30),
                five_hour_window_minutes: Some(300),
                seven_day_pct: Some(17.0),
                seven_day_resets_at: Some(now + 6 * 86_400 + 13 * 3_600 + 30),
                seven_day_window_minutes: Some(10_080),
                updated_at: Some(now),
            },
            RateLimitInfo {
                source: "codex".into(),
                five_hour_pct: Some(35.0),
                five_hour_resets_at: Some(now + 3 * 3_600 + 30),
                five_hour_window_minutes: Some(300),
                seven_day_pct: Some(40.0),
                seven_day_resets_at: Some(now + 12 * 3_600 + 30 * 60 + 30),
                seven_day_window_minutes: Some(10_080),
                updated_at: Some(now),
            },
            RateLimitInfo {
                source: "kiro".into(),
                seven_day_pct: Some(0.0),
                seven_day_resets_at: Some(now + 19 * 86_400 + 23 * 3_600 + 30),
                seven_day_window_minutes: Some(43_200),
                updated_at: Some(now),
                ..Default::default()
            },
        ];
        print_quota_panel(&app, 120);
        return;
    }
    let en: (&[&str], &[&str]) = (
        &[
            "5h100%", "7d 83%", "in 1h", "in 6d", "7d 60%", "in 12h", "in 19d",
        ],
        &["1h 1", "6d 1", "19d 2", "■"],
    );
    // The trailing space is the cell that keeps CODEX's row off KIRO's.
    let zh: (&[&str], &[&str]) = (
        &[
            "5时100%", "5时65% ", "7天83%", "1时", "6天", "7天60%", "12时", "19天",
        ],
        &["还有", "5时 100", "■"],
    );
    let panels = panels_in_en_and_zh("quota_rows_fit_a_7_cell_column_in_en_and_zh");
    // zh `30天100%` is 8 cells, exactly KIRO's width.
    let langs = [("en", en, "30d100%"), ("zh", zh, "30天100%")];
    for ((lang, (present, absent), kiro_row), panel) in langs.into_iter().zip(panels) {
        for row in present {
            assert!(panel.contains(row), "{lang}: no {row:?} in:\n{panel}");
        }
        for clipped in absent {
            assert!(!panel.contains(clipped), "{lang}: {clipped:?} in:\n{panel}");
        }
        let kiro = column(&panel, 2);
        assert!(
            kiro.iter().any(|r| r.contains(kiro_row)),
            "{lang}: no {kiro_row:?} in {kiro:?}"
        );
    }
}

#[test]
fn zh_labels_an_unknown_5h_window_with_the_short_hour_unit() {
    if std::env::var_os("ABTOP_LOCALE_CHILD").is_some() {
        let _home = TempHome::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut app = default_layout_app(now);
        app.rate_limits = vec![RateLimitInfo {
            source: "claude".into(),
            five_hour_pct: Some(17.0),
            five_hour_resets_at: Some(now + 72 * 60 + 30),
            five_hour_window_minutes: None,
            updated_at: Some(now),
            ..Default::default()
        }];
        print_quota_panel(&app, 120);
        return;
    }
    let [_, zh] = panels_in_en_and_zh("zh_labels_an_unknown_5h_window_with_the_short_hour_unit");
    assert!(zh.contains("5时83%"), "no \"5时83%\" in:\n{zh}");
    assert!(!zh.contains("小时"), "\"小时\" in:\n{zh}");
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

/// Child side of a locale test: prints the locale's no-data text and the quota panel.
fn print_quota_panel(app: &App, width: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
    terminal.draw(|f| abtop::ui::draw(f, app)).unwrap();
    println!("no_data={}", t("quota.no_data"));
    for line in quota_panel(&format!("{}", terminal.backend())).lines() {
        println!("panel={line}");
    }
}

/// Reruns `test` in an en and a zh child and returns their quota panels.
fn panels_in_en_and_zh(test: &str) -> [String; 2] {
    [("en_US.UTF-8", "no data"), ("zh_CN.UTF-8", "无数据")].map(|(lang, no_data)| {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("ABTOP_LOCALE_CHILD", "1")
            .env("LANG", lang)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let child_no_data = stdout
            .lines()
            .find_map(|l| l.strip_prefix("no_data="))
            .unwrap_or_else(|| panic!("{lang} child printed nothing:\n{stdout}"));
        assert_eq!(child_no_data, no_data, "child did not run in {lang}");
        let panel: Vec<&str> = stdout
            .lines()
            .filter_map(|l| l.strip_prefix("panel="))
            .collect();
        panel.join("\n")
    })
}

#[test]
fn wide_reset_rows_end_in_a_unit_at_180_wide_in_en_and_zh() {
    if std::env::var_os("ABTOP_LOCALE_CHILD").is_some() {
        let _home = TempHome::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (in_2h_58m, in_18d_23h) = (now + 178 * 60 + 30, now + 18 * 86_400 + 23 * 3_600 + 30);
        let mut app = default_layout_app(now);
        app.rate_limits = ["claude", "codex"]
            .map(|source| RateLimitInfo {
                source: source.into(),
                five_hour_pct: Some(20.0),
                five_hour_resets_at: Some(in_2h_58m),
                five_hour_window_minutes: Some(300),
                seven_day_pct: Some(20.0),
                seven_day_resets_at: Some(in_18d_23h),
                seven_day_window_minutes: Some(10_080),
                updated_at: Some(now),
            })
            .into();
        app.rate_limits.push(RateLimitInfo {
            source: "kiro".into(),
            seven_day_pct: Some(20.0),
            seven_day_resets_at: Some(in_2h_58m),
            seven_day_window_minutes: Some(43_200),
            updated_at: Some(now),
            ..Default::default()
        });
        print_quota_panel(&app, 180);
        return;
    }
    let panels = panels_in_en_and_zh("wide_reset_rows_end_in_a_unit_at_180_wide_in_en_and_zh");
    for ((lang, units), panel) in [("en", "mhd"), ("zh", "分时天")].into_iter().zip(panels) {
        let mut checked = 0;
        for idx in 0..3 {
            let col = column(&panel, idx);
            // Header, then bar/reset pairs; the last row is the totals line.
            for k in (1..col.len() - 2).step_by(2) {
                let (bar, reset) = (&col[k], col[k + 1].trim_end());
                if bar.trim().is_empty() {
                    continue;
                }
                // At 180 wide CLAUDE and CODEX are 11 cells (compact), KIRO 12.
                if idx == 2 {
                    assert!(bar.contains('■'), "{lang}: no bar in {bar:?}:\n{panel}");
                }
                let last = reset.chars().last().unwrap_or(' ');
                assert!(
                    units.contains(last),
                    "{lang}: {reset:?} ends mid-unit:\n{panel}"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 5, "{lang}: rows missing:\n{panel}");
    }
}

#[test]
fn a_6_cell_column_shows_the_percent_alone() {
    if std::env::var_os("ABTOP_LOCALE_CHILD").is_some() {
        let _home = TempHome::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut app = default_layout_app(now);
        app.rate_limits = vec![RateLimitInfo {
            source: "kiro".into(),
            seven_day_pct: Some(0.0),
            seven_day_resets_at: Some(now + 19 * 86_400 + 23 * 3_600 + 30),
            seven_day_window_minutes: Some(43_200),
            updated_at: Some(now),
            ..Default::default()
        }];
        print_quota_panel(&app, 100);
        return;
    }
    let panels = panels_in_en_and_zh("a_6_cell_column_shows_the_percent_alone");
    for (lang, panel) in ["en", "zh"].into_iter().zip(panels) {
        let kiro = column(&panel, 2);
        assert_eq!(kiro[0].width(), 6, "{lang}: KIRO column:\n{panel}");
        assert!(
            kiro.iter().any(|row| row.contains("100%")),
            "{lang}: {kiro:?}:\n{panel}"
        );
    }
}

#[test]
fn an_11_cell_column_uses_the_compact_rows() {
    let _home = TempHome::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut app = default_layout_app(now);
    app.rate_limits.push(RateLimitInfo {
        source: "kiro".into(),
        seven_day_pct: Some(17.0),
        seven_day_resets_at: Some(now + 19 * 86_400 + 23 * 3_600 + 30),
        seven_day_window_minutes: Some(43_200),
        updated_at: Some(now),
        ..Default::default()
    });
    let mut terminal = Terminal::new(TestBackend::new(175, 40)).unwrap();
    terminal.draw(|f| abtop::ui::draw(f, &app)).unwrap();
    let panel = quota_panel(&format!("{}", terminal.backend()));

    let width = panel.lines().next().unwrap().chars().count();
    assert_eq!((width - 2) / 3, 11, "quota panel {width} wide:\n{panel}");
    let d = t("time.d");
    let within = t("quota.in");
    assert!(panel.contains(&format!("{within} 19{d}")), "{panel}");
    assert!(
        !panel.contains(&format!("19{d} 2")),
        "clipped hours:\n{panel}"
    );
    assert!(!panel.contains('■'), "bar in an 11-cell column:\n{panel}");
}

#[test]
fn demo_seeds_a_kiro_monthly_window() {
    let _home = TempHome::new();
    let mut app = quota_only_app(&[]);
    abtop::demo::populate_demo(&mut app);

    let sources: Vec<&str> = app.rate_limits.iter().map(|r| r.source.as_str()).collect();
    let kiro = app
        .rate_limits
        .iter()
        .find(|r| r.source == "kiro")
        .unwrap_or_else(|| panic!("no kiro entry in demo rate_limits: {sources:?}"));

    assert!(kiro.five_hour_pct.is_none(), "{kiro:?}");
    assert!(kiro.seven_day_pct.is_some(), "{kiro:?}");
    assert_eq!(kiro.seven_day_window_minutes, Some(43_200));
    assert!(
        kiro.seven_day_resets_at.unwrap() > kiro.updated_at.unwrap(),
        "{kiro:?}"
    );
}

#[cfg(not(windows))]
#[test]
fn kiro_cache_absent_returns_none() {
    let _home = TempHome::new();

    assert!(read_kiro_cache().is_none());
}

#[cfg(not(windows))]
#[test]
fn kiro_cache_without_a_window_returns_none() {
    let home = TempHome::new();
    home.write_kiro_cache(r#"{"source":"kiro","updated_at":1789990000}"#);

    assert!(read_kiro_cache().is_none());
}

#[test]
fn quota_panel_shows_claude_codex_kiro_columns_in_order() {
    let _home = TempHome::new();
    let screen = render_with_only_quota_mid_panel();

    let row = screen
        .lines()
        .find(|l| l.contains(" CLAUDE"))
        .unwrap_or_else(|| panic!("no CLAUDE column in:\n{screen}"));
    let at = |name: &str| {
        row.find(name)
            .unwrap_or_else(|| panic!("{name} missing: {row}"))
    };
    assert!(
        at("CLAUDE") < at("CODEX") && at("CODEX") < at("KIRO"),
        "{row}"
    );
}

#[test]
fn kiro_column_without_data_shows_kiro_setup_hint() {
    let _home = TempHome::new();
    let screen = render_with_only_quota_mid_panel();
    let (run_codex, kiro_setup) = (t("quota.run_codex"), t("quota.kiro_setup"));

    let row = screen
        .lines()
        .find(|l| l.contains(&t("quota.abtop_setup")))
        .unwrap_or_else(|| panic!("no hint row in:\n{screen}"));
    assert_eq!(row.matches(run_codex.as_str()).count(), 1, "{row}");
    let after_codex = row.find(&run_codex).unwrap() + run_codex.len();
    assert!(row[after_codex..].contains(&kiro_setup), "{row}");
}

#[test]
fn a_hidden_kiro_column_keeps_no_data_and_drops_the_login_hint() {
    let _home = TempHome::new();
    let screen = render(&quota_only_app(&["kiro".into()]));
    let panel = quota_panel(&screen);
    let cols: Vec<Vec<String>> = (0..3).map(|idx| column(&panel, idx)).collect();

    assert!(cols[2][0].contains("KIRO"), "{:?}:\n{panel}", cols[2]);
    for col in &cols {
        assert_eq!(
            col[1].trim(),
            format!("— {}", t("quota.no_data")),
            "\n{panel}"
        );
    }
    assert!(cols[0][2].contains(&t("quota.abtop_setup")), "\n{panel}");
    assert!(cols[1][2].contains(&t("quota.run_codex")), "\n{panel}");
    assert!(!screen.contains(&t("quota.kiro_setup")), "\n{screen}");
    assert!(cols[2][2].trim().is_empty(), "\n{panel}");
}

#[cfg(not(windows))]
#[test]
fn a_hidden_kiro_column_ignores_a_cache_left_by_a_visible_run() {
    let home = TempHome::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let resets_at = now + 20 * 86_400 + 5 * 3_600 + 30;
    home.write_kiro_cache(&format!(
        r#"{{"source":"kiro","seven_day":{{"used_percentage":15,"resets_at":{resets_at},"window_minutes":43200}},"updated_at":{now}}}"#
    ));
    let mut app = quota_only_app(&["kiro".into()]);
    app.tick_no_summaries();
    assert!(
        app.rate_limits.iter().all(|r| r.source != "kiro"),
        "stale kiro cache leaked into rate_limits"
    );

    let panel = quota_panel(&render(&app));
    let kiro = column(&panel, 2);

    assert!(kiro[0].contains("KIRO"), "\n{panel}");
    assert_eq!(
        kiro[1].trim(),
        format!("— {}", t("quota.no_data")),
        "\n{panel}"
    );
    assert!(kiro[2].trim().is_empty(), "\n{panel}");
}

#[test]
fn a_no_data_hint_is_whole_or_absent() {
    let _home = TempHome::new();
    let app = App::new_with_config_and_claude_dirs(
        Theme::default(),
        &[],
        PanelVisibility::default(),
        &[],
    );
    for width in [190, 300] {
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
        terminal.draw(|f| abtop::ui::draw(f, &app)).unwrap();
        let panel = quota_panel(&format!("{}", terminal.backend()));
        for (idx, key) in ["quota.abtop_setup", "quota.run_codex", "quota.kiro_setup"]
            .into_iter()
            .enumerate()
        {
            let (hint, col_w) = (t(key), column(&panel, idx)[0].width());
            // The hint row is ` {hint}`, so it fits only below the column width.
            if hint.width() < col_w {
                assert!(panel.contains(&hint), "{width}: no {hint:?} in:\n{panel}");
            } else {
                let start: String = hint.chars().take(4).collect();
                assert!(
                    !panel.contains(&start),
                    "{width}: {col_w}-cell column clips {hint:?}:\n{panel}"
                );
            }
        }
    }
}

/// The locale is fixed once per process from LANG, so each language runs in a child.
#[test]
fn kiro_setup_hint_is_translated_in_en_and_zh() {
    if std::env::var_os("ABTOP_LOCALE_CHILD").is_some() {
        println!(
            "kiro_setup={}|no_data={}",
            t("quota.kiro_setup"),
            t("quota.no_data")
        );
        return;
    }
    let _home = TempHome::new();
    for (lang, no_data, expected) in [
        ("en_US.UTF-8", "no data", "kiro-cli login"),
        ("zh_CN.UTF-8", "无数据", "登录 kiro-cli"),
    ] {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kiro_setup_hint_is_translated_in_en_and_zh",
                "--nocapture",
            ])
            .env("ABTOP_LOCALE_CHILD", "1")
            .env("LANG", lang)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let (hint, child_no_data) = stdout
            .lines()
            .find_map(|l| l.strip_prefix("kiro_setup="))
            .and_then(|l| l.split_once("|no_data="))
            .unwrap_or_else(|| panic!("{lang} child printed no hint:\n{stdout}"));

        assert_eq!(child_no_data, no_data, "child did not run in {lang}");
        assert_eq!(hint, expected, "{lang}");
    }
}
