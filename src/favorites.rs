//! Favourites, kept by the plugin since an indexer has no user library:
//! `<data_dir>/favorites.json`, the saved items with their metadata so
//! that the library lists need no network. Newest first.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

#[derive(Default)]
pub struct Favorites {
    path: PathBuf,
    items: Vec<Value>,
}

impl Favorites {
    pub fn load(dir: &Path) -> Favorites {
        let path = dir.join("favorites.json");
        let items = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|i| i["ref"].is_string())
            .collect();
        Favorites { path, items }
    }

    pub fn all(&self) -> &[Value] {
        &self.items
    }

    /// The saved items of one kind (`album`, `artist`, `track`).
    pub fn of_kind(&self, kind: &str) -> Vec<Value> {
        self.items
            .iter()
            .filter(|i| i["kind"] == kind)
            .cloned()
            .collect()
    }

    pub fn add(&mut self, item: Value) -> std::io::Result<()> {
        self.items.retain(|i| i["ref"] != item["ref"]);
        self.items.insert(0, item);
        self.save()
    }

    pub fn remove(&mut self, r: &str) -> std::io::Result<()> {
        self.items.retain(|i| i["ref"] != r);
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json!({ "items": self.items }).to_string().as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("torznab-stream-fav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = Favorites::load(&dir);
        assert!(f.all().is_empty());
        f.add(json!({"ref": "i/a", "kind": "album"})).unwrap();
        f.add(json!({"ref": "c/b", "kind": "artist"})).unwrap();
        f.add(json!({"ref": "i/a", "kind": "album", "title": "again"}))
            .unwrap();
        let f2 = Favorites::load(&dir);
        assert_eq!(f2.all().len(), 2);
        assert_eq!(f2.all()[0]["title"], "again");
        assert_eq!(f2.of_kind("artist").len(), 1);
        f.remove("i/a").unwrap();
        assert_eq!(Favorites::load(&dir).all().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
