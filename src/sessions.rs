//! Commands that look across sessions or watch one: `ls`, `attach`, `clean`.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use regex::Regex;

use crate::clean::clean;
use crate::client::Client;
use crate::paths::{SessionPaths, state_dir, validate_name};
use crate::proto::{Outcome, Status};

/// Bytes of earlier output `attach` shows before following live output.
const ATTACH_BACKLOG: u64 = 4096;
/// How long one `attach` poll waits before asking again.
const ATTACH_POLL: Duration = Duration::from_secs(30);
/// Quiet period after which `attach` shows an incomplete line (e.g. a prompt).
const ATTACH_QUIET: Duration = Duration::from_millis(150);

struct SessionInfo {
    name: String,
    /// `running`, `exited`, or `stale` (socket left behind by a dead daemon).
    state: &'static str,
    status: Option<Status>,
}

impl SessionInfo {
    /// The status fields plus `state`; just `name` and `state` when stale.
    fn to_json(&self) -> Result<serde_json::Value> {
        let mut obj = match &self.status {
            Some(st) => serde_json::to_value(st)?,
            None => serde_json::json!({ "name": self.name }),
        };
        obj["state"] = self.state.into();
        Ok(obj)
    }
}

fn session_names(dir: &Path, suffix: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(suffix))
                .filter(|n| validate_name(n).is_ok())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

fn sessions(dir: &Path) -> Vec<SessionInfo> {
    session_names(dir, ".sock")
        .into_iter()
        .map(|name| {
            let paths = SessionPaths::new(dir, &name);
            let status = Client::new(&paths.socket, &name).status().ok();
            let state = match &status {
                None => "stale",
                Some(st) if st.exit_code.is_some() => "exited",
                Some(_) => "running",
            };
            SessionInfo {
                name,
                state,
                status,
            }
        })
        .collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compact age like `45s`, `12m`, `3h`, `2d`.
pub fn fmt_age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// Parse `90`, `30s`, `10m`, `2h`, `7d`.
pub fn parse_age(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = match num.parse() {
        Ok(n) => n,
        Err(_) => bail!("invalid duration {s:?}: use e.g. 30s, 10m, 2h, 7d"),
    };
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => bail!("invalid duration {s:?}: use e.g. 30s, 10m, 2h, 7d"),
    };
    Ok(Duration::from_secs(secs))
}

pub fn ls(json: bool) -> Result<ExitCode> {
    let list = sessions(&state_dir()?);
    if json {
        let list = list
            .iter()
            .map(SessionInfo::to_json)
            .collect::<Result<Vec<_>>>()?;
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(ExitCode::SUCCESS);
    }
    if list.is_empty() {
        eprintln!("no sessions");
        return Ok(ExitCode::SUCCESS);
    }

    let now = unix_now();
    let mut rows = vec![[
        "NAME".to_string(),
        "STATE".to_string(),
        "PID".to_string(),
        "UPTIME".to_string(),
        "IDLE".to_string(),
        "COMMAND".to_string(),
    ]];
    for s in &list {
        let dash = || "-".to_string();
        let row = match &s.status {
            None => [
                s.name.clone(),
                s.state.into(),
                dash(),
                dash(),
                dash(),
                dash(),
            ],
            Some(st) => [
                s.name.clone(),
                match st.exit_code {
                    Some(code) => format!("exited({code})"),
                    None => s.state.into(),
                },
                st.child_pid.map_or_else(dash, |p| p.to_string()),
                fmt_age(now.saturating_sub(st.started_unix)),
                st.last_output_unix
                    .map_or_else(dash, |t| fmt_age(now.saturating_sub(t))),
                st.command.join(" "),
            ],
        };
        rows.push(row);
    }

    let mut widths = [0usize; 5];
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    for row in rows {
        let mut line = String::new();
        for (cell, w) in row[..5].iter().zip(widths) {
            line.push_str(&format!("{cell:<w$}  "));
        }
        line.push_str(&row[5]);
        println!("{}", line.trim_end());
    }
    Ok(ExitCode::SUCCESS)
}

