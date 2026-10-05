//! Reading what a release is from text: Torznab gives a free-form title
//! (`Artist - Album (1962) [FLAC 24-96]`), the torrent gives file paths
//! (`CD1/03 - Title.flac`).

/// Artist, album and year read from a release title.
#[derive(Debug, Default, PartialEq)]
pub struct Title {
    pub artist: Option<String>,
    pub album: String,
    pub year: Option<i64>,
}

fn year_of(s: &str) -> Option<i64> {
    let y: i64 = s.trim().parse().ok()?;
    (1880..=2100).contains(&y).then_some(y)
}

/// Words that end the name part of a dotted release title.
const TAGS: &[&str] = &[
    "FLAC", "MP3", "AAC", "ALAC", "OGG", "OPUS", "WAV", "DSD", "WEB", "WEBFLAC", "CD", "2CD", "3CD", "CDDA", "VINYL",
    "LP", "SACD", "LOSSLESS", "HIRES", "HI-RES", "24BIT", "16BIT", "24B", "16B", "320", "V0", "V2", "VBR", "CBR", "DELUXE.EDITION",
];

fn is_tag(w: &str) -> bool {
    let up = w.to_ascii_uppercase();
    TAGS.contains(&up.as_str()) || (up.contains('-') && up.split('-').all(|p| p.parse::<u32>().is_ok()))
}

/// `Artist.Album.1982.FLAC-GROUP` or `Artist_Name-Album_Name-2020-GROUP`
/// (titles without spaces) as `Artist - Album (1982)`, or `Artist Album
/// (1982)` when nothing tells the artist from the album.
fn unscene(title: &str) -> Option<String> {
    let t = title.trim();
    if t.contains(' ') || t.matches(['.', '_']).count() < 2 {
        return None;
    }
    // `…-GROUP` at the end: the uploader's tag.
    let mut t = t;
    if let Some(i) = t.rfind('-') {
        let group = &t[i + 1..];
        if (1..=24).contains(&group.len()) && group.bytes().all(|b| b.is_ascii_alphanumeric()) && i > 0 {
            t = t[..i].trim_end_matches(['.', '_', '-']);
        }
    }
    let mut parts: Vec<String> = if t.contains('_') && t.contains('-') && !t.contains('.') {
        t.split('-').map(|p| p.replace('_', " ")).collect()
    } else {
        vec![t.replace(['.', '_'], " ")]
    };
    let mut year = None;
    let mut cut = |p: &str| -> String {
        let mut keep = Vec::new();
        for w in p.split(' ').filter(|w| !w.is_empty()) {
            if let Some(y) = year_of(w).filter(|_| !keep.is_empty()) {
                year.get_or_insert(y);
                break;
            }
            if is_tag(w) && !keep.is_empty() {
                break;
            }
            keep.push(w);
        }
        keep.join(" ")
    };
    // `Artist-Album-WEB-1928`: what follows the album is tags and the year.
    let tail_year = parts.iter().skip(2).find_map(|p| year_of(p));
    parts.truncate(2);
    parts = parts.iter().map(|p| cut(p)).filter(|p| !p.is_empty()).collect();
    let year = year.or(tail_year);
    let name = match parts.as_slice() {
        [] => return None,
        [one] => one.clone(),
        [artist, album, ..] => format!("{artist} - {album}"),
    };
    Some(match year {
        Some(y) => format!("{name} ({y})"),
        None => name,
    })
}

/// When the title did not say who the artist is: `hint` (the artist or
/// the words searched for) if the album starts with it. Returns the
/// artist as written in the title, and the rest.
pub fn split_artist(album: &str, hint: &str) -> Option<(String, String)> {
    let hint: Vec<String> = hint.split_whitespace().map(str::to_lowercase).collect();
    let words: Vec<&str> = album.split_whitespace().collect();
    if hint.is_empty() || words.len() <= hint.len() {
        return None;
    }
    let same = words.iter().zip(&hint).all(|(w, h)| w.to_lowercase() == *h);
    same.then(|| (words[..hint.len()].join(" "), words[hint.len()..].join(" ")))
}

