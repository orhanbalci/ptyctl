#[cfg(not(unix))]
compile_error!("ptyctl only supports Unix-like systems");

mod clean;
mod client;
mod daemon;
mod paths;
mod payload;
mod proto;

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use regex::Regex;

use clean::clean;
use client::Client;
use paths::{SessionPaths, ensure_state_dir, state_dir, validate_name};
use proto::Outcome;

/// Exit code when a wait times out (matches `timeout(1)`).
const EXIT_TIMEOUT: u8 = 124;
/// Exit code when the session's command exited before the run finished.
const EXIT_SESSION_GONE: u8 = 125;

/// Keep interactive terminal sessions (REPLs, shells, `kubectl exec -it`)
/// alive in the background and drive them from scripts, one command at a time.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a session running COMMAND in a new PTY.
    Start {
        name: String,
        /// Terminal width. Wide by default so long lines don't wrap.
        #[arg(long, default_value_t = 250)]
        cols: u16,
        #[arg(long, default_value_t = 50)]
        rows: u16,
        /// Working directory for the command (default: current directory).
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
        /// TERM for the command. `dumb` keeps REPL output free of colors and redraws.
        #[arg(long, default_value = "dumb")]
        term: String,
        /// Extra environment variables, KEY=VALUE.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Wait until the output matches this regex (e.g. '>>> $') and print it.
        #[arg(long)]
        wait_for: Option<String>,
        /// Seconds to wait for --wait-for.
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Run code in a session and print only its output.
    ///
    /// With --lang python (default) or sh, the code is wrapped with unique
    /// markers so completion is detected reliably and the input echo is
    /// removed. Multi-line code is fine. Exit status: python 0/1 (exception),
    /// sh the command's status, 124 on timeout, 125 if the session exited.
    Run {
        name: String,
        /// Code to run; read from stdin when omitted or '-'.
        code: Option<String>,
        #[arg(long, value_enum, default_value_t = Lang::Python)]
        lang: Lang,
        /// Raw mode only: regex marking the prompt's return, e.g. 'mysql> $'.
        /// Without it, raw mode waits for output to go quiet (--idle-ms).
        #[arg(long)]
        prompt: Option<String>,
        /// Raw mode only: quiet period that counts as done.
        #[arg(long, default_value_t = 500)]
        idle_ms: u64,
        /// Seconds to wait for completion; 0 waits forever.
        #[arg(long, default_value_t = 60.0)]
        timeout: f64,
    },
    /// Send text to a session without waiting (Enter is appended).
    Send {
        name: String,
        /// Text to send; read from stdin when omitted or '-'.
        text: Option<String>,
        /// Don't append Enter.
        #[arg(long)]
        no_enter: bool,
    },
    /// Print output since the last read/run (or since --since).
    Read {
        name: String,
        /// Absolute byte offset to read from (0 = session start).
        #[arg(long)]
        since: Option<u64>,
        /// Wait until the output matches this regex.
        #[arg(long)]
        wait_for: Option<String>,
        /// Wait until output has been quiet for this many milliseconds.
        #[arg(long)]
        wait_idle: Option<u64>,
        /// Seconds to wait when --wait-for/--wait-idle are given.
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
        /// Print output as received, ANSI codes and all.
        #[arg(long)]
        raw: bool,
    },
    /// Send Ctrl-C to a session.
    Interrupt { name: String },
    /// Stop a session: kill its command and remove its socket (the log stays).
    Stop { name: String },
    /// List sessions.
    Ls,
    /// Print a session's status as JSON.
    Status { name: String },
    #[command(name = "__daemon", hide = true)]
    Daemon { spec: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum Lang {
    /// Python REPL: python, `manage.py shell`, IPython.
    Python,
    /// POSIX shell: sh, bash, zsh.
    Sh,
    /// Send the text as typed and wait for --prompt or for quiet.
    Raw,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ptyctl: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Daemon { spec } => {
            daemon::run(serde_json::from_str(&spec)?)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Start {
            name,
            cols,
            rows,
            cwd,
            term,
            env,
            wait_for,
            timeout,
            command,
        } => start(name, cols, rows, cwd, term, env, wait_for, timeout, command),
        Cmd::Run {
            name,
            code,
            lang,
            prompt,
            idle_ms,
            timeout,
        } => run(&name, code, lang, prompt, idle_ms, timeout),
        Cmd::Send {
            name,
            text,
            no_enter,
        } => {
            let mut text = arg_or_stdin(text)?;
            if !no_enter {
                text.push('\r');
            }
            with_session(&name, |c| c.write(text))?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Read {
            name,
            since,
            wait_for,
            wait_idle,
            timeout,
            raw,
        } => {
            let out = with_session(&name, |c| {
                c.read(
                    since,
                    wait_for,
                    wait_idle.map(Duration::from_millis),
                    secs(timeout),
                )
            })?;
            if out.truncated {
                eprintln!("ptyctl: older output was dropped from the buffer; see the log file");
            }
            if raw {
                print!("{}", out.data);
            } else {
                print!("{}", without_markers(&clean(out.data.as_bytes())));
            }
            io::stdout().flush()?;
            Ok(match out.outcome {
                Outcome::Timeout => ExitCode::from(EXIT_TIMEOUT),
                _ => ExitCode::SUCCESS,
            })
        }
        Cmd::Interrupt { name } => {
            with_session(&name, |c| c.write("\x03"))?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Stop { name } => {
            with_session(&name, |c| c.stop())?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ls => ls(),
        Cmd::Status { name } => {
            let st = with_session(&name, |c| c.status())?;
            println!("{}", serde_json::to_string_pretty(&st)?);
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn secs(s: f64) -> Option<Duration> {
    (s > 0.0).then(|| Duration::from_secs_f64(s))
}

fn arg_or_stdin(arg: Option<String>) -> Result<String> {
    match arg.as_deref() {
        Some("-") | None => {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
        Some(_) => Ok(arg.unwrap()),
    }
}

fn with_session<T>(name: &str, f: impl FnOnce(&Client) -> Result<T>) -> Result<T> {
    validate_name(name)?;
    let paths = SessionPaths::new(&state_dir()?, name);
    f(&Client::new(&paths.socket, name))
}

#[allow(clippy::too_many_arguments)]
fn start(
    name: String,
    cols: u16,
    rows: u16,
    cwd: Option<std::path::PathBuf>,
    term: String,
    env: Vec<String>,
    wait_for: Option<String>,
    timeout: f64,
    command: Vec<String>,
) -> Result<ExitCode> {
    validate_name(&name)?;
    // Resolve before anything may change our cwd (see short_socket_path).
    let cwd = match cwd {
        Some(c) => c,
        None => std::env::current_dir()?,
    };
    if let Some(p) = &wait_for {
        Regex::new(p).context("invalid --wait-for regex")?;
    }
    let dir = ensure_state_dir()?;
    let paths = SessionPaths::new(&dir, &name);
    let client = Client::new(&paths.socket, &name);

    if paths.socket.exists() {
        if client.status().is_ok() {
            bail!("session '{name}' is already running (stop it with `ptyctl stop {name}`)");
        }
        fs::remove_file(&paths.socket)?;
    }

    let mut vars = vec![
        ("TERM".to_string(), term),
        // Keep Python 3.13+ on the plain REPL; its new one redraws heavily.
        ("PYTHON_BASIC_REPL".to_string(), "1".to_string()),
    ];
    for kv in env {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("--env expects KEY=VALUE, got {kv:?}"))?;
        vars.push((k.to_string(), v.to_string()));
    }
    let spec = daemon::Spec {
        name: name.clone(),
        dir: dir.clone(),
        command,
        cwd,
        cols,
        rows,
        env: vars,
    };

    let daemon_log = fs::File::create(&paths.daemon_log)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__daemon")
        .arg(serde_json::to_string(&spec)?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(daemon_log);
    // Detach from our session so the daemon outlives this process and its terminal.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("spawning session daemon")?;

    let began = Instant::now();
    let status = loop {
        if let Ok(st) = client.status() {
            break st;
        }
        if child.try_wait()?.is_some() || began.elapsed() > Duration::from_secs(10) {
            let log = fs::read_to_string(&paths.daemon_log).unwrap_or_default();
            bail!("session failed to start: {}", log.trim());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    eprintln!(
        "ptyctl: started '{name}' (pid {}), log: {}",
        status.child_pid.map_or("?".into(), |p| p.to_string()),
        status.log
    );

    let Some(pattern) = wait_for else {
        return Ok(ExitCode::SUCCESS);
    };
    let out = client.read(Some(0), Some(pattern), None, secs(timeout))?;
    print!("{}", clean(out.data.as_bytes()));
    io::stdout().flush()?;
    Ok(match out.outcome {
        Outcome::Timeout => {
            eprintln!("ptyctl: timed out waiting for --wait-for pattern");
            ExitCode::from(EXIT_TIMEOUT)
        }
        Outcome::Exited => {
            eprintln!("ptyctl: session exited");
            ExitCode::from(EXIT_SESSION_GONE)
        }
        _ => ExitCode::SUCCESS,
    })
}

fn run(
    name: &str,
    code: Option<String>,
    lang: Lang,
    prompt: Option<String>,
    idle_ms: u64,
    timeout: f64,
) -> Result<ExitCode> {
    let code = arg_or_stdin(code)?;
    let code = code.trim_end_matches('\n');
    let timeout = secs(timeout);
    with_session(name, |c| {
        let st = c.status()?;
        if let Some(code) = st.exit_code {
            eprintln!(
                "ptyctl: session '{name}' has exited (code {code}); `ptyctl read {name}` shows its last output"
            );
            return Ok(ExitCode::from(EXIT_SESSION_GONE));
        }
        let since = st.end;
        let lang = if prompt.is_some() { Lang::Raw } else { lang };

        let tag = unique_tag();
        let (lines, pattern, idle) = match lang {
            Lang::Python => (payload::python(code, &tag), done_pattern(&tag), None),
            Lang::Sh => (payload::sh(code, &tag), done_pattern(&tag), None),
            Lang::Raw => (
                code.lines().map(str::to_string).collect(),
                prompt.clone(),
                prompt.is_none().then(|| Duration::from_millis(idle_ms)),
            ),
        };
        let mut keys = String::new();
        for line in &lines {
            keys.push_str(line);
            keys.push('\r');
        }
        c.write(keys)?;
        let out = c.read(Some(since), pattern, idle, timeout)?;
        let text = clean(out.data.as_bytes());

        let (body, status) = match lang {
            Lang::Raw => (raw_body(&text, code, prompt.as_deref()), None),
            _ => {
                let e = payload::extract(&text, &tag);
                (e.body.unwrap_or(text), e.status)
            }
        };
        print!("{body}");
        io::stdout().flush()?;

        Ok(match out.outcome {
            Outcome::Timeout => {
                eprintln!(
                    "ptyctl: timed out; the command may still be running \
                     (`ptyctl read {name}` for more output, `ptyctl interrupt {name}` to stop it)"
                );
                ExitCode::from(EXIT_TIMEOUT)
            }
            Outcome::Exited => {
                eprintln!("ptyctl: session '{name}' exited");
                ExitCode::from(EXIT_SESSION_GONE)
            }
            _ => match status {
                Some(s) if s != 0 => ExitCode::from(s.clamp(1, 255) as u8),
                _ => ExitCode::SUCCESS,
            },
        })
    })
}

/// Hide `run`'s marker lines from plain `read` output.
fn without_markers(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !line.contains("__PTYCTL_BEGIN_") && !line.contains("__PTYCTL_DONE_"))
        .collect()
}

fn done_pattern(tag: &str) -> Option<String> {
    Some(format!(
        "{}[^\\n]*\\n",
        regex::escape(&payload::done_marker(tag))
    ))
}

fn unique_tag() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", nanos as u64, std::process::id())
}

/// Raw mode: drop the echoed first input line and the trailing prompt.
fn raw_body(text: &str, code: &str, prompt: Option<&str>) -> String {
    let mut body = text;
    let first = code.lines().next().unwrap_or("").trim();
    if let Some((line, rest)) = body.split_once('\n')
        && !first.is_empty()
        && line.contains(first)
    {
        body = rest;
    }
    if let Some(re) = prompt.and_then(|p| Regex::new(p).ok())
        && let Some(m) = re.find_iter(body).last()
    {
        let line_start = body[..m.start()].rfind('\n').map_or(0, |i| i + 1);
        body = &body[..line_start];
    }
    body.to_string()
}

fn ls() -> Result<ExitCode> {
    let dir = state_dir()?;
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(ExitCode::SUCCESS);
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".sock"))
                .map(str::to_string)
        })
        .collect();
    names.sort();
    for name in names {
        let paths = SessionPaths::new(&dir, &name);
        match Client::new(&paths.socket, &name).status() {
            Ok(st) => {
                let state = match st.exit_code {
                    Some(code) => format!("exited({code})"),
                    None => "running".to_string(),
                };
                println!(
                    "{name}\t{state}\tpid {}\t{}",
                    st.child_pid.map_or("?".into(), |p| p.to_string()),
                    st.command.join(" ")
                );
            }
            Err(_) => println!("{name}\tstale"),
        }
    }
    Ok(ExitCode::SUCCESS)
}
