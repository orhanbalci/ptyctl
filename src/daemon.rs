//! The background process that owns the PTY, records its output and serves
//! requests on the session socket.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::clean::clean;
use crate::paths::{SessionPaths, short_socket_path};
use crate::proto::{Outcome, Output, Request, Response, Status};

/// Output kept in memory; older bytes are dropped (the log file keeps everything).
const BUFFER_CAP: usize = 16 * 1024 * 1024;
/// How long the daemon stays around after the command exits, so its final
/// output can still be read.
const LINGER_AFTER_EXIT: Duration = Duration::from_secs(600);

#[derive(Serialize, Deserialize, Debug)]
pub struct Spec {
    pub name: String,
    pub dir: PathBuf,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
}

struct Buffer {
    data: Vec<u8>,
    /// Absolute offset of `data[0]`.
    start: u64,
    /// Where a `read` without `since` continues from.
    cursor: u64,
    last_output: Instant,
    last_output_unix: Option<u64>,
    eof: bool,
    exit_code: Option<u32>,
}

impl Buffer {
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    fn since(&self, offset: u64) -> (&[u8], u64, bool) {
        let from = offset.clamp(self.start, self.end());
        (
            &self.data[(from - self.start) as usize..],
            from,
            offset < self.start,
        )
    }
}

struct Session {
    spec: Spec,
    paths: SessionPaths,
    started_unix: u64,
    child_pid: Option<u32>,
    buf: Mutex<Buffer>,
    changed: Condvar,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

pub fn run(spec: Spec) -> Result<()> {
    let paths = SessionPaths::new(&spec.dir, &spec.name);

    let pair = native_pty_system().openpty(PtySize {
        rows: spec.rows,
        cols: spec.cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new(&spec.command[0]);
    cmd.args(&spec.command[1..]);
    cmd.cwd(&spec.cwd);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawning {:?}", spec.command))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&paths.log)
        .with_context(|| format!("opening {}", paths.log.display()))?;

    let session = Arc::new(Session {
        child_pid: child.process_id(),
        killer: Mutex::new(child.clone_killer()),
        started_unix: unix_now(),
        buf: Mutex::new(Buffer {
            data: Vec::new(),
            start: 0,
            cursor: 0,
            last_output: Instant::now(),
            last_output_unix: None,
            eof: false,
            exit_code: None,
        }),
        changed: Condvar::new(),
        writer: Mutex::new(writer),
        spec,
        paths,
    });

    // PTY output -> buffer + log.
    let s = session.clone();
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            let n = match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let _ = log.write_all(&chunk[..n]);
            let mut b = s.buf.lock().unwrap();
            b.data.extend_from_slice(&chunk[..n]);
            if b.data.len() > BUFFER_CAP {
                let drop_n = b.data.len() - BUFFER_CAP / 2;
                b.data.drain(..drop_n);
                b.start += drop_n as u64;
            }
            b.last_output = Instant::now();
            b.last_output_unix = Some(unix_now());
            s.changed.notify_all();
        }
        s.buf.lock().unwrap().eof = true;
        s.changed.notify_all();
    });

    // Child exit -> record status, linger, then shut down.
    let s = session.clone();
    thread::spawn(move || {
        let code = child.wait().map(|st| st.exit_code()).unwrap_or(1);
        // Give the reader a moment to drain the last output before reporting exit.
        let b = s.buf.lock().unwrap();
        let (mut b, _) = s
            .changed
            .wait_timeout_while(b, Duration::from_millis(300), |b| !b.eof)
            .unwrap();
        b.exit_code = Some(code);
        drop(b);
        s.changed.notify_all();
        thread::sleep(LINGER_AFTER_EXIT);
        shutdown(&s);
    });