/// Split `title` into artist, album and year. Bracketed tags (`[FLAC]`,
/// `(Remastered)`, `{WEB}`) are dropped, except that a bracketed year is
/// kept as the year. Titles without spaces (`Artist.Album.1982.FLAC-GROUP`)
/// are read word by word.
pub fn release_title(title: &str) -> Title {
    let unscened = unscene(title);
    let title = unscened.as_deref().unwrap_or(title);
    let mut year = None;
    let mut plain = String::with_capacity(title.len());
    let mut depth = 0usize;
    let mut inner = String::new();
    for c in title.chars() {
        match c {
            '[' | '(' | '{' => {
                depth += 1;
                inner.clear();
            }
            ']' | ')' | '}' if depth > 0 => {
                depth -= 1;
                if year.is_none() {
                    year = year_of(&inner);
                }
            }
            _ if depth > 0 => inner.push(c),
            _ => plain.push(c),
        }
    }
    let plain = plain.replace('_', " ");
    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut words: Vec<&str> = plain.split(' ').collect();
    // A bare year at the end: `Artist - Album 1962`.
    if year.is_none()
        && words.len() > 1
        && let Some(y) = words.last().and_then(|w| year_of(w))
    {
        year = Some(y);
        words.pop();
    }
    let plain = words.join(" ");
    let plain = plain.trim_matches(|c: char| c == '-' || c == '.' || c.is_whitespace());
    let (artist, album) = match plain.split_once(" - ") {
        Some((a, b)) if !a.trim().is_empty() && !b.trim().is_empty() => {
            (Some(a.trim().to_string()), b.trim().trim_end_matches(" -").trim().to_string())
        }
        _ => (None, plain.to_string()),
    };
    let album = if album.is_empty() { title.trim().to_string() } else { album };
    Title { artist, album, year }
}

/// The audio format a release title announces, for the subtitle: `FLAC`,
/// `FLAC 24-bit`, `MP3`…
pub fn format_hint(title: &str) -> Option<String> {
    let up = title.to_ascii_uppercase();
    let has = |w: &str| {
        up.match_indices(w).any(|(i, _)| {
            let before = up[..i].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric());
            let after = up[i + w.len()..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric());
            before && after
        })
    };
    let codec = ["FLAC", "ALAC", "MP3", "OPUS", "OGG", "AAC", "WAV", "DSD", "APE", "WV"]
        .into_iter()
        .find(|c| has(c))?;
    let hires = has("24BIT") || has("24-BIT") || has("24 BIT") || up.contains("24-96") || up.contains("24-192") || up.contains("24-48") || up.contains("24-88");
    Some(if hires { format!("{codec} 24-bit") } else { codec.to_string() })
}

/// Audio file extensions the engine plays, lower-case.
const AUDIO: &[&str] = &["flac", "mp3", "ogg", "oga", "opus", "m4a", "aac", "wav", "aif", "aiff", "wv", "ape"];
const IMAGE: &[&str] = &["jpg", "jpeg", "png", "webp"];

fn ext(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let (stem, e) = name.rsplit_once('.')?;
    (!stem.is_empty()).then(|| e.to_ascii_lowercase())
}

pub fn is_audio(path: &str) -> bool {
    ext(path).is_some_and(|e| AUDIO.contains(&e.as_str()))
}

/// The codec name the host knows, from the extension.
pub fn codec(path: &str) -> Option<&'static str> {
    Some(match ext(path)?.as_str() {
        "flac" => "flac",
        "mp3" => "mp3",
        "ogg" | "oga" => "vorbis",
        "opus" => "opus",
        "m4a" | "aac" => "aac",
        "wav" => "wav",
        "aif" | "aiff" => "aiff",
        "wv" => "wavpack",
        "ape" => "ape",
        _ => return None,
    })
}

