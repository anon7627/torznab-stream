//! Track lengths, read from each FLAC file's header: torrents do not say
//! how long their files play. `<data_dir>/durations.json`, keyed by
//! `<release id>/<file index>`, in milliseconds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const KEPT: usize = 100_000;

#[derive(Default)]
pub struct Durations {
    path: PathBuf,
    ms: HashMap<String, u64>,
}

pub fn key(id: &str, index: usize) -> String {
    format!("{id}/{index}")
}

impl Durations {
    pub fn load(dir: &Path) -> Durations {
        let path = dir.join("durations.json");
        let ms = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Durations { path, ms }
    }

    pub fn get(&self, id: &str, index: usize) -> Option<u64> {
        self.ms.get(&key(id, index)).copied()
    }

    /// Remember a length; written at once (lengths come one by one, as
    /// headers arrive).
    pub fn set(&mut self, id: &str, index: usize, ms: u64) {
        if ms == 0 || self.ms.get(&key(id, index)) == Some(&ms) {
            return;
        }
        if self.ms.len() >= KEPT {
            self.ms.clear();
        }
        self.ms.insert(key(id, index), ms);
        if self.path.as_os_str().is_empty() {
            return;
        }
        let tmp = self.path.with_extension("tmp");
        let saved = serde_json::to_vec(&self.ms)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = saved {
            eprintln!("cannot save durations.json: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("torznab-stream-dur-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut d = Durations::load(&dir);
        d.set("l0000000000000001", 3, 245_000);
        d.set("l0000000000000001", 4, 0);
        let d = Durations::load(&dir);
        assert_eq!(d.get("l0000000000000001", 3), Some(245_000));
        assert_eq!(d.get("l0000000000000001", 4), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
