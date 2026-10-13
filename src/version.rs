//! Who am I, and am I newer than the copy on disk?
//!
//! `Cargo.toml`'s version alone can't answer that: during development every
//! build says `3.0.0`. So each binary also carries the UTC time it was built
//! (stamped by `build.rs`), and comparison falls through to it when the semver
//! triple ties. That makes "is this build newer than the installed one?" a
//! total order over any two Hedge binaries, released or hand-built.

use serde::{Deserialize, Serialize};

/// The identity of one Hedge binary.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Build {
    /// Semver from `Cargo.toml`, e.g. `3.0.0`.
    pub version: String,
    /// Unix seconds at compile time — the tie-breaker.
    pub build_epoch: u64,
    /// Human-readable form of `build_epoch`, e.g. `2026-08-09 12:15 UTC`.
    #[serde(default)]
    pub build_stamp: String,
    /// Short git hash, empty when built outside a checkout.
    #[serde(default)]
    pub git_hash: String,
}

impl Build {
    /// Sort key: semver triple first, build time as the tie-break.
    fn key(&self) -> (u64, u64, u64, u64) {
        let mut it = self
            .version
            .split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            self.build_epoch,
        )
    }

    /// Strictly newer than `other`?
    pub fn is_newer_than(&self, other: &Build) -> bool {
        self.key() > other.key()
    }

    /// `3.0.0 (built 2026-08-09 12:15 UTC)` — one line for the UI.
    pub fn label(&self) -> String {
        if self.build_stamp.is_empty() {
            self.version.clone()
        } else {
            format!("{} (built {})", self.version, self.build_stamp)
        }
    }

    /// Same thing plus the git hash, for `--version` and the About box.
    pub fn long_label(&self) -> String {
        match self.git_hash.as_str() {
            "" => self.label(),
            g => format!("{} [{g}]", self.label()),
        }
    }
}

/// This running binary's identity.
pub fn current() -> Build {
    Build {
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_epoch: env!("HEDGE_BUILD_EPOCH").parse().unwrap_or(0),
        build_stamp: env!("HEDGE_BUILD_STAMP").to_string(),
        git_hash: env!("HEDGE_GIT_HASH").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(v: &str, e: u64) -> Build {
        Build {
            version: v.into(),
            build_epoch: e,
            build_stamp: String::new(),
            git_hash: String::new(),
        }
    }

    #[test]
    fn semver_beats_build_time() {
        // an older *build* of a newer version still wins
        assert!(b("3.1.0", 100).is_newer_than(&b("3.0.9", 999)));
    }

    #[test]
    fn build_time_breaks_ties() {
        assert!(b("3.0.0", 200).is_newer_than(&b("3.0.0", 100)));
        assert!(!b("3.0.0", 100).is_newer_than(&b("3.0.0", 100)));
    }

    #[test]
    fn missing_or_odd_parts_dont_panic() {
        assert!(b("3.1", 0).is_newer_than(&b("3.0.9", 0)));
        assert!(b("3.0.1-rc1", 0).is_newer_than(&b("3.0.0", 0)));
    }
}
