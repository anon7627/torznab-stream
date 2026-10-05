//! Torznab client: the indexer API spoken by Prowlarr, Jackett, bitmagnet
//! and others. `t=caps` says what the indexer can search, `t=search` and
//! `t=music` return an RSS feed of releases with `torznab:attr` extras.
//!
//! The API key travels as a query value: it is never logged (see
//! [`masked`]).

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Newznab audio categories.
pub const CAT_AUDIO: u32 = 3000;
pub const CAT_VIDEO: u32 = 3020;
pub const CAT_AUDIOBOOK: u32 = 3030;
pub const CAT_LOSSLESS: u32 = 3040;

#[derive(Debug)]
pub enum Error {
    /// Bad or missing API key (Newznab errors 100 to 102, HTTP 401/403).
    Auth(String),
    /// Request limit reached (Newznab 429/500/501, HTTP 429).
    RateLimited,
    /// The indexer answered, but not with something usable.
    Indexer(String),
    Network(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Auth(m) => write!(f, "the indexer refused the API key: {m}"),
            Error::RateLimited => write!(f, "the indexer's request limit is reached"),
            Error::Indexer(m) => write!(f, "indexer error: {m}"),
            Error::Network(m) => write!(f, "{m}"),
        }
    }
}

/// One search result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub title: String,
    /// Where the `.torrent` comes from (may redirect to a magnet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub magnet: Option<String>,
    /// Lower-case hex BTv1 info hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub infohash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeders: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peers: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer: Option<String>,
    /// Newznab category ids, the indexer's own included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<u32>,
}

/// One page of a feed.
#[derive(Debug, Default)]
pub struct Page {
    pub releases: Vec<Release>,
    /// From `newznab:response`, when the indexer gives it.
    pub total: Option<u64>,
}

/// What `t=caps` says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Caps {
    /// `t=music` is available, with these parameters (`q`, `artist`…).
    pub music: Option<Vec<String>>,
    /// Largest page the indexer serves.
    pub max_limit: Option<u64>,
    /// Every category the indexer declares, subcategories named
    /// `Parent/Sub`.
    pub categories: Vec<(u32, String)>,
}

/// Words that tell an indexer's own category is about sound.
const AUDIO_WORDS: &[&str] = &[
    "audio", "music", "musique", "musik", "música", "podcast", "audiobook", "audio book", "livre audio", "livres audio",
    "flac", "mp3", "lossless", "concert", "radio", "soundtrack", "ost",
];

impl Caps {
    /// The categories worth offering to a music player: Newznab's audio
    /// ones (3000 to 3999), and the indexer's own whose name is about
    /// sound.
    pub fn audio_categories(&self) -> Vec<(u32, String)> {
        self.categories
            .iter()
            .filter(|(id, name)| {
                let low = name.to_lowercase();
                let words: Vec<&str> = low.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
                // `podcasts` for `podcast`; short words whole only.
                let about_sound = AUDIO_WORDS.iter().any(|w| {
                    if w.contains(' ') {
                        low.contains(w)
                    } else if w.chars().count() >= 4 {
                        words.iter().any(|x| x.starts_with(w))
                    } else {
                        words.contains(w)
                    }
                });
                (3000..4000).contains(id) || (*id >= 100_000 && about_sound)
            })
            .cloned()
            .collect()
    }

    pub fn music_param(&self, p: &str) -> bool {
        self.music.as_ref().is_some_and(|m| m.iter().any(|x| x == p))
    }
}

/// A search request.
#[derive(Debug, Default)]
pub struct Query<'a> {
    pub q: Option<&'a str>,
    pub artist: Option<&'a str>,
    /// Comma-separated category ids.
    pub cat: &'a str,
    pub offset: u64,
    pub limit: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Indexer {
    /// The feed URL, without `apikey`, e.g.
    /// `http://localhost:9696/1/api` or `http://localhost:9117/api/v2.0/indexers/all/results/torznab/api`.
    pub url: String,
    pub apikey: Option<String>,
}