pub fn mime(path: &str) -> &'static str {
    match ext(path).as_deref() {
        Some("flac") => "audio/flac",
        Some("mp3") => "audio/mpeg",
        Some("ogg" | "oga") => "audio/ogg",
        Some("opus") => "audio/opus",
        Some("m4a" | "aac") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("aif" | "aiff") => "audio/aiff",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Which image of the torrent is the cover: `cover`, `folder`, `front`
/// first, then any image, the shallowest first. `files` are
/// `(index, path, len)`.
pub fn cover(files: &[(usize, String, u64)]) -> Option<usize> {
    files
        .iter()
        .filter(|(_, p, len)| ext(p).is_some_and(|e| IMAGE.contains(&e.as_str())) && *len <= 20 << 20)
        .min_by_key(|(_, p, _)| {
            let name = p.rsplit('/').next().unwrap_or(p).to_ascii_lowercase();
            let rank = ["cover", "folder", "front"]
                .iter()
                .position(|w| name.starts_with(w))
                .unwrap_or(3);
            (rank, p.matches('/').count(), p.clone())
        })
        .map(|(i, _, _)| *i)
}

/// A track read from its path in the torrent.
#[derive(Debug, PartialEq)]
pub struct TrackName {
    pub title: String,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
}

/// Leading digits of `s` and the rest.
fn leading_number(s: &str) -> Option<(u32, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 || end > 3 {
        return None;
    }
    Some((s[..end].parse().ok()?, &s[end..]))
}

/// `CD1`, `CD 2`, `Disc 3`, `Disk-1`: the disc number of a folder.
fn disc_of(dir: &str) -> Option<u32> {
    let low = dir.to_ascii_lowercase();
    let rest = ["cd", "disc", "disk"]
        .iter()
        .find_map(|p| low.strip_prefix(p))?
        .trim_start_matches([' ', '-', '_', '.']);
    let (n, tail) = leading_number(rest)?;
    (tail.is_empty() || tail.starts_with([' ', '-', '_', '.', '('])).then_some(n)
}

/// Title, track and disc numbers from `path`, dropping a leading
/// `artist - ` when it repeats the release's artist.
pub fn track_name(path: &str, artist: Option<&str>) -> TrackName {
    let mut parts: Vec<&str> = path.split('/').collect();
    let file = parts.pop().unwrap_or(path);
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let mut disc_no = parts.iter().rev().find_map(|d| disc_of(d));
    let mut track_no = None;
    let mut rest = stem.trim();
    if let Some((n, tail)) = leading_number(rest) {
        // `1-03 Title`: disc 1, track 3.
        if let Some(t) = tail.strip_prefix('-')
            && let Some((m, tail2)) = leading_number(t)
            && disc_no.is_none()
            && !tail2.starts_with(|c: char| c.is_ascii_alphanumeric())
        {
            disc_no = Some(n);
            track_no = Some(m);
            rest = tail2;
        } else if tail.is_empty() || tail.starts_with([' ', '.', '-', '_', ')']) {
            track_no = Some(n);
            rest = tail;
        }
    }
    let mut title = rest
        .trim_start_matches([' ', '.', '-', '_', ')'])
        .replace('_', " ")
        .trim()
        .to_string();
    if let Some(a) = artist
        && let Some((head, tail)) = title.split_once(" - ")
        && head.trim().eq_ignore_ascii_case(a.trim())
        && !tail.trim().is_empty()
    {
        title = tail.trim().to_string();
    }
    if title.is_empty() {
        title = stem.to_string();
    }
    TrackName { title, track_no, disc_no }
}

/// Order of tracks: by disc, then track number, then path (with numbers
/// compared as numbers).
pub fn natural_key(path: &str) -> Vec<(u8, u64, String)> {
    let mut out = Vec::new();
    let mut chars = path.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut n = 0u64;
            while let Some(&d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                n = n.saturating_mul(10).saturating_add(u64::from(d as u8 - b'0'));
                chars.next();
            }
            out.push((0, n, String::new()));
        } else {
            let mut s = String::new();
            while let Some(&d) = chars.peek().filter(|d| !d.is_ascii_digit()) {
                s.extend(d.to_lowercase());
                chars.next();
            }
            out.push((1, 0, s));
        }
    }
    out
}

