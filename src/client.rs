use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::proto::{Output, Request, Response, Status};

pub struct Client<'a> {
    socket: &'a Path,
    name: &'a str,
}

impl<'a> Client<'a> {
    pub fn new(socket: &'a Path, name: &'a str) -> Self {
        Client { socket, name }
    }

    pub fn call(&self, req: &Request, timeout: Option<Duration>) -> Result<Response> {
        let socket = crate::paths::short_socket_path(self.socket)?;
        let mut stream = UnixStream::connect(socket).with_context(|| {
            format!("no running session named '{}' (see `ptyctl ls`)", self.name)
        })?;
        stream.set_read_timeout(timeout.map(|t| t + Duration::from_secs(5)))?;
        serde_json::to_writer(&mut stream, req)?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .context("reading response from session daemon")?;
        if line.is_empty() {
            bail!("session daemon closed the connection");
        }
        let resp: Response = serde_json::from_str(&line).context("parsing daemon response")?;
        if let Response::Error { message } = resp {
            bail!(message);
        }
        Ok(resp)
    }

    pub fn status(&self) -> Result<Status> {
        match self.call(&Request::Status, None)? {
            Response::Status(s) => Ok(s),
            other => bail!("unexpected response: {other:?}"),
        }
    }

    pub fn write(&self, data: impl Into<String>) -> Result<()> {
        self.call(&Request::Write { data: data.into() }, None)?;
        Ok(())
    }

    pub fn read(
        &self,
        since: Option<u64>,
        pattern: Option<String>,
        idle: Option<Duration>,
        timeout: Option<Duration>,
    ) -> Result<Output> {
        let req = Request::Read {
            since,
            pattern,
            idle_ms: idle.map(|d| d.as_millis() as u64),
            timeout_ms: timeout.map(|d| d.as_millis() as u64),
        };
        match self.call(&req, timeout)? {
            Response::Output(o) => Ok(o),
            other => bail!("unexpected response: {other:?}"),
        }
    }

    pub fn stop(&self) -> Result<()> {
        self.call(&Request::Stop, None)?;
        Ok(())
    }
}
