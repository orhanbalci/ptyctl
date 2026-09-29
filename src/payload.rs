//! Turning user code into keystrokes for `ptyctl run`.
//!
//! The code is base64-encoded and sent in short assignment lines, then a
//! final line decodes and runs it. The decoded script prints a BEGIN marker,
//! runs the code, and prints a DONE marker with a status. Everything the
//! terminal echoes comes before BEGIN, and the markers never appear in the
//! echo (they are inside the base64), so the output between the markers is
//! exactly what the code printed.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Base64 characters per line. Keeps every line well under the 1024-byte
/// canonical-mode line limit of some terminals.
///
/// Lines also have to fit the terminal width (250 columns by default):
/// readline scrolls longer lines horizontally when echoing them, so the echo
/// no longer contains the `_ptyctl_c` name that `attach` uses to hide them.
const CHUNK: usize = 200;

const PY_WRAPPER: &str = r#"def _ptyctl_run(src, tag):
    import ast, sys, traceback
    g = globals()
    status = 0
    print('__PTYCTL_BEGIN_' + tag + '__', flush=True)
    try:
        tree = ast.parse(src, '<ptyctl>', 'exec')
        last = None
        if tree.body and isinstance(tree.body[-1], ast.Expr):
            last = ast.Expression(tree.body.pop().value)
        exec(compile(tree, '<ptyctl>', 'exec'), g)
        if last is not None:
            value = eval(compile(last, '<ptyctl>', 'eval'), g)
            if value is not None:
                g['_'] = value
                print(repr(value))
    except BaseException as e:
        status = 1
        tb = e.__traceback__.tb_next if e.__traceback__ else None
        traceback.print_exception(type(e), e, tb)
    finally:
        sys.stdout.flush()
        sys.stderr.flush()
        print('__PTYCTL_DONE_' + tag + '__ ' + str(status), flush=True)
_ptyctl_run(__import__('base64').b64decode('{SRC}').decode('utf-8'), '{TAG}')
del _ptyctl_run
"#;

pub fn begin_marker(tag: &str) -> String {
    format!("__PTYCTL_BEGIN_{tag}__")
}

pub fn done_marker(tag: &str) -> String {
    format!("__PTYCTL_DONE_{tag}__")
}

fn chunks(script: &str) -> Vec<String> {
    let b64 = STANDARD.encode(script);
    // base64 output is ASCII, so byte chunks are valid str slices.
    (0..b64.len())
        .step_by(CHUNK)
        .map(|i| b64[i..(i + CHUNK).min(b64.len())].to_string())
        .collect()
}

/// Lines to type into a Python REPL (plain `python`, Django shell, IPython).
/// Variables the code defines persist in the REPL's globals. If the last
/// statement is an expression, its repr is printed like in the REPL.
/// DONE status: 0 on success, 1 if an exception was raised.
pub fn python(code: &str, tag: &str) -> Vec<String> {
    let script = PY_WRAPPER
        .replace("{SRC}", &STANDARD.encode(code))
        .replace("{TAG}", tag);
    let mut lines: Vec<String> = chunks(&script)
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let op = if i == 0 { "=" } else { "+=" };
            format!("_ptyctl_c{op}'{c}'")
        })
        .collect();
    lines.push(
        "exec(__import__('base64').b64decode(_ptyctl_c).decode('utf-8')); del _ptyctl_c".into(),
    );
    lines
}

/// Lines to type into a POSIX shell. The code runs via `eval` in the current
/// shell, so `cd`, exported variables and functions persist.
/// DONE status: the exit status of the last command.
pub fn sh(code: &str, tag: &str) -> Vec<String> {
    let script = format!(
        "printf '%s\\n' '{begin}'\n{code}\nprintf '%s %s\\n' '{done}' \"$?\"\n",
        begin = begin_marker(tag),
        done = done_marker(tag),
    );
    let mut lines: Vec<String> = chunks(&script)
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if i == 0 {
                format!("_ptyctl_c='{c}'")
            } else {
                format!("_ptyctl_c=\"$_ptyctl_c\"'{c}'")
            }
        })
        .collect();
    lines.push("eval \"$(printf '%s' \"$_ptyctl_c\" | base64 -d)\"; unset _ptyctl_c".into());
    lines
}

/// Result of scanning cleaned output for a run's markers.
pub struct Extracted {
    /// Output between the markers, or everything after BEGIN if DONE is missing.
    pub body: Option<String>,
    pub status: Option<i32>,
}

pub fn extract(text: &str, tag: &str) -> Extracted {
    let begin = begin_marker(tag);
    let done = done_marker(tag);
    let Some(b) = text.find(&begin) else {
        return Extracted {
            body: None,
            status: None,
        };
    };
    let after = &text[b + begin.len()..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    match after.find(&done) {
        Some(d) => {
            let status = after[d + done.len()..]
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok());
            Extracted {
                body: Some(after[..d].to_string()),
                status,
            }
        }
        None => Extracted {
            body: Some(after.to_string()),
            status: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_never_contains_markers() {
        let code = "print('x' * 5000)\nfor i in range(3):\n    print(i)";
        for lines in [python(code, "abc"), sh(code, "abc")] {
            for line in &lines {
                assert!(!line.contains("__PTYCTL_"), "{line}");
                // Fits the default 250 columns after a prompt like ">>> ".
                assert!(line.len() < 240, "{} bytes: {line}", line.len());
            }
        }
    }

    #[test]
    fn extract_between_markers() {
        let text = ">>> junk echo\n__PTYCTL_BEGIN_t__\nhello\nworld\n__PTYCTL_DONE_t__ 3\n>>> ";
        let e = extract(text, "t");
        assert_eq!(e.body.as_deref(), Some("hello\nworld\n"));
        assert_eq!(e.status, Some(3));
    }
}
