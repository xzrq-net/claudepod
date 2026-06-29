use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct NixRunRoots {
    paths: HashSet<PathBuf>,
}

impl NixRunRoots {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read(path)
            .with_context(|| format!("read nix run roots manifest {}", path.display()))?;
        parse_manifest(&raw)
            .with_context(|| format!("parse nix run roots manifest {}", path.display()))
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }
}

fn parse_manifest(raw: &[u8]) -> Result<NixRunRoots> {
    let mut paths = HashSet::new();
    let mut lines = raw.split(|byte| *byte == b'\n').peekable();
    let mut line_no = 1;
    while let Some(line) = lines.next() {
        let is_final_line = lines.peek().is_none();
        if line.is_empty() {
            if is_final_line {
                break;
            }
            bail!("blank line {line_no}");
        }

        let path = Path::new(OsStr::from_bytes(line)).to_path_buf();
        crate::store_path::validate_direct(&path).with_context(|| format!("line {line_no}"))?;
        paths.insert(path);
        line_no += 1;
    }
    Ok(NixRunRoots { paths })
}

#[cfg(test)]
mod tests {
    use super::parse_manifest;
    use std::path::Path;

    #[test]
    fn manifest_accepts_newline_separated_store_paths() {
        let roots = parse_manifest(
            b"/nix/store/aaa111-one\n/nix/store/bbb222-two\n/nix/store/ccc333-three\n",
        )
        .unwrap();
        assert_eq!(roots.paths.len(), 3);
        assert!(roots.contains(Path::new("/nix/store/aaa111-one")));
        assert!(roots.contains(Path::new("/nix/store/bbb222-two")));
        assert!(roots.contains(Path::new("/nix/store/ccc333-three")));
    }

    #[test]
    fn membership_uses_full_store_path() {
        let roots = parse_manifest(b"/nix/store/aaa111-one\n").unwrap();
        assert!(roots.contains(Path::new("/nix/store/aaa111-one")));
        assert!(!roots.contains(Path::new("/nix/store/aaa111-two")));
        assert!(!roots.contains(Path::new("/nix/store/bbb222-one")));
    }

    #[test]
    fn manifest_accepts_missing_final_newline_and_empty_file() {
        let roots = parse_manifest(b"/nix/store/aaa111-one").unwrap();
        assert_eq!(roots.paths.len(), 1);
        assert!(roots.contains(Path::new("/nix/store/aaa111-one")));
        assert!(parse_manifest(b"").unwrap().paths.is_empty());
    }

    #[test]
    fn manifest_rejects_blank_lines_except_final_trailing_newline() {
        assert!(parse_manifest(b"\n").is_err());
        assert!(parse_manifest(b"/nix/store/aaa111-one\n\n").is_err());
        assert!(parse_manifest(b"/nix/store/aaa111-one\n\n/nix/store/bbb222-two").is_err());
    }

    #[test]
    fn manifest_rejects_non_store_or_partial_store_paths() {
        assert!(parse_manifest(b"/tmp/aaa111-one\n").is_err());
        assert!(parse_manifest(b"/nix/store/aaa111\n").is_err());
        assert!(parse_manifest(b"/nix/store/aaa111-one/bin\n").is_err());
    }
}
