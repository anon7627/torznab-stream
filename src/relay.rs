//! Local relay: serves file `<index>` of release `<id>` on
//! `http://127.0.0.1:<port>/f/<id>/<index>/<name>`, straight from the
//! torrent, bytes unchanged.
//!
//! - `Content-Length` is the file's size from the torrent's metadata.
//! - `Range: bytes=a-b` is honoured (`206`): librqbit fetches the pieces
//!   at the requested offset first, so a seek far ahead does not wait for
//!   the whole file.
//! - `HEAD` answers from the metadata, without starting the download.
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
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

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
        let stream = async {
            let h = engine.open(&id, index).await?;
            let mut s = h.stream(index).await?;
            s.seek(SeekFrom::Start(start)).await?;
            anyhow::Ok(s)
        };
        match stream.await {
            Ok(s) => Body::from_stream(ReaderStream::with_capacity(s.take(length), 64 * 1024)),
            Err(e) => {
                eprintln!("relay: {id}/{index}: {e:#}");
                return (StatusCode::SERVICE_UNAVAILABLE, "torrent unavailable").into_response();
            }
        }
    };
    builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
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
