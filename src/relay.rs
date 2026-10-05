//! Local relay: serves file `<index>` of release `<id>` on
//! `http://127.0.0.1:<port>/f/<id>/<index>/<name>`, straight from the
//! torrent, bytes unchanged.
//!
//! - `Content-Length` is the file's size from the torrent's metadata.
//! - `Range: bytes=a-b` is honoured (`206`): librqbit fetches the pieces
//!   at the requested offset first, so a seek far ahead does not wait for
//!   the whole file.
//! - `HEAD` answers from the metadata, without starting the download.
//! - A `GET` answers once its first bytes are there, so that the player
//!   shows a loading state for as long as nothing comes, and gets `503`
//!   when the swarm sends nothing in time (before its own read timeout).
//! - `/cover?artist=…&album=…` redirects to the album's front cover on
//!   Cover Art Archive (see `covers`), or answers `404`.
//! - The port is kept in `<data_dir>/relay-port` and reused, so that cover
//!   URLs handed to the host stay valid across restarts.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use std::time::Duration;

use axum::body::Bytes;
use futures_util::{StreamExt, stream};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::covers::Covers;
use crate::engine::Engine;
use crate::names;

pub struct Relay {
    port: u16,
}

#[derive(Clone)]
struct Shared {
    engine: Arc<Engine>,
    covers: Option<Arc<Covers>>,
}

