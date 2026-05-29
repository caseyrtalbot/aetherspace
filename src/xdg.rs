//! Hand-rolled XDG base-directory resolution (zero-dep). A `dirs`-style crate
//! resolves these to `~/Library/Application Support` on macOS, which is wrong for
//! a CLI tool that wants `~/.config` and `~/.local/state`. We read the XDG_* vars
//! ourselves with the spec's `$HOME/<fallback>` default.

use std::env;
use std::path::{Path, PathBuf};

/// `$<var>` if set to a non-empty absolute path, else `$HOME/<fallback>`. The XDG
/// spec requires these variables to be absolute; a relative or empty value is
/// invalid and ignored in favor of the default.
pub fn home(var: &str, fallback: &str) -> PathBuf {
    resolve(env::var(var).ok(), fallback)
}

/// Pure core of [`home`], split out so the absolute-path rule is unit-testable
/// without mutating process environment (which would race across test threads).
fn resolve(value: Option<String>, fallback: &str) -> PathBuf {
    match value {
        Some(x) if !x.is_empty() && Path::new(&x).is_absolute() => PathBuf::from(x),
        _ => PathBuf::from(env::var("HOME").unwrap_or_default()).join(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_value_is_used() {
        assert_eq!(
            resolve(Some("/srv/state".into()), ".local/state"),
            PathBuf::from("/srv/state")
        );
    }

    #[test]
    fn relative_value_is_ignored() {
        // A relative XDG value is invalid per spec, so we fall back to $HOME/...
        let got = resolve(Some("relative/dir".into()), ".config");
        assert!(got.is_absolute() || env::var("HOME").is_err());
        assert!(got.ends_with(".config"));
        assert_ne!(got, PathBuf::from("relative/dir"));
    }

    #[test]
    fn empty_and_unset_fall_back() {
        assert!(resolve(Some(String::new()), ".config").ends_with(".config"));
        assert!(resolve(None, ".config").ends_with(".config"));
    }
}
