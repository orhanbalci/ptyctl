# ptyctl

Keep interactive terminal sessions alive in the background and drive them one
command at a time from scripts, CI jobs or AI coding agents.

A tool that runs each shell command in a fresh process can't use a REPL,
`kubectl exec -it`, or a database console, because the state is lost as soon
as the command returns. `ptyctl` starts the program inside a PTY owned by a
small background daemon. Each `ptyctl` call connects, does one thing and
exits, while the session and its state keep running.

```console
$ ptyctl start shell --wait-for '>>> $' -- kubectl exec -it my-pod -- python manage.py shell
$ ptyctl run shell 'from app.models import Order; o = Order.objects.last()'
$ ptyctl run shell 'o.status'
'shipped'
$ ptyctl run shell '1/0'; echo "exit=$?"
Traceback (most recent call last):
  File "<ptyctl>", line 1, in <module>
ZeroDivisionError: division by zero
exit=1
$ ptyctl stop shell
```

## Install

```console
cargo install ptyctl
```

The latest version from GitHub, which may be ahead of crates.io:

```console
cargo install --git https://github.com/orhanbalci/ptyctl
```

Supports Linux and macOS.

## Commands

| Command | What it does |
| --- | --- |
| `ptyctl start <name> [--wait-for RE] -- <cmd>...` | Start `<cmd>` in a new PTY under a background daemon. |
| `ptyctl run <name> [code]` | Run code and print only its output (code from stdin if omitted). |
| `ptyctl send <name> [text] [--no-enter]` | Type text without waiting. |
| `ptyctl read <name> [--wait-for RE] [--wait-idle MS]` | Print output since the last `read`/`run`. |
| `ptyctl interrupt <name>` | Send Ctrl-C. |
| `ptyctl stop <name>` | Kill the command and remove the session. |
| `ptyctl ls [--json]` | List sessions: state, pid, uptime, time since last output, command. |
| `ptyctl status <name>` | Show one session as JSON. |
| `ptyctl attach <name> [--from-start] [--raw]` | Watch a session live, read-only. |
| `ptyctl clean [--older-than 7d] [--dry-run]` | Remove stale sockets and old logs of finished sessions. |

```console
$ ptyctl ls
NAME       STATE      PID    UPTIME  IDLE  COMMAND
django     running    43439  12m     8s    kubectl exec -it my-pod -- python manage.py shell
scratch    exited(0)  43471  2h      2h    python3 -q
```

### Watching a session

`ptyctl attach <name>` shows the last few KB of output and then follows new
output live, so you can watch while a script or agent drives the session. It
never sends input: keystrokes go nowhere, and Ctrl-C only stops watching. The
session keeps running. The base64 lines that `run` types are hidden, and each
run is framed as `── ptyctl run ──` … `── done (status N) ──`. Use `--raw` to
see the stream exactly as received.

### Cleaning up

Logs are kept after a session stops. `ptyctl clean` removes sockets left behind
by daemons that died, and the logs of sessions that are no longer running and
were last written more than `--older-than` ago (default `7d`, `0` for all).
Running sessions are never touched.

### `run` languages

`run` sends the code base64-encoded, wrapped with markers unique to each run,
so it knows exactly when the code finished. It also removes the terminal's
echo of the input and returns an exit status:

- `--lang python` (default): plain `python`, `manage.py shell`, IPython.
  Multi-line code works as-is, variables persist, and a trailing expression
  prints its `repr` like in the REPL. Exit status 1 if an exception is raised.
- `--lang sh`: bash, zsh, sh. The code is `eval`'d in the running shell, so
  `cd` and `export` persist. Exit status is the last command's status.
- `--lang raw` / `--prompt RE`: type the text as-is and wait until the
  output matches `RE` (e.g. `'mysql> $'`) or goes quiet for `--idle-ms`.
  Use this for REPLs ptyctl doesn't know.

Special exit codes: `124` = timed out (`--timeout`, default 60s; the command
keeps running, use `read` / `interrupt`), `125` = the session has exited.

## Details

- State lives in `~/.ptyctl` (override with `PTYCTL_DIR`). The directory is
  `0700` and sockets are `0600`: anyone who can write to a socket can run
  commands in that session.
- `~/.ptyctl/<name>.log` has the full raw output; `tail -f` it to watch live.
- The PTY is 250 columns wide by default (`--cols`) so long lines don't wrap.
- `TERM=dumb` and `PYTHON_BASIC_REPL=1` are set by default to keep output free
  of colors and redraws (`--term` / `--env` to change).
- After the command exits, the daemon lingers for 10 minutes so its last
  output can still be read, then cleans up.

## Protocol

The daemon speaks newline-delimited JSON on `~/.ptyctl/<name>.sock`, one
request per connection: `{"op":"status"}`, `{"op":"write","data":"..."}`,
`{"op":"read","since":0,"pattern":"...","idle_ms":500,"timeout_ms":30000}`,
`{"op":"stop"}`. Output offsets are absolute byte offsets from session start.
A `read` moves the session's read cursor unless it sets `"peek":true`, and
`"until_output":true` returns as soon as any new output exists (this is how
`attach` follows a session without disturbing `read`).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
