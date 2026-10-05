//! Covers for releases whose torrent has none (or is not fetched yet):
//! MusicBrainz finds the release group from the artist and album names,
//! Cover Art Archive serves its front image.
//!
//! Lookups happen when the host asks the relay for a cover, so only the
//! covers shown cost a request. MusicBrainz allows one request a second:
//! lookups wait their turn. Answers, found or not, are kept in
//! `<cache_dir>/covers.json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MUSICBRAINZ: &str = "https://musicbrainz.org/ws/2";
const COVER_ART_ARCHIVE: &str = "https://coverartarchive.org";
/// Time between two MusicBrainz requests.
const SPACING: Duration = Duration::from_millis(1100);
/// A lookup that found nothing is tried again after this long.
const MISS_TTL: u64 = 7 * 86_400;
/// Lowest MusicBrainz search score taken as a match.
const MIN_SCORE: u64 = 90;
const KEPT: usize = 20_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mbid: Option<String>,
    at: u64,
}

pub struct Covers {
    http: reqwest::Client,
    musicbrainz: String,
    archive: String,
    path: PathBuf,
    known: Mutex<HashMap<String, Entry>>,
    /// When the last MusicBrainz request left; one lookup at a time.
    gate: tokio::sync::Mutex<Option<Instant>>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn key(artist: &str, album: &str) -> String {
    format!("{}\u{1f}{}", artist.trim().to_lowercase(), album.trim().to_lowercase())
}

/// `album` without what editions add to it: `Remastered`, `(Deluxe
/// Edition)`, `[2015 Remaster]`…
pub fn plain_album(album: &str) -> String {
    const EXTRA: &[&str] = &[
        "remastered", "remaster", "deluxe", "edition", "expanded", "anniversary", "bonus", "tracks", "version", "reissue",
    ];
    let mut s = String::new();
    let mut depth = 0usize;
    for c in album.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => s.push(c),
            _ => {}
        }
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut end = words.len();
    while end > 1 {
        let w = words[end - 1].trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        let year = w.len() == 4 && w.parse::<u32>().is_ok_and(|y| (1880..=2100).contains(&y));
        if EXTRA.contains(&w.as_str()) || year || w.is_empty() || w.ends_with("th") && w[..w.len() - 2].parse::<u32>().is_ok() {
            end -= 1;
        } else {
            break;
        }
    }
    let out = words[..end].join(" ").trim_end_matches(['-', ',', ':', ' ']).to_string();
    if out.is_empty() { album.trim().to_string() } else { out }
}

