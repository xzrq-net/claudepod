use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Result, anyhow, bail};

pub fn direct_basename(path: &Path) -> Result<&OsStr> {
    let raw = path.as_os_str().as_bytes();
    let Some(basename) = raw.strip_prefix(b"/nix/store/") else {
        bail!("not a /nix/store path: {}", path.display());
    };
    if basename.is_empty() || basename.contains(&b'/') {
        bail!("not a direct /nix/store path: {}", path.display());
    }
    Ok(OsStr::from_bytes(basename))
}

pub fn validate_direct(path: &Path) -> Result<()> {
    direct_name_parts(path).map(|_| ())
}

pub fn direct_hash(path: &Path) -> Result<&str> {
    let (basename, dash) = direct_name_parts(path)?;
    let basename = basename
        .to_str()
        .ok_or_else(|| anyhow!("store path is not valid UTF-8: {}", path.display()))?;
    Ok(&basename[..dash])
}

fn direct_name_parts(path: &Path) -> Result<(&OsStr, usize)> {
    let basename = direct_basename(path)?;
    let bytes = basename.as_bytes();
    let Some(dash) = bytes.iter().position(|byte| *byte == b'-') else {
        bail!("store path basename has no dash: {}", path.display());
    };
    if dash == 0 || dash + 1 == bytes.len() {
        bail!(
            "store path basename must be <hash>-<name>: {}",
            path.display()
        );
    }
    Ok((basename, dash))
}

#[cfg(test)]
mod tests {
    use super::{direct_hash, validate_direct};
    use std::path::Path;

    #[test]
    fn direct_hash_uses_direct_store_path_basename() {
        assert_eq!(
            direct_hash(Path::new("/nix/store/abc123-source")).unwrap(),
            "abc123"
        );
        assert!(direct_hash(Path::new("/tmp/abc123-source")).is_err());
        assert!(direct_hash(Path::new("/nix/store/abc123")).is_err());
        assert!(direct_hash(Path::new("/nix/store/abc123-source/bin")).is_err());
    }

    #[test]
    fn validate_direct_rejects_non_store_or_partial_store_paths() {
        assert!(validate_direct(Path::new("/nix/store/aaa111-one")).is_ok());
        assert!(validate_direct(Path::new("/tmp/aaa111-one")).is_err());
        assert!(validate_direct(Path::new("/nix/store/aaa111")).is_err());
        assert!(validate_direct(Path::new("/nix/store/aaa111-one/bin")).is_err());
    }
}
