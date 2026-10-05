//! Releases seen in search results, by id, so that refs stay short and a
//! release can be opened (and its tracks played) after the search is gone:
//! `<data_dir>/releases.json`, the most recently seen kept.
//!
//! A release's id is its info hash when the indexer gives one, else `l`
//! and a hash of its link.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::torznab::Release;

const KEPT: usize = 5000;

/// FNV-1a, 64 bits: stable across runs and builds.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

pub fn id_of(r: &Release) -> String {
    if let Some(h) = &r.infohash {
        return h.clone();
    }
    let key = r.link.as_deref().or(r.magnet.as_deref()).unwrap_or(&r.title);
    // Links of the same release differ by the indexer's per-user key.
    let key = key.split(['?', '&']).filter(|p| !p.contains("apikey")).collect::<Vec<_>>().join("&");
    format!("l{:016x}", fnv(&key))
}

/// Release ids: 40 hex digits, or `l` and 16.
pub fn valid_id(id: &str) -> bool {
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    (id.len() == 40 && hex(id)) || (id.len() == 17 && id.starts_with('l') && hex(&id[1..]))
}

#[derive(Default)]
pub struct Catalog {
    path: PathBuf,
    /// id → (release, sequence number of the last time it was seen).
    releases: HashMap<String, (Release, u64)>,
    seq: u64,
    dirty: bool,
}

impl Catalog {
    pub fn load(dir: &Path) -> Catalog {
        let path = dir.join("releases.json");
        let list: Vec<(String, Release)> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let seq = list.len() as u64;
        let releases = list
            .into_iter()
            .enumerate()
            .filter(|(_, (id, _))| valid_id(id))
            .map(|(i, (id, r))| (id, (r, i as u64)))
            .collect();
        Catalog { path, releases, seq, dirty: false }
    }

    pub fn get(&self, id: &str) -> Option<&Release> {
        self.releases.get(id).map(|(r, _)| r)
    }

    /// Remember `r`; returns its id.
    pub fn put(&mut self, r: Release) -> String {
        let id = id_of(&r);
        self.seq += 1;
        self.releases.insert(id.clone(), (r, self.seq));
        if self.releases.len() > KEPT {
            let mut by_age: Vec<(u64, String)> = self.releases.iter().map(|(k, (_, s))| (*s, k.clone())).collect();
            by_age.sort();
            for (_, k) in by_age.into_iter().take(self.releases.len() - KEPT) {
                self.releases.remove(&k);
            }
        }
        self.dirty = true;
        id
    }

    /// Write the catalogue if it changed.
    pub fn save(&mut self) -> std::io::Result<()> {
        if !self.dirty || self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let mut list: Vec<(&String, &(Release, u64))> = self.releases.iter().collect();
        list.sort_by_key(|(_, (_, s))| *s);
        let list: Vec<(&String, &Release)> = list.into_iter().map(|(k, (r, _))| (k, r)).collect();
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string(&list)?.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(title: &str, link: &str) -> Release {
        Release { title: title.into(), link: Some(link.into()), ..Default::default() }
    }

    #[test]
    fn ids() {
        let a = rel("A", "http://h/dl?file=a&apikey=one");
        let b = rel("A", "http://h/dl?apikey=two&file=a");
        assert_eq!(id_of(&a), id_of(&b));
        assert!(valid_id(&id_of(&a)));
        let h = Release { infohash: Some("0123456789abcdef0123456789abcdef01234567".into()), ..a };
        assert_eq!(id_of(&h), "0123456789abcdef0123456789abcdef01234567");
        assert!(!valid_id("../etc"));
        assert!(!valid_id("0123456789ABCDEF0123456789ABCDEF01234567"));
    }

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("torznab-stream-cat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = Catalog::load(&dir);
        let id = c.put(rel("A", "http://h/a"));
        c.put(rel("B", "http://h/b"));
        c.save().unwrap();
        let c2 = Catalog::load(&dir);
        assert_eq!(c2.get(&id).unwrap().title, "A");
        assert_eq!(c2.releases.len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