/// A Lucene phrase for MusicBrainz' search.
fn phrase(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

pub fn query(artist: &str, album: &str) -> String {
    format!("releasegroup:{} AND artist:{}", phrase(&plain_album(album)), phrase(artist.trim()))
}

/// The best release group of a search answer, if it is a match.
pub fn best(answer: &Value) -> Option<String> {
    let g = answer["release-groups"].as_array()?.first()?;
    let score = g["score"].as_u64().or_else(|| g["score"].as_str()?.parse().ok())?;
    let id = g["id"].as_str()?;
    let ok = score >= MIN_SCORE && id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    ok.then(|| id.to_string())
}

impl Covers {
    pub fn new(cache_dir: &std::path::Path) -> Covers {
        let path = cache_dir.join("covers.json");
        let known = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("torznab-stream/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(8))
            .build()
            .expect("HTTP client");
        // Tests point these at a local server.
        let env = |k: &str, d: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string());
        Covers {
            http,
            musicbrainz: env("TORZNAB_STREAM_MUSICBRAINZ", MUSICBRAINZ),
            archive: env("TORZNAB_STREAM_COVER_ART_ARCHIVE", COVER_ART_ARCHIVE),
            path,
            known: Mutex::new(known),
            gate: tokio::sync::Mutex::new(None),
        }
    }

    fn cached(&self, k: &str) -> Option<Option<String>> {
        let known = self.known.lock().unwrap();
        let e = known.get(k)?;
        (e.mbid.is_some() || now().saturating_sub(e.at) < MISS_TTL).then(|| e.mbid.clone())
    }

    /// The image URL of the front cover of `artist`'s `album`.
    pub async fn front(&self, artist: &str, album: &str) -> Option<String> {
        let k = key(artist, album);
        let mbid = match self.cached(&k) {
            Some(m) => m,
            None => {
                let mut last = self.gate.lock().await;
                // Someone may have looked it up while we waited.
                match self.cached(&k) {
                    Some(m) => m,
                    None => {
                        if let Some(at) = *last {
                            tokio::time::sleep(SPACING.saturating_sub(at.elapsed())).await;
                        }
                        let found = self.search(artist, album).await;
                        *last = Some(Instant::now());
                        drop(last);
                        // A failed request is not an answer: try again later.
                        let found = found.ok()?;
                        self.remember(k, found.clone());
                        found
                    }
                }
            }
        }?;
        Some(format!("{}/release-group/{mbid}/front-500", self.archive))
    }

    async fn search(&self, artist: &str, album: &str) -> Result<Option<String>, ()> {
        let url = format!(
            "{}/release-group/?fmt=json&limit=1&query={}",
            self.musicbrainz,
            urlencoding::encode(&query(artist, album))
        );
        let resp = self.http.get(&url).send().await.map_err(|e| eprintln!("MusicBrainz: {}", e.without_url()))?;
        if !resp.status().is_success() {
            eprintln!("MusicBrainz: HTTP {}", resp.status().as_u16());
            return Err(());
        }
        let body = resp.bytes().await.map_err(|e| eprintln!("MusicBrainz: {}", e.without_url()))?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| eprintln!("MusicBrainz: {e}"))?;
        Ok(best(&v))
    }

    fn remember(&self, k: String, mbid: Option<String>) {
        let mut known = self.known.lock().unwrap();
        known.insert(k, Entry { mbid, at: now() });
        if known.len() > KEPT {
            let mut by_age: Vec<(u64, String)> = known.iter().map(|(k, e)| (e.at, k.clone())).collect();
            by_age.sort();
            for (_, k) in by_age.into_iter().take(known.len() - KEPT) {
                known.remove(&k);
            }
        }
        let tmp = self.path.with_extension("tmp");
        let saved = serde_json::to_vec(&*known)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = saved {
            eprintln!("cannot save covers.json: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn albums() {
        assert_eq!(plain_album("Kill Em All Remastered"), "Kill Em All");
        assert_eq!(plain_album("Abbey Road (Super Deluxe Edition) [2019 Remaster]"), "Abbey Road");
        assert_eq!(plain_album("Goldberg Variations 1955"), "Goldberg Variations");
        assert_eq!(plain_album("Rumours 35th Anniversary Edition"), "Rumours");
        assert_eq!(plain_album("1999"), "1999");
        assert_eq!(plain_album("(Untitled)"), "(Untitled)");
    }

    #[test]
    fn queries() {
        assert_eq!(query("Glenn Gould", "Goldberg Variations"), r#"releasegroup:"Goldberg Variations" AND artist:"Glenn Gould""#);
        assert_eq!(query("A \"B\"", "C\\D"), r#"releasegroup:"C\\D" AND artist:"A \"B\"""#);
    }

    #[test]
    fn answers() {
        let id = "0d4cf3a9-7d6c-3d7b-ae7a-3b8a2a0a6b1e";
        assert_eq!(best(&json!({"release-groups": [{"id": id, "score": 100}]})).as_deref(), Some(id));
        assert_eq!(best(&json!({"release-groups": [{"id": id, "score": "95"}]})).as_deref(), Some(id));
        assert_eq!(best(&json!({"release-groups": [{"id": id, "score": 60}]})), None);
        assert_eq!(best(&json!({"release-groups": [{"id": "../x", "score": 100}]})), None);
        assert_eq!(best(&json!({"release-groups": []})), None);
    }

    #[tokio::test]
    async fn cache_and_misses() {
        let dir = std::env::temp_dir().join(format!("torznab-stream-covers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Covers::new(&dir);
        c.remember(key("A", "B"), Some("0d4cf3a9-7d6c-3d7b-ae7a-3b8a2a0a6b1e".into()));
        c.remember(key("A", "Nothing"), None);
        let c = Covers::new(&dir);
        assert_eq!(
            c.front(" a ", "b").await.as_deref(),
            Some("https://coverartarchive.org/release-group/0d4cf3a9-7d6c-3d7b-ae7a-3b8a2a0a6b1e/front-500")
        );
        // A recent miss is not asked again.
        assert_eq!(c.front("A", "Nothing").await, None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
