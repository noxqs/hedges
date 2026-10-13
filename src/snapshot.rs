//! Save and restore the whole desktop: every Hedge (position, contents and
//! settings) plus the desktop icons' own positions.
//!
//! The Hedge half is just our `config.json`, embedded verbatim. The desktop
//! half is GIO metadata: nemo records each icon's spot as
//! `metadata::nemo-icon-position` ("x,y") and `metadata::monitor` on the file
//! itself, so a snapshot reads those attributes and a restore writes them back.
//! Files are never moved, copied or deleted — only their metadata is touched.

use std::path::PathBuf;

use gio::prelude::*;
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};

const POS_ATTR: &str = "metadata::nemo-icon-position";
const STAMP_ATTR: &str = "metadata::nemo-icon-position-timestamp";
const MONITOR_ATTR: &str = "metadata::monitor";

/// One desktop entry's placement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesktopItem {
    /// File name within the desktop directory.
    pub name: String,
    /// `"x,y"` as nemo stores it; `None` if the icon was never positioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    /// Which monitor the icon sits on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Unix seconds, so the UI can say how old a snapshot is.
    pub saved_at: u64,
    /// Every hedge: position, size, settings and contents.
    pub config: Config,
    /// Desktop icon placements.
    pub desktop: Vec<DesktopItem>,
}

pub fn snapshot_path() -> PathBuf {
    config::config_dir().join("desktop-snapshot.json")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read every desktop entry's stored placement.
pub fn read_desktop() -> Vec<DesktopItem> {
    let dir = crate::desktop::desktop_dir();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let file = gio::File::for_path(entry.path());
        let (mut position, mut monitor) = (None, None);
        if let Ok(info) = file.query_info(
            "metadata::*",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        ) {
            position = info.attribute_string(POS_ATTR).map(|s| s.to_string());
            monitor = info.attribute_string(MONITOR_ATTR).map(|s| s.to_string());
        }
        out.push(DesktopItem {
            name,
            position,
            monitor,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Write the placements back. Returns how many entries were restored; entries
/// whose file no longer exists are skipped rather than treated as an error.
pub fn apply_desktop(items: &[DesktopItem]) -> usize {
    let dir = crate::desktop::desktop_dir();
    let stamp = now().to_string();
    let mut n = 0;
    for item in items {
        let path = dir.join(&item.name);
        if !path.exists() {
            continue;
        }
        let file = gio::File::for_path(&path);
        let mut touched = false;
        if let Some(pos) = &item.position {
            if file
                .set_attribute_string(
                    POS_ATTR,
                    pos,
                    gio::FileQueryInfoFlags::NONE,
                    gio::Cancellable::NONE,
                )
                .is_ok()
            {
                touched = true;
                // nemo stamps every placement; refresh it so the restored
                // position is treated as current rather than stale
                let _ = file.set_attribute_string(
                    STAMP_ATTR,
                    &stamp,
                    gio::FileQueryInfoFlags::NONE,
                    gio::Cancellable::NONE,
                );
            }
        }
        if let Some(mon) = &item.monitor {
            let _ = file.set_attribute_string(
                MONITOR_ATTR,
                mon,
                gio::FileQueryInfoFlags::NONE,
                gio::Cancellable::NONE,
            );
            touched = true;
        }
        if touched {
            n += 1;
        }
    }
    n
}

/// Capture the current config and desktop layout to `desktop-snapshot.json`.
pub fn save(cfg: &Config) -> std::io::Result<Snapshot> {
    let snap = Snapshot {
        saved_at: now(),
        config: cfg.clone(),
        desktop: read_desktop(),
    };
    let dir = config::config_dir();
    std::fs::create_dir_all(&dir)?;
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    snap.serialize(&mut ser)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let path = snapshot_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, &path)?;
    Ok(snap)
}

pub fn load() -> std::io::Result<Snapshot> {
    let text = std::fs::read_to_string(snapshot_path())?;
    serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Is there a snapshot to restore, and how old is it?
pub fn existing() -> Option<u64> {
    load().ok().map(|s| s.saved_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips() {
        let snap = Snapshot {
            saved_at: 1_700_000_000,
            config: config::default_config(),
            desktop: vec![
                DesktopItem {
                    name: "Thonny.desktop".into(),
                    position: Some("221,928".into()),
                    monitor: Some("0".into()),
                },
                // an icon nemo never positioned
                DesktopItem {
                    name: "notes.txt".into(),
                    position: None,
                    monitor: None,
                },
            ],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.saved_at, 1_700_000_000);
        assert_eq!(back.desktop.len(), 2);
        assert_eq!(back.desktop[0].position.as_deref(), Some("221,928"));
        assert_eq!(back.desktop[1].position, None);
        // the hedges ride along whole
        assert_eq!(back.config.hedges.len(), 1);
        assert_eq!(back.config.hedges["Hedge"].apps.len(), 4);
    }

    #[test]
    fn unpositioned_icons_are_omitted_from_json() {
        let item = DesktopItem {
            name: "x".into(),
            position: None,
            monitor: None,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert_eq!(json, r#"{"name":"x"}"#);
    }
}