impl Indexer {
    /// From what the user pasted: a feed URL, optionally with
    /// `apikey=…` in it or followed by whitespace and the key.
    pub fn parse(input: &str) -> Option<Indexer> {
        let mut words = input.split_whitespace();
        let raw = words.next()?;
        let loose_key = words.next().map(str::to_string);
        if !(raw.starts_with("http://") || raw.starts_with("https://")) || words.next().is_some() {
            return None;
        }
        let (base, query) = match raw.split_once('?') {
            Some((b, q)) => (b, q),
            None => (raw, ""),
        };
        let mut apikey = None;
        let mut kept = Vec::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            match pair.split_once('=') {
                Some(("apikey", v)) => {
                    apikey = urlencoding::decode(v).ok().map(|v| v.into_owned());
                }
                // The feed's own parameters are ours to set.
                Some(("t" | "q" | "cat" | "offset" | "limit" | "extended", _)) => {}
                _ => kept.push(pair),
            }
        }
        let base = base.trim_end_matches('/');
        let url = if kept.is_empty() {
            base.to_string()
        } else {
            format!("{base}?{}", kept.join("&"))
        };
        let apikey = loose_key.or(apikey).filter(|k| !k.is_empty());
        Some(Indexer { url, apikey })
    }

    /// From the plugin's settings: the endpoint URL, with `/api` added
    /// when its path does not end with it, and the API key (the setting
    /// wins over an `apikey` left in the URL).
    pub fn from_settings(url: &str, apikey: &str) -> Option<Indexer> {
        if url.split_whitespace().count() != 1 {
            return None;
        }
        let mut ix = Indexer::parse(url)?;
        let (path, query) = match ix.url.split_once('?') {
            Some((p, q)) => (p.trim_end_matches('/').to_string(), format!("?{q}")),
            None => (ix.url.clone(), String::new()),
        };
        let after_host = path.find("://").map_or(0, |i| i + 3);
        let has_path = path[after_host..].contains('/');
        let path = if has_path && path.ends_with("/api") { path } else { format!("{path}/api") };
        ix.url = format!("{path}{query}");
        if !apikey.is_empty() {
            ix.apikey = Some(apikey.to_string());
        }
        Some(ix)
    }

    /// The download link of a release: links on the indexer's own host get
    /// the API key, as Newznab downloads (`t=get`) expect, unless they
    /// carry one already.
    pub fn download_url(&self, link: &str) -> String {
        let (Some(key), Some(host)) = (&self.apikey, origin(&self.url)) else {
            return link.to_string();
        };
        let has_key = link.split_once('?').is_some_and(|(_, q)| q.split('&').any(|p| p.starts_with("apikey=")));
        if origin(link) != Some(host) || has_key {
            return link.to_string();
        }
        let sep = if link.contains('?') { '&' } else { '?' };
        format!("{link}{sep}apikey={}", urlencoding::encode(key))
    }

    fn request_url(&self, params: &[(&str, String)]) -> String {
        let mut url = self.url.clone();
        let mut sep = if url.contains('?') { '&' } else { '?' };
        let key = self.apikey.iter().map(|k| ("apikey", k.clone()));
        for (k, v) in params.iter().cloned().chain(key) {
            url.push(sep);
            url.push_str(k);
            url.push('=');
            url.push_str(&urlencoding::encode(&v));
            sep = '&';
        }
        url
    }
}

/// Scheme, host and port of an http(s) URL, lower-cased.
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = rest.split(['/', '?', '#']).next()?;
    Some(format!("{scheme}://{}", host.to_ascii_lowercase()))
}

/// `url` without its query values, for logs and errors.
pub fn masked(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let names: Vec<String> = query
        .split('&')
        .map(|p| format!("{}=…", p.split('=').next().unwrap_or("")))
        .collect();
    format!("{base}?{}", names.join("&"))
}

pub struct Client {
    http: reqwest::Client,
}

impl Client {
    pub fn new() -> Client {
        let http = reqwest::Client::builder()
            .user_agent(concat!("torznab-stream/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(9))
            .build()
            .expect("HTTP client");
        Client { http }
    }

    async fn get(&self, ix: &Indexer, params: &[(&str, String)]) -> Result<String, Error> {
        let url = ix.request_url(params);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Network(format!("{}: {}", masked(&url), e.without_url())))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Network(e.without_url().to_string()))?;
        // Newznab errors come as XML, with any status.
        if let Some(e) = feed_error(&body) {
            return Err(e);
        }
        match status {
            200..=299 => Ok(body),
            401 | 403 => Err(Error::Auth(format!("HTTP {status}"))),
            429 => Err(Error::RateLimited),
            _ => Err(Error::Indexer(format!("HTTP {status} from {}", masked(&url)))),
        }
    }

