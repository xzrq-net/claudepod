use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

pub const NAMES_ENV: &str = "CLAUDEPOD_AGENT_ENV_NAMES";

pub fn is_shell_identifier(name: &OsStr) -> bool {
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
    use super::is_shell_identifier;
    use std::ffi::OsStr;

    #[test]
    fn shell_identifiers_are_sourceable_by_bash() {
        assert!(is_shell_identifier(OsStr::new("FOO")));
        assert!(is_shell_identifier(OsStr::new("FOO_BAR")));
        assert!(is_shell_identifier(OsStr::new("_X1")));

        assert!(!is_shell_identifier(OsStr::new("")));
        assert!(!is_shell_identifier(OsStr::new("1BAD")));
        assert!(!is_shell_identifier(OsStr::new("BAD-NAME")));
    }
}