/// `1.2 GB`, `640 MB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut v = bytes as f64 / 1000.0;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if v < 10.0 { format!("{v:.1} {}", UNITS[u]) } else { format!("{v:.0} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(
            release_title("Glenn Gould - Goldberg Variations (1955) [FLAC]"),
            Title { artist: Some("Glenn Gould".into()), album: "Goldberg Variations".into(), year: Some(1955) }
        );
        assert_eq!(
            release_title("Bessie_Smith_-_Empty_Bed_Blues_1928"),
            Title { artist: Some("Bessie Smith".into()), album: "Empty Bed Blues".into(), year: Some(1928) }
        );
        assert_eq!(
            release_title("Old Jazz 78s [MP3 320]"),
            Title { artist: None, album: "Old Jazz 78s".into(), year: None }
        );
        assert_eq!(release_title("[FLAC]").album, "[FLAC]");
        assert_eq!(
            release_title("Glenn.Gould.Goldberg.Variations.1955.FLAC.-Cross173"),
            Title { artist: None, album: "Glenn Gould Goldberg Variations".into(), year: Some(1955) }
        );
        assert_eq!(
            release_title("Bessie_Smith-Empty_Bed_Blues-WEB-1928-PDGROUP"),
            Title { artist: Some("Bessie Smith".into()), album: "Empty Bed Blues".into(), year: Some(1928) }
        );
        assert_eq!(
            release_title("Kevin.MacLeod.Free.Classics.24-96.FLAC"),
            Title { artist: None, album: "Kevin MacLeod Free Classics".into(), year: None }
        );
        assert_eq!(release_title("1984.Live.FLAC").album, "1984 Live");
        assert_eq!(release_title("a.b").album, "a.b");
        assert_eq!(release_title("Artist - 1999").year, Some(1999));
    }

    #[test]
    fn artist_from_hint() {
        assert_eq!(
            split_artist("Glenn Gould Goldberg Variations", "glenn gould"),
            Some(("Glenn Gould".into(), "Goldberg Variations".into()))
        );
        assert_eq!(split_artist("Glenn Gould Goldberg Variations", "gould"), None);
        assert_eq!(split_artist("Glenn Gould", "glenn gould"), None);
        assert_eq!(split_artist("X", ""), None);
    }

    #[test]
    fn hints() {
        assert_eq!(format_hint("X - Y [FLAC 24-96]").as_deref(), Some("FLAC 24-bit"));
        assert_eq!(format_hint("X - Y (2001) MP3 V0").as_deref(), Some("MP3"));
        assert_eq!(format_hint("Flacon - Live"), None);
    }

    #[test]
    fn tracks() {
        assert_eq!(
            track_name("Album/03 - Aria.flac", None),
            TrackName { title: "Aria".into(), track_no: Some(3), disc_no: None }
        );
        assert_eq!(
            track_name("Album/CD2/01. Glenn Gould - Variatio 1.flac", Some("Glenn Gould")),
            TrackName { title: "Variatio 1".into(), track_no: Some(1), disc_no: Some(2) }
        );
        assert_eq!(
            track_name("1-07 Blues.mp3", None),
            TrackName { title: "Blues".into(), track_no: Some(7), disc_no: Some(1) }
        );
        assert_eq!(
            track_name("1999 Remix.mp3", None),
            TrackName { title: "1999 Remix".into(), track_no: None, disc_no: None }
        );
        assert_eq!(track_name("07.flac", None).title, "07");
    }

    #[test]
    fn files() {
        assert!(is_audio("a/b/01 x.FLAC"));
        assert!(!is_audio("a/cover.jpg"));
        assert!(!is_audio("a/.flac"));
        assert_eq!(codec("x.ogg"), Some("vorbis"));
        let files = vec![
            (0, "A/scans/back.jpg".to_string(), 100),
            (1, "A/01.flac".to_string(), 100),
            (2, "A/scans/Front.png".to_string(), 100),
            (3, "A/Folder.jpg".to_string(), 100),
        ];
        assert_eq!(cover(&files), Some(3));
        assert_eq!(cover(&files[..3]), Some(2));
        assert_eq!(cover(&files[1..2]), None);
    }

    #[test]
    fn ordering() {
        let mut v = vec!["a/10 x.flac", "a/2 x.flac", "a/1 x.flac"];
        v.sort_by_key(|p| natural_key(p));
        assert_eq!(v, ["a/1 x.flac", "a/2 x.flac", "a/10 x.flac"]);
    }

    #[test]
    fn sizes() {
        assert_eq!(size(312_000_000), "312 MB");
        assert_eq!(size(1_250_000_000), "1.2 GB");
    }
}
