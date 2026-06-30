use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

pub fn forwarded(name: &OsStr, value: &OsStr) -> bool {
    is_shell_identifier(name)
        && (name.as_bytes().starts_with(b"CLAUDE_CODE_")
            || (name == "MAX_THINKING_TOKENS" && !value.is_empty()))
}

fn is_shell_identifier(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    is_shell_identifier_start(first) && rest.iter().all(|&byte| is_shell_identifier_char(byte))
}

fn is_shell_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_shell_identifier_char(byte: u8) -> bool {
    is_shell_identifier_start(byte) || byte.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::forwarded;
    use std::ffi::OsStr;

    #[test]
    fn forwards_only_agent_env_with_shell_safe_names() {
        assert!(forwarded(OsStr::new("CLAUDE_CODE_TOKEN"), OsStr::new("x")));
        assert!(forwarded(
            OsStr::new("MAX_THINKING_TOKENS"),
            OsStr::new("1")
        ));

        assert!(!forwarded(
            OsStr::new("MAX_THINKING_TOKENS"),
            OsStr::new("")
        ));
        assert!(!forwarded(OsStr::new("CLAUDE-CODE-TOKEN"), OsStr::new("x")));
        assert!(!forwarded(OsStr::new("CLAUDE_CODE-BAD"), OsStr::new("x")));
        assert!(!forwarded(OsStr::new("PATH"), OsStr::new("x")));
    }
}
