//! Wire protocol between the CLI and a session daemon: one JSON request line,
//! one JSON response line, over a Unix socket.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    Write {
        data: String,
    },
    /// Return output after `since` (default: the session's read cursor).
    /// With `pattern` and/or `idle_ms`, block until the cleaned output matches,
    /// output has been quiet for `idle_ms`, the session exits, or `timeout_ms` passes.
    Read {
        since: Option<u64>,
        pattern: Option<String>,
        idle_ms: Option<u64>,
        timeout_ms: Option<u64>,
        /// Also finish as soon as any output after `since` exists.
        #[serde(default)]
        until_output: bool,
        /// Leave the session's read cursor alone (for watchers).
        #[serde(default)]
        peek: bool,
    },
    Stop,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status(Status),
    Output(Output),
    Error { message: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Status {
    pub name: String,
    pub daemon_pid: u32,
    pub child_pid: Option<u32>,
    pub command: Vec<String>,
    pub started_unix: u64,
    /// When the command last produced output; `None` if it never has.
    #[serde(default)]
    pub last_output_unix: Option<u64>,
    /// `Some(code)` once the command has exited.
    pub exit_code: Option<u32>,
    /// Absolute offset one past the last byte of output received.
    pub end: u64,
    pub log: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Output {
    /// Raw output (lossy UTF-8), ANSI codes included.
    pub data: String,
    pub start: u64,
    pub end: u64,
    /// True when `since` pointed before the retained buffer and output was lost.
    pub truncated: bool,
    pub outcome: Outcome,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Immediate,
    Matched,
    Idle,
    Exited,
    Timeout,
}