    pub async fn caps(&self, ix: &Indexer) -> Result<Caps, Error> {
        let body = self.get(ix, &[("t", "caps".into())]).await?;
        parse_caps(&body)
    }

    pub async fn search(&self, ix: &Indexer, caps: &Caps, q: &Query<'_>) -> Result<Page, Error> {
        let limit = caps.max_limit.map_or(q.limit, |m| q.limit.min(m)).max(1);
        let mut params = vec![
            ("cat", q.cat.to_string()),
            ("offset", q.offset.to_string()),
            ("limit", limit.to_string()),
            ("extended", "1".to_string()),
        ];
        match (q.artist, caps.music_param("artist")) {
            (Some(a), true) => {
                params.push(("t", "music".into()));
                params.push(("artist", a.to_string()));
            }
            (Some(a), false) => {
                params.push(("t", "search".into()));
                params.push(("q", a.to_string()));
            }
            (None, _) => {
                params.push(("t", "search".into()));
                if let Some(text) = q.q.filter(|t| !t.trim().is_empty()) {
                    params.push(("q", text.trim().to_string()));
                }
            }
        }
        let body = self.get(ix, &params).await?;
        parse_feed(&body)
    }
}

fn feed_error(body: &str) -> Option<Error> {
    let doc = roxmltree::Document::parse(body).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "error" {
        return None;
    }
    let code: u32 = root.attribute("code").and_then(|c| c.parse().ok()).unwrap_or(0);
    let what = root.attribute("description").unwrap_or("").to_string();
    Some(match code {
        100..=102 => Error::Auth(what),
        429 | 500 | 501 => Error::RateLimited,
        _ => Error::Indexer(if what.is_empty() { format!("error {code}") } else { what }),
    })
}

pub fn parse_caps(body: &str) -> Result<Caps, Error> {
    let doc = roxmltree::Document::parse(body).map_err(|e| Error::Indexer(format!("bad caps: {e}")))?;
    let root = doc.root_element();
    if root.tag_name().name() != "caps" {
        return Err(Error::Indexer("this URL is not a Torznab feed (no caps)".into()));
    }
    let mut caps = Caps::default();
    for n in root.descendants().filter(|n| n.is_element()) {
        match n.tag_name().name() {
            "music-search" | "audio-search" if n.attribute("available") == Some("yes") => {
                let params = n
                    .attribute("supportedParams")
                    .unwrap_or("q")
                    .split(',')
                    .map(|p| p.trim().to_string())
                    .collect();
                caps.music = Some(params);
            }
            "limits" => caps.max_limit = n.attribute("max").and_then(|m| m.parse().ok()),
            "category" => {
                let Some(id) = n.attribute("id").and_then(|i| i.trim().parse::<u32>().ok()) else { continue };
                let name = n.attribute("name").unwrap_or("").trim().to_string();
                caps.categories.push((id, if name.is_empty() { id.to_string() } else { name.clone() }));
                for sub in n.children().filter(|c| c.has_tag_name("subcat")) {
                    let Some(sid) = sub.attribute("id").and_then(|i| i.trim().parse::<u32>().ok()) else { continue };
                    let sname = sub.attribute("name").unwrap_or("").trim();
                    let label = if sname.contains('/') || name.is_empty() {
                        sname.to_string()
                    } else {
                        format!("{name}/{sname}")
                    };
                    caps.categories.push((sid, if label.is_empty() { sid.to_string() } else { label }));
                }
            }
            _ => {}
        }
    }
    let mut seen = std::collections::HashSet::new();
    caps.categories.retain(|(id, _)| seen.insert(*id));
    Ok(caps)
}

pub fn parse_feed(body: &str) -> Result<Page, Error> {
    let doc = roxmltree::Document::parse(body).map_err(|e| Error::Indexer(format!("bad feed: {e}")))?;
    let mut page = Page::default();
    for n in doc.descendants().filter(|n| n.is_element()) {
        match n.tag_name().name() {
            "response" => page.total = n.attribute("total").and_then(|t| t.parse().ok()),
            "item" => {
                if let Some(r) = release(n) {
                    page.releases.push(r);
                }
            }
            _ => {}
        }
    }
    Ok(page)
}

