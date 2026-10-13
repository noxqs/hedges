//! Stamp the binary with *when* it was built, so two copies of Hedge with the
//! same `Cargo.toml` version can still be ordered against each other.
//!
//! The build epoch is the real comparison key used by the installer/upgrade
//! check (`src/version.rs`); the human-readable stamp is only for display.
//! Honours `SOURCE_DATE_EPOCH` so a release pipeline can build reproducibly.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Rebuild (and therefore re-stamp) whenever the sources change. Without
    // these, cargo's default is the same package-wide scan; being explicit
    // keeps the env-changed line below from switching that default off.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=assets");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });

    println!("cargo:rustc-env=HEDGE_BUILD_EPOCH={epoch}");
    println!("cargo:rustc-env=HEDGE_BUILD_STAMP={}", stamp(epoch));
    println!("cargo:rustc-env=HEDGE_GIT_HASH={}", git_hash());
}

/// Short commit hash, or empty when built outside a git checkout (a release
/// tarball, say). Never fails the build.
fn git_hash() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// `2026-08-09 12:15 UTC`
fn stamp(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let secs = epoch % 86_400;
    let (y, m, d) = civil_from_days(days);
    let (hh, mm) = (secs / 3600, (secs % 3600) / 60);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

/// Days since the Unix epoch -> (year, month, day). Hinnant's civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}