    let _ = fs::remove_file(&session.paths.socket);
    let listener = UnixListener::bind(short_socket_path(&session.paths.socket)?)
        .with_context(|| format!("binding {}", session.paths.socket.display()))?;
    fs::set_permissions(&session.paths.socket, fs::Permissions::from_mode(0o600))?;

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let s = session.clone();
        thread::spawn(move || {
            if let Err(e) = handle(&s, stream) {
                eprintln!("ptyctl daemon: connection error: {e:#}");
            }
        });
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn shutdown(s: &Session) -> ! {
    let _ = fs::remove_file(&s.paths.socket);
    std::process::exit(0);
}

fn handle(s: &Session, stream: UnixStream) -> Result<()> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let req: Request = serde_json::from_str(&line).context("parsing request")?;

    let mut stop = false;
    let resp = match req {
        Request::Status => Response::Status(status(s)),
        Request::Write { data } => {
            let mut w = s.writer.lock().unwrap();
            match w.write_all(data.as_bytes()).and_then(|_| w.flush()) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: format!("write failed: {e}"),
                },
            }
        }
        Request::Read {
            since,
            pattern,
            idle_ms,
            timeout_ms,
            until_output,
            peek,
        } => match read(s, since, pattern, idle_ms, timeout_ms, until_output, peek) {
            Ok(out) => Response::Output(out),
            Err(e) => Response::Error {
                message: format!("{e:#}"),
            },
        },
        Request::Stop => {
            stop = true;
            Response::Ok
        }
    };

    let mut w = &stream;
    serde_json::to_writer(&mut w, &resp)?;
    w.write_all(b"\n")?;
    w.flush()?;

    if stop {
        let _ = s.killer.lock().unwrap().kill();
        shutdown(s);
    }
    Ok(())
}

fn status(s: &Session) -> Status {
    let b = s.buf.lock().unwrap();
    Status {
        name: s.spec.name.clone(),
        daemon_pid: std::process::id(),
        child_pid: s.child_pid,
        command: s.spec.command.clone(),
        started_unix: s.started_unix,
        last_output_unix: b.last_output_unix,
        exit_code: b.exit_code,
        end: b.end(),
        log: s.paths.log.display().to_string(),
    }
}

fn read(
    s: &Session,
    since: Option<u64>,
    pattern: Option<String>,
    idle_ms: Option<u64>,
    timeout_ms: Option<u64>,
    until_output: bool,
    peek: bool,
) -> Result<Output> {
    let pattern = pattern
        .map(|p| Regex::new(&p))
        .transpose()
        .context("invalid pattern")?;
    let idle = idle_ms.map(Duration::from_millis);
    let began = Instant::now();
    let deadline = timeout_ms.map(|t| began + Duration::from_millis(t));
    let waiting = pattern.is_some() || idle.is_some() || until_output;

    let mut b = s.buf.lock().unwrap();
    let since = since.unwrap_or(b.cursor);
    loop {
        let now = Instant::now();
        let quiet_since = b.last_output.max(began);
        let outcome = if !waiting {
            Some(Outcome::Immediate)
        } else if (until_output && b.end() > since)
            || pattern
                .as_ref()
                .is_some_and(|re| re.is_match(&clean(b.since(since).0)))
        {
            Some(Outcome::Matched)
        } else if b.exit_code.is_some() {
            Some(Outcome::Exited)
        } else if idle.is_some_and(|i| now >= quiet_since + i) {
            Some(Outcome::Idle)
        } else if deadline.is_some_and(|d| now >= d) {
            Some(Outcome::Timeout)
        } else {
            None
        };

        if let Some(outcome) = outcome {
            let (data, start, truncated) = b.since(since);
            let out = Output {
                data: String::from_utf8_lossy(data).into_owned(),
                start,
                end: b.end(),
                truncated,
                outcome,
            };
            if !peek {
                b.cursor = out.end;
            }
            return Ok(out);
        }

        let wake = [deadline, idle.map(|i| quiet_since + i)]
            .into_iter()
            .flatten()
            .min();
        b = match wake {
            Some(w) => {
                s.changed
                    .wait_timeout(b, w.saturating_duration_since(now))
                    .unwrap()
                    .0
            }
            None => s.changed.wait(b).unwrap(),
        };
    }
}