fn release(item: roxmltree::Node) -> Option<Release> {
    let mut r = Release::default();
    let mut enclosure = None;
    for n in item.children().filter(|n| n.is_element()) {
        let text = || n.text().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        match n.tag_name().name() {
            "title" => r.title = text().unwrap_or_default(),
            "link" => r.link = text(),
            "size" => r.size = text().and_then(|s| s.parse().ok()),
            "enclosure" => enclosure = n.attribute("url").map(str::to_string),
            "jackettindexer" | "prowlarrindexer" | "indexer" => r.indexer = text(),
            "category" => r.categories.extend(text().and_then(|c| c.parse::<u32>().ok())),
            "attr" => {
                let (Some(name), Some(v)) = (n.attribute("name"), n.attribute("value")) else {
                    continue;
                };
                let v = v.trim();
                if v.is_empty() {
                    continue;
                }
                match name {
                    "seeders" => r.seeders = v.parse().ok(),
                    "category" => r.categories.extend(v.parse::<u32>().ok()),
                    "peers" => r.peers = v.parse().ok(),
                    "size" if r.size.is_none() => r.size = v.parse().ok(),
                    "magneturl" => r.magnet = Some(v.to_string()),
                    "infohash" => r.infohash = hex_hash(v),
                    "artist" => r.artist = Some(v.to_string()),
                    "album" => r.album = Some(v.to_string()),
                    "year" => r.year = v.get(..4).and_then(|y| y.parse().ok()),
                    "genre" => r.genre = Some(v.to_string()),
                    "coverurl" if v.starts_with("https://") || v.starts_with("http://") => {
                        r.cover = Some(v.to_string())
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // The enclosure is the download; `<link>` is often the details page.
    let usable_link = |l: &str| l.starts_with("http://") || l.starts_with("https://") || l.starts_with("magnet:");
    if let Some(e) = enclosure.filter(|e| usable_link(e)) {
        r.link = Some(e);
    } else if r.link.as_deref().is_some_and(|l| !usable_link(l)) {
        r.link = None;
    }
    if let Some(l) = r.link.as_deref().filter(|l| l.starts_with("magnet:")) {
        r.magnet.get_or_insert_with(|| l.to_string());
        r.link = None;
    }
    if r.infohash.is_none() {
        r.infohash = r.magnet.as_deref().and_then(magnet_hash);
    }
    r.categories.sort_unstable();
    r.categories.dedup();
    let usable = r.link.is_some() || r.magnet.is_some() || r.infohash.is_some();
    (usable && !r.title.is_empty()).then_some(r)
}

/// A 40-character hex info hash, lower-cased.
fn hex_hash(v: &str) -> Option<String> {
    (v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit())).then(|| v.to_ascii_lowercase())
}

/// The BTv1 info hash of a magnet link, hex or base32.
pub fn magnet_hash(magnet: &str) -> Option<String> {
    let query = magnet.strip_prefix("magnet:?")?;
    query.split('&').find_map(|p| {
        let v = p.strip_prefix("xt=urn:btih:")?;
        hex_hash(v).or_else(|| base32_hex(v))
    })
}

fn base32_hex(v: &str) -> Option<String> {
    if v.len() != 32 {
        return None;
    }
    let mut bits: u64 = 0;
    let mut n = 0;
    let mut out = String::with_capacity(40);
    for c in v.bytes() {
        let x = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(x);
        n += 5;
        if n >= 8 {
            n -= 8;
            out.push_str(&format!("{:02x}", (bits >> n) & 0xff));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel>
  <atom:link href="http://127.0.0.1:9117/api" rel="self" type="application/rss+xml" />
  <title>Indexer</title>
  <newznab:response xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/" offset="0" total="42"/>
  <item>
    <title>Glenn Gould - Goldberg Variations (1955) [FLAC]</title>
    <guid>https://example.org/t/1</guid>
    <jackettindexer id="pd">Public Domain</jackettindexer>
    <link>http://127.0.0.1:9117/dl/pd/?jackett_apikey=secret&amp;file=gould</link>
    <size>312000000</size>
    <enclosure url="http://127.0.0.1:9117/dl/pd/?jackett_apikey=secret&amp;file=gould" length="312000000" type="application/x-bittorrent" />
    <category>3000</category>
    <category>3040</category>
    <torznab:attr name="category" value="3040" />
    <torznab:attr name="seeders" value="12" />
    <torznab:attr name="peers" value="15" />
    <torznab:attr name="infohash" value="0123456789ABCDEF0123456789ABCDEF01234567" />
    <torznab:attr name="artist" value="Glenn Gould" />
    <torznab:attr name="year" value="1955" />
  </item>
  <item>
    <title>Old Jazz &amp; Blues 78s</title>
    <link>magnet:?xt=urn:btih:CIQGJZVOK2SKKMKDBAHNNNWCLLIH5RUN&amp;dn=jazz</link>
    <torznab:attr name="seeders" value="0" />
  </item>
  <item><title>No link at all</title></item>
</channel></rss>"#;

    #[test]
    fn feed() {
        let p = parse_feed(FEED).unwrap();
        assert_eq!(p.total, Some(42));
        assert_eq!(p.releases.len(), 2);
        let a = &p.releases[0];
        assert_eq!(a.title, "Glenn Gould - Goldberg Variations (1955) [FLAC]");
        assert_eq!(a.seeders, Some(12));
        assert_eq!(a.size, Some(312_000_000));
        assert_eq!(a.year, Some(1955));
        assert_eq!(a.artist.as_deref(), Some("Glenn Gould"));
        assert_eq!(a.indexer.as_deref(), Some("Public Domain"));
        assert_eq!(a.categories, [3000, 3040]);
        assert_eq!(a.infohash.as_deref(), Some("0123456789abcdef0123456789abcdef01234567"));
        assert!(a.link.as_deref().unwrap().contains("file=gould"));
        let b = &p.releases[1];
        assert_eq!(b.title, "Old Jazz & Blues 78s");
        assert!(b.link.is_none());
        assert!(b.magnet.as_deref().unwrap().starts_with("magnet:?xt="));
        assert_eq!(b.infohash.as_deref(), Some("122064e6ae56a4a53143080ed6b6c25ad07ec68d"));
    }

    #[test]
    fn caps() {
        let c = parse_caps(
            r#"<caps><limits max="100" default="50"/><searching>
               <search available="yes" supportedParams="q"/>
               <music-search available="yes" supportedParams="q,artist,album"/>
               </searching></caps>"#,
        )
        .unwrap();
        assert!(c.music_param("artist"));
        assert_eq!(c.max_limit, Some(100));
        assert_eq!(c.categories, []);
        let c = parse_caps(
            r#"<caps><categories>
               <category id="2000" name="Movies"><subcat id="2040" name="Movies/HD"/></category>
               <category id="3000" name="Audio"><subcat id="3040" name="Lossless"/><subcat id="3010" name="Audio/MP3"/></category>
               <category id="100123" name="Podcasts"/>
               <category id="100456" name="Musique - FLAC Hi-Res"/>
               <category id="100789" name="Jeux PC"/>
               <category id="100790" name="Postcards"/>
               <category id="3000" name="Audio"/>
               </categories></caps>"#,
        )
        .unwrap();
        assert_eq!(c.categories.len(), 9, "the repeated 3000 counts once");
        assert_eq!(
            c.audio_categories(),
            [
                (3000, "Audio".to_string()),
                (3040, "Audio/Lossless".to_string()),
                (3010, "Audio/MP3".to_string()),
                (100123, "Podcasts".to_string()),
                (100456, "Musique - FLAC Hi-Res".to_string()),
            ]
        );
        let c = parse_caps(r#"<caps><searching><music-search available="no"/></searching></caps>"#).unwrap();
        assert!(!c.music_param("artist"));
        assert!(parse_caps("<rss/>").is_err());
    }

    #[test]
    fn errors() {
        assert!(matches!(feed_error(r#"<error code="100" description="Incorrect user credentials"/>"#), Some(Error::Auth(_))));
        assert!(matches!(feed_error(r#"<error code="500" description="Request limit reached"/>"#), Some(Error::RateLimited)));
        assert!(feed_error(FEED).is_none());
    }

    #[test]
    fn pasted_indexer() {
        let ix = Indexer::parse("http://localhost:9696/3/api?apikey=abc&t=search").unwrap();
        assert_eq!(ix, Indexer { url: "http://localhost:9696/3/api".into(), apikey: Some("abc".into()) });
        let ix = Indexer::parse("  https://ix.example/torznab/api/  KEY  ").unwrap();
        assert_eq!(ix.url, "https://ix.example/torznab/api");
        assert_eq!(ix.apikey.as_deref(), Some("KEY"));
        let ix = Indexer::parse("http://h:3333/torznab").unwrap();
        assert_eq!(ix.apikey, None);
        assert_eq!(
            ix.request_url(&[("t", "search".into()), ("q", "a b&c".into())]),
            "http://h:3333/torznab?t=search&q=a%20b%26c"
        );
        assert!(Indexer::parse("ftp://h/api").is_none());
        assert!(Indexer::parse("").is_none());
        assert!(Indexer::parse("http://h/api a b").is_none());
    }

    #[test]
    fn from_settings() {
        let url = |u: &str| Indexer::from_settings(u, "").map(|i| i.url);
        assert_eq!(url("http://localhost:9696/1/api").as_deref(), Some("http://localhost:9696/1/api"));
        assert_eq!(url("http://localhost:9696/1/").as_deref(), Some("http://localhost:9696/1/api"));
        assert_eq!(url("http://localhost:9696/1").as_deref(), Some("http://localhost:9696/1/api"));
        assert_eq!(url("http://localhost:3333").as_deref(), Some("http://localhost:3333/api"));
        assert_eq!(
            url("http://h:9117/api/v2.0/indexers/all/results/torznab/").as_deref(),
            Some("http://h:9117/api/v2.0/indexers/all/results/torznab/api")
        );
        assert_eq!(url("http://h/x/api?apikey=a&t=caps").as_deref(), Some("http://h/x/api"));
        assert_eq!(Indexer::from_settings("http://h/x/api?apikey=a", "").unwrap().apikey.as_deref(), Some("a"));
        assert_eq!(Indexer::from_settings("http://h/x/api?apikey=a", "b").unwrap().apikey.as_deref(), Some("b"));
        assert_eq!(url(""), None);
        assert_eq!(url("localhost:9696"), None);
        assert_eq!(url("http://h/api k"), None);
    }

    #[test]
    fn download_links() {
        let ix = Indexer { url: "https://Tracker.example/api".into(), apikey: Some("k y".into()) };
        assert_eq!(ix.download_url("https://tracker.example/dl/42"), "https://tracker.example/dl/42?apikey=k%20y");
        assert_eq!(ix.download_url("https://tracker.example/api?t=get&id=42"), "https://tracker.example/api?t=get&id=42&apikey=k%20y");
        assert_eq!(ix.download_url("https://tracker.example/dl/42?apikey=own"), "https://tracker.example/dl/42?apikey=own");
        assert_eq!(ix.download_url("https://elsewhere.example/dl/42"), "https://elsewhere.example/dl/42");
        assert_eq!(ix.download_url("magnet:?xt=urn:btih:x"), "magnet:?xt=urn:btih:x");
        let open = Indexer { url: "http://h/api".into(), apikey: None };
        assert_eq!(open.download_url("http://h/dl/1"), "http://h/dl/1");
    }

    #[test]
    fn details_link_and_enclosure() {
        let feed = r#"<rss><channel><item><title>A - B</title>
            <link>https://t.example/torrents/abc</link>
            <comments>https://t.example/torrents/abc</comments>
            <enclosure url="https://t.example/api?t=get&amp;id=abc" type="application/x-bittorrent"/></item>
            <item><title>C - D</title><link>https://t.example/dl/2</link></item>
            <item><title>E - F</title><link>/relative</link><torznab:attr name="infohash" value="0123456789abcdef0123456789abcdef01234567"/></item></channel></rss>"#
            .replace("torznab:attr", "attr");
        let p = parse_feed(&feed).unwrap();
        assert_eq!(p.releases[0].link.as_deref(), Some("https://t.example/api?t=get&id=abc"));
        assert_eq!(p.releases[1].link.as_deref(), Some("https://t.example/dl/2"));
        assert_eq!(p.releases[2].link, None);
    }

    #[test]
    fn masking() {
        assert_eq!(masked("http://h/api?t=search&apikey=s3cret"), "http://h/api?t=…&apikey=…");
        assert_eq!(masked("http://h/api"), "http://h/api");
    }

    #[test]
    fn magnets() {
        assert_eq!(
            magnet_hash("magnet:?dn=x&xt=urn:btih:0123456789abcdef0123456789abcdef01234567").as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(magnet_hash("magnet:?dn=x"), None);
        assert_eq!(magnet_hash("http://x"), None);
    }
}