impl Relay {
    pub async fn start(engine: Arc<Engine>, covers: Option<Arc<Covers>>, data_dir: &Path) -> Result<Relay> {
        let saved = data_dir.join("relay-port");
        let wanted: u16 = std::fs::read_to_string(&saved).ok().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
        let listener = match tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, wanted))).await {
            Ok(l) => l,
            Err(_) => tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.context("relay: bind")?,
        };
        let port = listener.local_addr()?.port();
        if port != wanted {
            let _ = std::fs::write(&saved, port.to_string());
        }
        let app = Router::new()
            .route("/f/{id}/{index}/{name}", get(serve).head(serve))
            .route("/cover", get(cover))
            .with_state(Shared { engine, covers });
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("relay stopped: {e}");
            }
        });
        Ok(Relay { port })
    }

    /// Where the host gets the cover of `artist`'s `album`.
    pub fn cover_url(&self, artist: &str, album: &str) -> String {
        format!(
            "http://127.0.0.1:{}/cover?artist={}&album={}",
            self.port,
            urlencoding::encode(artist.trim()),
            urlencoding::encode(album.trim())
        )
    }

    pub fn url(&self, id: &str, index: usize, path: &str) -> String {
        let name = path.rsplit('/').next().unwrap_or("file");
        format!("http://127.0.0.1:{}/f/{id}/{index}/{}", self.port, urlencoding::encode(name))
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` against a file of `size` bytes:
/// the inclusive range, `Err` when it cannot be satisfied, `None` for the
/// whole file (no header, or one that is not a single byte range).
fn byte_range(headers: &HeaderMap, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = headers.get(header::RANGE)?.to_str().ok()?.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let range = match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
        (Some(a), Some(b)) if a <= b => (a, b.min(size.saturating_sub(1))),
        (Some(a), None) if b.is_empty() => (a, size.saturating_sub(1)),
        (None, Some(n)) if a.is_empty() && n > 0 => (size.saturating_sub(n), size.saturating_sub(1)),
        _ => return None,
    };
    Some(if size == 0 || range.0 >= size || range.0 > range.1 { Err(()) } else { Ok(range) })
}

async fn cover(State(shared): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let (Some(covers), Some(artist), Some(album)) = (&shared.covers, q.get("artist"), q.get("album")) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if artist.trim().is_empty() || album.trim().is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match covers.front(artist, album).await {
        Some(url) => (StatusCode::FOUND, [(header::LOCATION, url), (header::CONTENT_LENGTH, "0".into())]).into_response(),
        None => (StatusCode::NOT_FOUND, "no cover").into_response(),
    }
}

async fn serve(
    State(Shared { engine, .. }): State<Shared>,
    UrlPath((id, index, _name)): UrlPath<(String, usize, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let Some(meta) = engine.known(&id) else {
        return (StatusCode::NOT_FOUND, "unknown release").into_response();
    };
    let Some(file) = meta.file(index).cloned() else {
        return (StatusCode::NOT_FOUND, "no such file").into_response();
    };
    let size = file.len;
    let (status, start, end) = match byte_range(&headers, size) {
        None => (StatusCode::OK, 0, size.saturating_sub(1)),
        Some(Ok((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Some(Err(())) => {
            return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{size}"))]).into_response();
        }
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, names::mime(&file.path))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        // Opening the torrent counts in the wait too: it can take long
        // (initialising after a restart, another file being added).
        let first = async {
            let r = reader_at(&engine, &id, index, start).await?;
            let serving = Serving::new(&engine);
            let read = Reading { engine: engine.clone(), id: id.clone(), index, pos: start, end: end + 1, reader: Some(r), failures: 0, _serving: serving };
            anyhow::Ok(next_chunk(read).await)
        };
        match tokio::time::timeout(FIRST_BYTES_WAIT, first).await {
            Ok(Ok(Some((Ok(first), rest)))) => {
                Body::from_stream(stream::once(async move { Ok(first) }).chain(stream::unfold(rest, next_chunk)))
            }
            Ok(Ok(Some((Err(e), _)))) => {
                eprintln!("relay: {id}/{index}: {e}");
                return (StatusCode::SERVICE_UNAVAILABLE, "torrent unavailable").into_response();
            }
            Ok(Err(e)) => {
                eprintln!("relay: {id}/{index}: {e:#}");
                return (StatusCode::SERVICE_UNAVAILABLE, "torrent unavailable").into_response();
            }
            Ok(Ok(None)) => Body::empty(),
            Err(_) => {
                let peers = engine.live_peers(&id).map_or("unknown".to_string(), |n| n.to_string());
                eprintln!(
                    "relay: {id}/{index}: nothing from the swarm in {} s at byte {start} ({peers} peers connected)",
                    FIRST_BYTES_WAIT.as_secs()
                );
                return (StatusCode::SERVICE_UNAVAILABLE, "no data from the swarm").into_response();
            }
        }
    };
    builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// librqbit's file stream (not a public type), positioned.
type FileStream = std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>;

async fn reader_at(engine: &Engine, id: &str, index: usize, pos: u64) -> anyhow::Result<FileStream> {
    let h = engine.open(id, index).await?;
    let mut s = h.stream(index).await?;
    s.seek(SeekFrom::Start(pos)).await?;
    Ok(Box::pin(s))
}

/// How long a response waits for its first bytes: less than the player's
/// read timeout (30 s), so that it gets a clear error instead.
const FIRST_BYTES_WAIT: Duration = Duration::from_secs(25);
/// Failed reads in a row before the relay gives up on a response.
const MAX_FAILURES: u32 = 8;
const CHUNK: u64 = 64 * 1024;

/// A response being served: the file from `pos` to `end` (exclusive).
struct Reading {
    engine: Arc<Engine>,
    id: String,
    index: usize,
    pos: u64,
    end: u64,
    reader: Option<FileStream>,
    failures: u32,
    _serving: Serving,
}

/// Counts the responses being served while it lives: background reads
/// (the albums' FLAC headers) wait for none to be, so as not to share the
/// swarm's bandwidth with what plays.
struct Serving(Arc<Engine>);

impl Serving {
    fn new(engine: &Arc<Engine>) -> Serving {
        engine.serving.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Serving(engine.clone())
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.0.serving.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The next bytes of a response. A read that fails (the torrent changing
/// state under it, an error librqbit recovers from) opens the file again
/// at the same offset instead of ending the response early: a body cut
/// short would read as a truncated file to the player.
async fn next_chunk(mut r: Reading) -> Option<(std::io::Result<Bytes>, Reading)> {
    if r.pos >= r.end {
        return None;
    }
    loop {
        let reader = match r.reader.as_mut() {
            Some(reader) => Ok(reader),
            None => match reader_at(&r.engine, &r.id, r.index, r.pos).await {
                Ok(reader) => Ok(r.reader.insert(reader)),
                Err(e) => Err(format!("{e:#}")),
            },
        };
        let read = match reader {
            Ok(reader) => {
                let mut buf = vec![0u8; CHUNK.min(r.end - r.pos) as usize];
                match reader.read(&mut buf).await {
                    Ok(0) => Err("the file ended early".to_string()),
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(buf)
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
            Err(e) => Err(e),
        };
        match read {
            Ok(buf) => {
                r.pos += buf.len() as u64;
                r.failures = 0;
                return Some((Ok(Bytes::from(buf)), r));
            }
            Err(e) => {
                r.reader = None;
                r.failures += 1;
                eprintln!("relay: {}/{} at byte {}: {e} (try {} of {MAX_FAILURES})", r.id, r.index, r.pos, r.failures);
                if r.failures >= MAX_FAILURES {
                    r.pos = r.end;
                    return Some((Err(std::io::Error::other(e)), r));
                }
                tokio::time::sleep(Duration::from_millis(500 * u64::from(r.failures)).min(Duration::from_secs(3))).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(h: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, h.parse().unwrap());
        byte_range(&headers, size)
    }

    #[test]
    fn ranges() {
        assert_eq!(byte_range(&HeaderMap::new(), 100), None);
        assert_eq!(range("bytes=0-9", 100), Some(Ok((0, 9))));
        assert_eq!(range("bytes=90-", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=90-500", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(range("bytes=100-", 100), Some(Err(())));
        assert_eq!(range("bytes=0-1,5-6", 100), None, "several ranges: the whole file");
    }
}
