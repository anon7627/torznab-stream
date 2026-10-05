//! Releases and torrent files → the host's items, and whether the DAC takes
//! a FLAC file.
//!
//! Refs: `r/<id>` a release (shown as an album), `t/<id>/<index>` one of
//! its audio files, `a/<name>` an artist (a search by artist name).
//! Top-level sections use bare words (`recent`, `favorites`).

use serde_json::{Value, json};

use crate::catalog;
use crate::engine::Meta;
use crate::names;
use crate::torznab::Release;

#[derive(Debug, PartialEq)]
pub enum Ref<'a> {
    Section(&'a str),
    Artist(&'a str),
    Release(&'a str),
    Track(&'a str, usize),
}

pub fn parse_ref(r: &str) -> Option<Ref<'_>> {
    if r.len() > 1024 {
        return None;
    }
    match r.split_once('/') {
        None => (!r.is_empty() && r.bytes().all(|b| b.is_ascii_lowercase())).then_some(Ref::Section(r)),
        Some(("a", name)) => (!name.trim().is_empty() && !name.chars().any(char::is_control)).then_some(Ref::Artist(name)),
        Some(("r", id)) => catalog::valid_id(id).then_some(Ref::Release(id)),
        Some(("t", rest)) => {
            let (id, index) = rest.split_once('/')?;
            let index = index.parse().ok().filter(|_| index.bytes().all(|b| b.is_ascii_digit()))?;
            catalog::valid_id(id).then_some(Ref::Track(id, index))
        }
        _ => None,
    }
}

/// Artist, album and year of a release, from its Torznab attributes
/// first, then its title.
pub struct AlbumInfo {
    pub artist: Option<String>,
    pub album: String,
    pub year: Option<i64>,
}

pub fn album_info(r: &Release) -> AlbumInfo {
    let t = names::release_title(&r.title);
    AlbumInfo {
        artist: r.artist.clone().or(t.artist),
        album: r.album.clone().unwrap_or(t.album),
        year: r.year.or(t.year),
    }
}

/// What is known of a release whose search result is gone: its torrent's
/// name.
pub fn album_info_from_meta(meta: &Meta) -> AlbumInfo {
    let t = names::release_title(&meta.name);
    AlbumInfo { artist: t.artist, album: t.album, year: t.year }
}

fn artist_ref(name: &str) -> String {
    format!("a/{name}")
}

fn strip_nulls(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    v
}

pub fn release(id: &str, r: &Release, fr: bool) -> Value {
    let a = album_info(r);
    let seeders = r.seeders.map(|s| match (s, fr) {
        (0, false) => "no seeders".to_string(),
        (0, true) => "aucune source".to_string(),
        (1, false) => "1 seeder".to_string(),
        (1, true) => "1 source".to_string(),
        (n, false) => format!("{n} seeders"),
        (n, true) => format!("{n} sources"),
    });
    let parts: Vec<String> = [
        names::format_hint(&r.title),
        r.size.map(names::size),
        seeders,
        a.year.map(|y| y.to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();
    strip_nulls(json!({
        "ref": format!("r/{id}"),
        "kind": "album",
        "title": a.album,
        "artist": a.artist,
        "album_artist": a.artist,
        "year": a.year,
        "genre": r.genre,
        "subtitle": parts.join(" · "),
        "art": r.cover,
        "browsable": true,
        "artist_ref": a.artist.as_deref().map(artist_ref),
    }))
}

pub fn artist(name: &str) -> Value {
    json!({"ref": artist_ref(name), "kind": "artist", "title": name, "browsable": true})
}

/// The audio files of a release, in play order, as tracks. `art` is the
/// cover's URL, if any.
pub fn tracks(id: &str, meta: &Meta, a: &AlbumInfo, art: Option<&str>) -> Vec<Value> {
    // By disc, then path: leading track numbers compare as numbers.
    type Order = (u32, Vec<(u8, u64, String)>);
    let mut list: Vec<(Order, Value)> = meta
        .files
        .iter()
        .filter(|f| names::is_audio(&f.path))
        .map(|f| {
            let n = names::track_name(&f.path, a.artist.as_deref());
            let item = strip_nulls(json!({
                "ref": format!("t/{id}/{}", f.index),
                "kind": "track",
                "title": n.title,
                "artist": a.artist,
                "album": a.album,
                "album_artist": a.artist,
                "track_no": n.track_no,
                "disc_no": n.disc_no,
                "year": a.year,
                "art": art,
                "format": {"codec": names::codec(&f.path)},
                "playable": true,
                "album_ref": format!("r/{id}"),
                "artist_ref": a.artist.as_deref().map(artist_ref),
            }));
            ((n.disc_no.unwrap_or(1), names::natural_key(&f.path)), item)
        })
        .collect();
    list.sort_by(|x, y| x.0.cmp(&y.0));
    list.into_iter().map(|t| t.1).collect()
}

/// Sample rate, bits, channels and length in samples from the start of a
/// FLAC file (after an ID3v2 tag, if any).
pub fn streaminfo(b: &[u8]) -> Option<(u32, u8, u8, u64)> {
    let mut at = 0;
    if b.get(..3)? == b"ID3" {
        let h = b.get(6..10)?;
        let size = h.iter().fold(0usize, |s, x| (s << 7) | (*x as usize & 0x7f));
        at = 10 + size;
    }
    if b.get(at..at + 4)? != b"fLaC" || b.get(at + 4)? & 0x7f != 0 {
        return None;
    }
    let s = b.get(at + 8..at + 8 + 34)?;
    let rate = (u32::from(s[10]) << 12) | (u32::from(s[11]) << 4) | (u32::from(s[12]) >> 4);
    let channels = ((s[12] >> 1) & 0x07) + 1;
    let bits = (((s[12] & 0x01) << 4) | (s[13] >> 4)) + 1;
    let samples = (u64::from(s[13] & 0x0f) << 32) | u64::from(u32::from_be_bytes([s[14], s[15], s[16], s[17]]));
    (rate > 0).then_some((rate, bits, channels, samples))
}

/// What the DAC takes natively, from `initialize` / `output.changed`.
#[derive(Clone, Debug, Default)]
pub struct Output {
    pub bit_perfect: bool,
    pub max_rate: Option<u32>,
    pub max_bits: Option<u8>,
    pub rates: Vec<u32>,
}

impl Output {
    pub fn from_json(v: &Value) -> Output {
        Output {
            bit_perfect: v["bit_perfect"].as_bool().unwrap_or(false),
            max_rate: v["max_rate"].as_u64().map(|r| r as u32),
            max_bits: v["max_bits"].as_u64().map(|b| b as u8),
            rates: v["rates"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_u64).map(|r| r as u32).collect())
                .unwrap_or_default(),
        }
    }

    /// Whether the engine can play `rate` / `bits` without converting.
    /// Without a bit-perfect device (the null sink, a sound server) the
    /// engine has the last word.
    pub fn takes(&self, rate: u32, bits: u8) -> bool {
        if !self.bit_perfect {
            return true;
        }
        let rate_ok = if self.rates.is_empty() { self.max_rate.is_none_or(|m| rate <= m) } else { self.rates.contains(&rate) };
        rate_ok && self.max_bits.is_none_or(|m| bits <= m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::FileEntry;

    #[test]
    fn refs() {
        let h = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(parse_ref("recent"), Some(Ref::Section("recent")));
        assert_eq!(parse_ref(&format!("r/{h}")), Some(Ref::Release(h)));
        assert_eq!(parse_ref(&format!("t/{h}/12")), Some(Ref::Track(h, 12)));
        assert_eq!(parse_ref("a/AC/DC"), Some(Ref::Artist("AC/DC")));
        assert_eq!(parse_ref(&format!("t/{h}/+1")), None);
        assert_eq!(parse_ref("r/../x"), None);
        assert_eq!(parse_ref("a/ "), None);
    }

    #[test]
    fn album() {
        let r = Release {
            title: "Glenn Gould - Goldberg Variations (1955) [FLAC]".into(),
            seeders: Some(12),
            size: Some(312_000_000),
            ..Default::default()
        };
        let v = release("l0000000000000001", &r, false);
        assert_eq!(v["title"], "Goldberg Variations");
        assert_eq!(v["artist"], "Glenn Gould");
        assert_eq!(v["subtitle"], "FLAC · 312 MB · 12 seeders · 1955");
        assert_eq!(v["artist_ref"], "a/Glenn Gould");
        assert!(v.get("art").is_none());
    }

    #[test]
    fn track_list() {
        let meta = Meta::for_tests(
            "Gould",
            vec![
                FileEntry { index: 0, path: "G/10 - Var 9.flac".into(), len: 1 },
                FileEntry { index: 1, path: "G/cover.jpg".into(), len: 1 },
                FileEntry { index: 2, path: "G/02 - Var 1.flac".into(), len: 1 },
                FileEntry { index: 3, path: "G/01 - Aria.flac".into(), len: 1 },
            ],
        );
        let a = AlbumInfo { artist: Some("Glenn Gould".into()), album: "Goldberg".into(), year: None };
        let t = tracks("l0000000000000001", &meta, &a, Some("http://127.0.0.1:1/f/x/1/cover.jpg"));
        let titles: Vec<&str> = t.iter().map(|i| i["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["Aria", "Var 1", "Var 9"]);
        assert_eq!(t[0]["ref"], "t/l0000000000000001/3");
        assert_eq!(t[0]["format"]["codec"], "flac");
        assert_eq!(t[0]["album_ref"], "r/l0000000000000001");
    }

    #[test]
    fn flac_header() {
        let mut b = b"fLaC\x00\x00\x00\x22".to_vec();
        let mut si = [0u8; 34];
        // 96000 Hz, 2 channels, 24 bits, 960000 samples.
        let rate: u32 = 96000;
        si[10] = (rate >> 12) as u8;
        si[11] = (rate >> 4) as u8;
        si[12] = ((rate & 0x0f) as u8) << 4 | (1 << 1) | ((23 >> 4) & 1);
        si[13] = ((23 & 0x0f) << 4) as u8;
        si[14..18].copy_from_slice(&960_000u32.to_be_bytes());
        b.extend_from_slice(&si);
        assert_eq!(streaminfo(&b), Some((96000, 24, 2, 960_000)));
        assert_eq!(streaminfo(b"RIFF...."), None);
    }

    #[test]
    fn dac() {
        let o = Output { bit_perfect: true, max_rate: Some(96000), max_bits: Some(24), rates: vec![] };
        assert!(o.takes(96000, 24));
        assert!(!o.takes(192000, 24));
        assert!(Output::default().takes(384000, 32));
    }
}
