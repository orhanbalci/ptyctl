use std::path::PathBuf;
use std::process::{Command, Output};

struct Sessions {
    dir: PathBuf,
}

impl Sessions {
    fn new(test: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ptyctl-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Sessions { dir }
    }

    fn ptyctl(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ptyctl"))
            .args(args)
            .env("PTYCTL_DIR", &self.dir)
            .output()
            .unwrap()
    }

    fn stdout(&self, args: &[&str]) -> (String, i32) {
        let out = self.ptyctl(args);
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code().unwrap_or(-1),
        )
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        for name in ["py", "sh", "watched", "live"] {
            let _ = self.ptyctl(&["stop", name]);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn have(bin: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {bin}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn python_session() {
    if !have("python3") {
        eprintln!("skipping: python3 not found");
        return;
    }
    let s = Sessions::new("py");
    let (_, code) = s.stdout(&["start", "py", "--wait-for", ">>> $", "--", "python3", "-q"]);
    assert_eq!(code, 0);

    assert_eq!(s.stdout(&["run", "py", "x = 20"]), (String::new(), 0));
    assert_eq!(s.stdout(&["run", "py", "x + 1"]), ("21\n".into(), 0));
    assert_eq!(
        s.stdout(&["run", "py", "for i in range(2):\n    print('i', i)\nx * 2"]),
        ("i 0\ni 1\n40\n".into(), 0)
    );
    let (out, code) = s.stdout(&["run", "py", "raise ValueError('boom')"]);
    assert!(out.contains("ValueError: boom"), "{out}");
    assert_eq!(code, 1);

    let (_, code) = s.stdout(&[
        "run",
        "py",
        "--timeout",
        "0.5",
        "import time; time.sleep(3)",
    ]);
    assert_eq!(code, 124);
    s.ptyctl(&["interrupt", "py"]);
    assert_eq!(
        s.stdout(&["run", "py", "print('back', x)"]),
        ("back 20\n".into(), 0)
    );

    assert_eq!(s.ptyctl(&["stop", "py"]).status.code(), Some(0));
    assert_ne!(s.ptyctl(&["run", "py", "1"]).status.code(), Some(0));
}

#[test]
fn shell_session() {
    let s = Sessions::new("sh");
    let (_, code) = s.stdout(&[
        "start",
        "sh",
        "--env",
        "PS1=$ ",
        "--wait-for",
        "\\$ $",
        "--",
        "sh",
    ]);
    assert_eq!(code, 0);

    assert_eq!(
        s.stdout(&["run", "sh", "--lang", "sh", "cd / && V=ok"]),
        (String::new(), 0)
    );
    assert_eq!(
        s.stdout(&["run", "sh", "--lang", "sh", "pwd\necho \"$V\""]),
        ("/\nok\n".into(), 0)
    );
    assert_eq!(s.stdout(&["run", "sh", "--lang", "sh", "false"]).1, 1);
    assert_eq!(
        s.stdout(&["run", "sh", "--prompt", "\\$ $", "echo raw"]),
        ("raw\n".into(), 0)
    );
}

#[test]
fn ls_attach_and_clean() {
    let s = Sessions::new("mgmt");
    let (_, code) = s.stdout(&[
        "start",
        "watched",
        "--env",
        "PS1=$ ",
        "--wait-for",
        "\\$ $",
        "--",
        "sh",
    ]);
    assert_eq!(code, 0);

    // ls: a table with a header, and JSON with state and status fields.
    let (table, _) = s.stdout(&["ls"]);
    let mut lines = table.lines();
    assert!(lines.next().unwrap().starts_with("NAME"), "{table}");
    assert!(lines.next().unwrap().contains("running"), "{table}");
    let (json, _) = s.stdout(&["ls", "--json"]);
    let list: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(list[0]["name"], "watched");
    assert_eq!(list[0]["state"], "running");
    assert!(list[0]["started_unix"].is_u64());

    // attach: follows output live and never sends input.
    let watcher = Command::new(env!("CARGO_BIN_EXE_ptyctl"))
        .args(["attach", "watched"])
        .env("PTYCTL_DIR", &s.dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        s.stdout(&["run", "watched", "--lang", "sh", "echo from-run"]),
        ("from-run\n".into(), 0)
    );
    // `read` still sees output since the last run: attach did not move the cursor.
    s.ptyctl(&["send", "watched", "echo after-attach"]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (read, _) = s.stdout(&["read", "watched", "--wait-idle", "200"]);
    assert!(read.contains("after-attach"), "{read}");
    s.ptyctl(&["stop", "watched"]);
    let watched = watcher.wait_with_output().unwrap();
    let watched = String::from_utf8_lossy(&watched.stdout);
    assert!(
        watched.contains("── ptyctl run ──\nfrom-run\n── done (status 0) ──"),
        "{watched}"
    );
    assert!(!watched.contains("_ptyctl_c"), "{watched}");

    // clean: removes stale sockets and finished sessions' logs, keeps live ones.
    std::fs::write(s.dir.join("ghost.sock"), "").unwrap();
    s.stdout(&["start", "live", "--", "sh"]);
    let (dry, _) = s.stdout(&["clean", "--older-than", "0", "--dry-run"]);
    assert!(
        dry.contains("ghost.sock") && dry.contains("watched.log"),
        "{dry}"
    );
    assert!(s.dir.join("ghost.sock").exists());
    s.stdout(&["clean", "--older-than", "0"]);
    assert!(!s.dir.join("ghost.sock").exists());
    assert!(!s.dir.join("watched.log").exists());
    assert!(!s.dir.join("watched.daemon.log").exists());
    assert!(s.dir.join("live.log").exists());
    assert!(s.dir.join("live.daemon.log").exists());
    assert!(s.dir.join("live.sock").exists());
}
