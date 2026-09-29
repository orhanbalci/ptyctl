/// Strip ANSI escape sequences, carriage returns and other control characters,
/// keeping newlines and tabs.
pub fn clean(bytes: &[u8]) -> String {
    let stripped = strip_ansi_escapes::strip(bytes);
    String::from_utf8_lossy(&stripped)
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn strips_escapes_and_cr() {
        assert_eq!(clean(b"\x1b[1;32mok\x1b[0m\r\nnext\x07\r\n"), "ok\nnext\n");
    }
}
