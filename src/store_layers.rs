//! Wire format for the recursive `/nix/store` overlay layer stack shared by
//! `claudepod-start` and `claudepod-entry`: an ordered list of absolute paths
//! (highest-priority lower layer first) joined with `:`, passed via the
//! `CLAUDEPOD_STORE_LAYERS` env and the `/run/claudepod-store-layers` file.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use anyhow::{Result, bail};

pub const STORE_LAYERS_ENV: &str = "CLAUDEPOD_STORE_LAYERS";

pub fn join(paths: &[PathBuf]) -> OsString {
    let mut out = OsString::new();
    for (idx, path) in paths.iter().enumerate() {
        if idx > 0 {
            out.push(":");
        }
        out.push(path.as_os_str());
    }
    out
}

pub fn parse(value: &OsStr) -> Result<Vec<PathBuf>> {
    let paths = value
        .as_bytes()
        .split(|&byte| byte == b':')
        .map(|path| PathBuf::from(OsString::from_vec(path.to_vec())))
        .collect::<Vec<_>>();
    for path in &paths {
        if path.as_os_str().is_empty() {
            bail!("store layer list contains an empty layer");
        }
        if !path.is_absolute() {
            bail!("store layer {} is not absolute", path.display());
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::{join, parse};
    use std::ffi::OsStr;
    use std::path::PathBuf;

    #[test]
    fn join_parse_roundtrips() {
        let paths = vec![PathBuf::from("/nix/.l/0"), PathBuf::from("/nix/.l/1")];
        let joined = join(&paths);
        assert_eq!(joined, OsStr::new("/nix/.l/0:/nix/.l/1"));
        assert_eq!(parse(&joined).unwrap(), paths);
    }

    #[test]
    fn parse_rejects_empty_layers() {
        assert!(parse(OsStr::new("/nix/.l/0::/nix/.l/1")).is_err());
    }

    #[test]
    fn parse_rejects_relative_layers() {
        assert!(parse(OsStr::new("nix/.l/0")).is_err());
    }
}