/// A `run` input line that readline echoed horizontally scrolled because it
/// was wider than the terminal: a prompt, `<`, then base64 (e.g. `>>> <cmVl…'`).
static SCROLLED_ECHO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[>$#%.] )<[A-Za-z0-9+/=]{16,}'?\s*$").unwrap());

/// Rewrites `run`'s plumbing in cleaned output: hides the base64 input lines
/// and turns the BEGIN/DONE markers into readable separators.
///
/// Incomplete lines are held back until the line completes or output goes
/// quiet (`flush`): a prompt followed by a `run` input line must be hidden
/// together, but a prompt that is waiting for input should still show.
#[derive(Default)]
struct WatchFilter {
    pending: String,
    /// The last thing shown was a flushed partial line (no newline yet).
    mid_line: bool,
}

impl WatchFilter {
    fn render(line: &str) -> Option<String> {
        if line.contains("__PTYCTL_BEGIN_") {
            return Some("── ptyctl run ──\n".into());
        }
        if let Some(i) = line.find("__PTYCTL_DONE_") {
            let status = line[i..].split_whitespace().nth(1).unwrap_or("?");
            // Output printed without a trailing newline ends up before the marker.
            let before = &line[..i];
            let sep = if before.is_empty() { "" } else { "\n" };
            return Some(format!("{before}{sep}── done (status {status}) ──\n"));
        }
        if line.contains("_ptyctl_c") || SCROLLED_ECHO.is_match(line) {
            return None;
        }
        Some(line.to_string())
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Output has gone quiet: show the held-back partial line as is.
    fn flush(&mut self) -> String {
        let partial = std::mem::take(&mut self.pending);
        match Self::render(&partial) {
            Some(r) if !r.is_empty() && !partial.contains("__PTYCTL_") => {
                self.mid_line = !r.ends_with('\n');
                r
            }
            _ => String::new(),
        }
    }

    fn feed(&mut self, text: &str) -> String {
        let text = std::mem::take(&mut self.pending) + text;
        let mut out = String::new();
        let mut rest = text.as_str();
        while let Some(i) = rest.find('\n') {
            let (line, tail) = rest.split_at(i + 1);
            if let Some(r) = Self::render(line) {
                // Start a separator on its own line, not after a shown prompt.
                if self.mid_line && r.starts_with('─') {
                    out.push('\n');
                }
                out.push_str(&r);
                self.mid_line = false;
            }
            rest = tail;
        }
        self.pending = rest.to_string();
        out
    }
}

/// Follow a session's output live, read-only. Never writes to the session.
pub fn attach(name: &str, from_start: bool, raw: bool) -> Result<ExitCode> {
    validate_name(name)?;
    let paths = SessionPaths::new(&state_dir()?, name);
    let client = Client::new(&paths.socket, name);
    let st = client.status()?;

    let mut since = if from_start {
        0
    } else {
        st.end.saturating_sub(ATTACH_BACKLOG)
    };
    let mut skip_partial_line = since > 0;
    let mut filter = WatchFilter::default();
    let mut stdout = io::stdout();
    eprintln!("ptyctl: watching '{name}' (read-only, Ctrl-C to stop watching)");

    loop {
        let poll = if filter.has_pending() {
            ATTACH_QUIET
        } else {
            ATTACH_POLL
        };
        let out = match client.watch(since, poll) {
            Ok(out) => out,
            Err(_) => {
                stdout.write_all(filter.flush().as_bytes())?;
                eprintln!("\nptyctl: session '{name}' was stopped");
                return Ok(ExitCode::SUCCESS);
            }
        };
        let mut data = out.data.as_str();
        if skip_partial_line && !data.is_empty() {
            // The backlog starts mid-line; begin at the next full line.
            data = data.split_once('\n').map_or("", |(_, rest)| rest);
            skip_partial_line = false;
        }
        if raw {
            stdout.write_all(data.as_bytes())?;
        } else {
            stdout.write_all(filter.feed(&clean(data.as_bytes())).as_bytes())?;
            if matches!(out.outcome, Outcome::Timeout | Outcome::Exited) {
                stdout.write_all(filter.flush().as_bytes())?;
            }
        }
        stdout.flush()?;
        since = out.end;

        if out.outcome == Outcome::Exited {
            let code = client.status().ok().and_then(|s| s.exit_code);
            eprintln!(
                "\nptyctl: session '{name}' exited{}",
                code.map_or(String::new(), |c| format!(" (code {c})"))
            );
            return Ok(ExitCode::SUCCESS);
        }
    }
}

/// Remove stale sockets, and logs of sessions that are no longer running
/// and were last written more than `older_than` ago.
pub fn clean_up(older_than: Duration, dry_run: bool) -> Result<ExitCode> {
    let dir = state_dir()?;
    let mut live = HashSet::new();
    let mut doomed = Vec::new();
    for s in sessions(&dir) {
        if s.status.is_some() {
            live.insert(s.name);
        } else {
            doomed.push(SessionPaths::new(&dir, &s.name).socket);
        }
    }

    let now = SystemTime::now();
    for suffix in [".daemon.log", ".log"] {
        for name in session_names(&dir, suffix) {
            // `x.daemon.log` also ends in `.log`; it's handled by the first pass.
            if live.contains(&name) || (suffix == ".log" && name.ends_with(".daemon")) {
                continue;
            }
            let path = dir.join(format!("{name}{suffix}"));
            let age = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .unwrap_or(Duration::MAX);
            if age >= older_than {
                doomed.push(path);
            }
        }
    }
    doomed.sort();

    for path in &doomed {
        if dry_run {
            println!("would remove {}", path.display());
        } else {
            fs::remove_file(path)?;
            println!("removed {}", path.display());
        }
    }
    if doomed.is_empty() {
        eprintln!("nothing to clean");
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(fmt_age(5), "5s");
        assert_eq!(fmt_age(125), "2m");
        assert_eq!(fmt_age(7200), "2h");
        assert_eq!(fmt_age(3 * 86400), "3d");
        assert_eq!(parse_age("7d").unwrap(), Duration::from_secs(7 * 86400));
        assert_eq!(parse_age("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_age("0").unwrap(), Duration::ZERO);
        assert!(parse_age("5w").is_err());
        assert!(parse_age("m").is_err());
    }

    #[test]
    fn watch_filter_hides_plumbing() {
        let mut f = WatchFilter::default();
        let text = ">>> _ptyctl_c='abc'\n>>> exec(... _ptyctl_c ...)\n__PTYCTL_BEGIN_t__\nhello\n__PTYCTL_DONE_t__ 0\n>>> ";
        assert_eq!(
            f.feed(text),
            "── ptyctl run ──\nhello\n── done (status 0) ──\n"
        );
        assert_eq!(f.flush(), ">>> ");
    }

    #[test]
    fn watch_filter_hides_scrolled_echo() {
        let mut f = WatchFilter::default();
        let text =
            ">>> <cmVlLmJvZHlbLTFdLCBhc3QuRXhwcik6'\n>>> <YiA9IGUuX190cmFj\nprint <this> stays\n";
        assert_eq!(f.feed(text), "print <this> stays\n");
    }

    #[test]
    fn watch_filter_holds_partial_lines_until_quiet() {
        let mut f = WatchFilter::default();
        // The prompt before a plumbing line disappears with it...
        assert_eq!(f.feed("out\n>>> "), "out\n");
        assert_eq!(f.feed("_ptyctl"), "");
        assert_eq!(f.feed("_c='abc'\n>>> "), "");
        // ...while a prompt left waiting shows once output goes quiet.
        assert!(f.has_pending());
        assert_eq!(f.flush(), ">>> ");
        assert!(!f.has_pending());
        // A run started at that prompt begins on a fresh line.
        assert_eq!(
            f.feed("_ptyctl_c='x'\n__PTYCTL_BEGIN_t__\nhi\n"),
            "\n── ptyctl run ──\nhi\n"
        );
    }
}
