//! The BitTorrent side, on librqbit.
//!
//! - A release's metadata (its file list) comes from the `.torrent` the
//!   indexer links to, or from the swarm for a magnet. It is kept in
//!   `<data_dir>/torrents/<id>.torrent`, so a release opened once lists and
//!   plays without the indexer.
//! - Playing a track adds its torrent with only that file selected; the
//!   next tracks are added to the selection as they are resolved. librqbit
//!   fetches the pieces a reader waits for first, so the relay can serve
//!   any offset of a file that is still downloading.
//! - Downloaded files live in `<cache_dir>/data/<id>/`, trimmed to the
//!   cache limit, least recently played first.

use std::collections::{HashMap, HashSet};
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use librqbit::api::TorrentIdOrHash;
use librqbit::dht::DhtPersistenceConfig;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, DhtSessionConfig, ListenerOptions, ManagedTorrent,
    ManagedTorrentState, Session, SessionOptions,
};
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;

use crate::catalog;
use crate::torznab::{Release, masked};

/// Largest `.torrent` accepted from an indexer.
const MAX_TORRENT: usize = 16 << 20;
/// How long to look for a magnet's metadata in the swarm.
const SWARM_WAIT: Duration = Duration::from_secs(90);
/// A torrent played this recently is never evicted.
const IN_USE: Duration = Duration::from_secs(15 * 60);

pub type Handle = Arc<ManagedTorrent>;

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub index: usize,
    /// Path inside the torrent, `/`-separated.
    pub path: String,
    pub len: u64,
}

pub struct Meta {
    pub name: String,
    pub files: Vec<FileEntry>,
    bytes: Vec<u8>,
}

impl Meta {
    pub fn parse(bytes: Vec<u8>) -> Result<Meta> {
        let t = librqbit::torrent_from_bytes(&bytes).map_err(|e| anyhow!("not a torrent file: {e}"))?;
        let info = t.info.data.validate().map_err(|e| anyhow!("invalid torrent: {e}"))?;
        let name = info.name().map(|n| n.into_owned()).unwrap_or_default();
        let mut files = Vec::new();
        for (index, fd) in info.iter_file_details().enumerate() {
            if fd.attrs().padding {
                continue;
            }
            let mut path = fd.filename.to_vec().join("/");
            if path.is_empty() {
                path.clone_from(&name);
            }
            files.push(FileEntry { index, path, len: fd.len });
        }
        drop(info);
        Ok(Meta { name, files, bytes })
    }

    pub fn file(&self, index: usize) -> Option<&FileEntry> {
        self.files.iter().find(|f| f.index == index)
    }

    #[cfg(test)]
    pub fn for_tests(name: &str, files: Vec<FileEntry>) -> Meta {
        Meta { name: name.to_string(), files, bytes: Vec::new() }
    }
}

/// Where a torrent comes from.
enum Source {
    Bytes(Vec<u8>),
    Magnet(String),
}

pub struct Options {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub cache_limit: u64,
    pub share: bool,
    pub upload_kbps: u32,
    pub download_kbps: u32,
}

pub struct Engine {
    session: Arc<Session>,
    torrents: PathBuf,
    data: PathBuf,
    cache_limit: u64,
    http: reqwest::Client,
    metas: Mutex<HashMap<String, Arc<OnceCell<Arc<Meta>>>>>,
    handles: Mutex<HashMap<String, Handle>>,
    used: Mutex<HashMap<String, Instant>>,
    adding: tokio::sync::Mutex<()>,
    /// Relay responses being served.
    pub serving: std::sync::atomic::AtomicUsize,
    /// When each torrent was started, to tell a swarm still being joined
    /// from an empty one.
    started: Mutex<HashMap<String, Instant>>,
}

impl Engine {
    pub async fn start(o: Options) -> Result<Engine> {
        let torrents = o.data_dir.join("torrents");
        let data = o.cache_dir.join("data");
        std::fs::create_dir_all(&torrents).context("creating the torrents directory")?;
        std::fs::create_dir_all(&data).context("creating the download directory")?;
        let bps = |kbps: u32| std::num::NonZeroU32::new(kbps.saturating_mul(1024));
        let (upload_bps, download_bps) = (bps(o.upload_kbps), bps(o.download_kbps));
        let opts = SessionOptions {
            dht: Some(DhtSessionConfig {
                bootstrap_addrs: None,
                port: None,
                persistence: Some(DhtPersistenceConfig {
                    dump_interval: None,
                    config_filename: Some(o.cache_dir.join("dht.json")),
                }),
            }),
            fastresume: false,
            persistence: None,
            listen: Some(ListenerOptions { listen_addr: (Ipv6Addr::UNSPECIFIED, 0).into(), ..Default::default() }),
            ratelimits: LimitsConfig { upload_bps, download_bps },
            disable_upload: !o.share,
            client_name_and_version: Some(concat!("torznab-stream/", env!("CARGO_PKG_VERSION")).to_string()),
            ..Default::default()
        };
        let session = Session::new_with_opts(data.clone(), opts).await.context("starting the BitTorrent session")?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("torznab-stream/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Engine {
            session,
            torrents,
            data,
            cache_limit: o.cache_limit,
            http,
            metas: Mutex::default(),
            handles: Mutex::default(),
            used: Mutex::default(),
            adding: tokio::sync::Mutex::new(()),
            serving: std::sync::atomic::AtomicUsize::new(0),
            started: Mutex::default(),
        })
    }

    pub async fn stop(&self) {
        self.session.stop().await;
    }

    fn torrent_file(&self, id: &str) -> PathBuf {
        self.torrents.join(format!("{id}.torrent"))
    }

    /// The metadata of release `id`, from memory or disk only.
    pub fn known(&self, id: &str) -> Option<Arc<Meta>> {
        if !catalog::valid_id(id) {
            return None;
        }
        let cell = self.metas.lock().unwrap().get(id).cloned();
        if let Some(m) = cell.and_then(|c| c.get().cloned()) {
            return Some(m);
        }
        let bytes = std::fs::read(self.torrent_file(id)).ok()?;
        let meta = Arc::new(Meta::parse(bytes).ok()?);
        let cell = Arc::new(OnceCell::new_with(Some(meta.clone())));
        self.metas.lock().unwrap().insert(id.to_string(), cell);
        Some(meta)
    }

    /// The metadata of release `id`, fetched when needed. It may take long
    /// for a magnet: the fetch goes on in the background when `wait` runs
    /// out, and a later call picks its result up.
    pub async fn meta(self: &Arc<Self>, id: &str, release: Option<Release>, wait: Duration) -> Result<Arc<Meta>> {
        if let Some(m) = self.known(id) {
            return Ok(m);
        }
        let cell = self.metas.lock().unwrap().entry(id.to_string()).or_default().clone();
        let fetch = {
            let me = self.clone();
            let id = id.to_string();
            move || async move {
                let r = me.fetch(&id, release).await;
                if let Err(e) = &r {
                    eprintln!("{id}: {e:#}");
                    // Let the next call try again.
                    me.metas.lock().unwrap().remove(&id);
                }
                r
            }
        };
        let background = {
            let cell = cell.clone();
            tokio::spawn(async move { cell.get_or_try_init(fetch).await.cloned() })
        };
        match tokio::time::timeout(wait, background).await {
            Ok(joined) => joined.map_err(|e| anyhow!("{e}"))?,
            Err(_) => bail!(Pending),
        }
    }

    async fn fetch(&self, id: &str, release: Option<Release>) -> Result<Arc<Meta>> {
        let r = release.ok_or_else(|| anyhow!("unknown release, search for it again"))?;
        let mut link_error = None;
        let source = match (&r.link, &r.magnet, &r.infohash) {
            (Some(link), _, _) => match self.download(link).await {
                Ok(s) => s,
                // The link may be gone while the swarm is not.
                Err(e) if r.magnet.is_some() || r.infohash.is_some() => {
                    eprintln!("{id}: {e:#}; trying the magnet");
                    link_error = Some(e);
                    Source::Magnet(magnet_of(&r).unwrap_or_default())
                }
                Err(e) => return Err(e),
            },
            _ => Source::Magnet(magnet_of(&r).ok_or_else(|| anyhow!("no link, magnet nor hash"))?),
        };
        let bytes = match source {
            Source::Bytes(b) => b,
            // When the swarm does not answer either, the link's failure is
            // what the user can act on.
            Source::Magnet(m) => match (self.swarm_metadata(&m).await, link_error) {
                (Ok(b), _) => b,
                (Err(e), Some(link)) if e.downcast_ref::<NoPeers>().is_some() => return Err(link),
                (Err(e), _) => return Err(e),
            },
        };
        let meta = Meta::parse(bytes)?;
        let path = self.torrent_file(id);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &meta.bytes).and_then(|()| std::fs::rename(&tmp, &path)).context("saving the torrent")?;
        Ok(Arc::new(meta))
    }

    /// GET `url`, following redirects by hand since an indexer's download
    /// link may end on a magnet.
    async fn download(&self, url: &str) -> Result<Source> {
        let mut url = reqwest::Url::parse(url).context("bad torrent link")?;
        for _ in 0..6 {
            let resp = self.http.get(url.clone()).send().await.map_err(|e| anyhow!("{}: {}", masked(url.as_str()), e.without_url()))?;
            let status = resp.status();
            if status.is_redirection() {
                let to = resp.headers().get(reqwest::header::LOCATION).and_then(|l| l.to_str().ok()).unwrap_or("");
                if to.starts_with("magnet:") {
                    return Ok(Source::Magnet(to.to_string()));
                }
                let next = url.join(to).context("bad redirect")?;
                if url.scheme() == "https" && next.scheme() != "https" {
                    bail!("refusing a redirect from https to {}", next.scheme());
                }
                url = next;
                continue;
            }
            if !status.is_success() {
                bail!("HTTP {} for {}", status.as_u16(), masked(url.as_str()));
            }
            if resp.content_length().is_some_and(|l| l as usize > MAX_TORRENT) {
                bail!("the torrent file is too large");
            }
            let body = resp.bytes().await.map_err(|e| anyhow!("{}", e.without_url()))?;
            if body.len() > MAX_TORRENT {
                bail!("the torrent file is too large");
            }
            if let Some(m) = std::str::from_utf8(&body).ok().map(str::trim).filter(|t| t.starts_with("magnet:")) {
                return Ok(Source::Magnet(m.to_string()));
            }
            if body.first() != Some(&b'd') {
                let page = body.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<');
                if page {
                    bail!(WebPage(masked(url.as_str())));
                }
                bail!("{} did not return a torrent file", masked(url.as_str()));
            }
            return Ok(Source::Bytes(body.to_vec()));
        }
        bail!("too many redirects")
    }

    async fn swarm_metadata(&self, magnet: &str) -> Result<Vec<u8>> {
        let opts = AddTorrentOptions { list_only: true, ..Default::default() };
        let added = tokio::time::timeout(SWARM_WAIT, self.session.add_torrent(AddTorrent::from_url(magnet), Some(opts)))
            .await
            .map_err(|_| anyhow!(NoPeers))??;
        match added {
            AddTorrentResponse::ListOnly(r) => Ok(r.torrent_bytes.to_vec()),
            _ => bail!("unexpected answer to a metadata request"),
        }
    }

    /// The torrent of release `id`, running, with file `index` selected.
    pub async fn open(&self, id: &str, index: usize) -> Result<Handle> {
        let meta = self.known(id).ok_or_else(|| anyhow!("{id}: no metadata"))?;
        meta.file(index).ok_or_else(|| anyhow!("{id}: no file {index}"))?;
        let _adding = self.adding.lock().await;
        let existing = self.handles.lock().unwrap().get(id).cloned();
        let (h, fresh) = match existing {
            Some(h) => (h, false),
            None => {
                let dir = self.data.join(id);
                let opts = AddTorrentOptions {
                    only_files: Some(vec![index]),
                    output_folder: Some(dir.to_string_lossy().into_owned()),
                    overwrite: true,
                    ..Default::default()
                };
                let added = self.session.add_torrent(AddTorrent::from_bytes(meta.bytes.clone()), Some(opts)).await?;
                let h = added.into_handle().ok_or_else(|| anyhow!("the torrent was not added"))?;
                self.handles.lock().unwrap().insert(id.to_string(), h.clone());
                self.started.lock().unwrap().insert(id.to_string(), Instant::now());
                (h, true)
            }
        };
        self.restart_if_failed(id, &h).await?;
        h.wait_until_initialized().await?;
        let mut files: HashSet<usize> = h.only_files().unwrap_or_default().into_iter().collect();
        if files.insert(index) {
            self.session.update_only_files(&h, &files).await?;
        }
        if h.is_paused() {
            self.session.unpause(&h).await?;
        }
        self.used.lock().unwrap().insert(id.to_string(), Instant::now());
        touch(&self.data.join(id));
        drop(_adding);
        if fresh {
            self.evict();
        }
        Ok(h)
    }

    /// A torrent in librqbit's error state (a storage error, most often)
    /// serves nothing until started again.
    async fn restart_if_failed(&self, id: &str, h: &Handle) -> Result<()> {
        let failed = h.with_state(|s| match s {
            ManagedTorrentState::Error(e) => Some(format!("{e:#}")),
            _ => None,
        });
        if let Some(e) = failed {
            eprintln!("{id}: the torrent stopped on an error ({e}); starting it again");
            self.session.unpause(h).await?;
        }
        Ok(())
    }

    /// Whether the relay is serving something now.
    pub fn busy(&self) -> bool {
        self.serving.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    /// While something plays, log every 15 s how each running torrent
    /// fares: peers and download speed say whether the swarm keeps up.
    pub fn log_swarms(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let Some(me) = me.upgrade() else { return };
                if !me.busy() {
                    continue;
                }
                let handles = me.handles.lock().unwrap().clone();
                for (id, h) in handles {
                    let Ok(v) = serde_json::to_value(h.stats()) else { continue };
                    let live = &v["live"];
                    if live.is_null() {
                        continue;
                    }
                    let peers = &live["snapshot"]["peer_stats"];
                    eprintln!(
                        "swarm {}: {} peers connected ({} known), {:.2} MiB/s down, {:.2} MiB/s up, {} of {} MB",
                        &id[..8.min(id.len())],
                        peers["live"].as_u64().unwrap_or(0),
                        peers["seen"].as_u64().unwrap_or(0),
                        live["download_speed"]["mbps"].as_f64().unwrap_or(0.0),
                        live["upload_speed"]["mbps"].as_f64().unwrap_or(0.0),
                        v["progress_bytes"].as_u64().unwrap_or(0) / 1_000_000,
                        v["total_bytes"].as_u64().unwrap_or(0) / 1_000_000,
                    );
                }
            }
        });
    }

    /// How the swarm of release `id` looks: peers connected, peers heard
    /// of, and how long the torrent has run.
    pub fn swarm(&self, id: &str) -> Option<(u64, u64, Duration)> {
        let age = self.started.lock().unwrap().get(id)?.elapsed();
        let v = serde_json::to_value(self.running(id)?.stats()).ok()?;
        let peers = &v["live"]["snapshot"]["peer_stats"];
        Some((peers["live"].as_u64()?, peers["seen"].as_u64().unwrap_or(0), age))
    }

    /// Peers connected to the torrent of release `id`, when it runs.
    pub fn live_peers(&self, id: &str) -> Option<u64> {
        let stats = serde_json::to_value(self.running(id)?.stats()).ok()?;
        stats["live"]["snapshot"]["peer_stats"]["live"].as_u64()
    }

    /// The running torrent of release `id`, if it was opened.
    pub fn running(&self, id: &str) -> Option<Handle> {
        self.handles.lock().unwrap().get(id).cloned()
    }

    /// The first bytes of file `index` of a running torrent, without
    /// selecting the whole file: only the pieces read are fetched.
    pub async fn head(&self, id: &str, index: usize, len: usize) -> Result<Vec<u8>> {
        let h = self.running(id).ok_or_else(|| anyhow!("{id}: not running"))?;
        self.restart_if_failed(id, &h).await?;
        h.wait_until_initialized().await?;
        let mut s = h.stream(index).await?;
        let mut buf = vec![0u8; len];
        s.read_exact(&mut buf).await?;
        Ok(buf)
    }

    /// Bring the cache under its limit, oldest first, sparing what was
    /// played lately.
    fn evict(&self) {
        let data = self.data.clone();
        let limit = self.cache_limit;
        let busy: HashSet<String> = self
            .used
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, at)| at.elapsed() < IN_USE)
            .map(|(id, _)| id.clone())
            .collect();
        let handles = self.handles.lock().unwrap().clone();
        let session = self.session.clone();
        tokio::spawn(async move {
            let victims = tokio::task::spawn_blocking(move || pick_victims(&data, limit, &busy))
                .await
                .unwrap_or_default();
            for (id, dir) in victims {
                if let Some(h) = handles.get(&id) {
                    let _ = session.delete(TorrentIdOrHash::Id(h.id()), true).await;
                }
                let _ = std::fs::remove_dir_all(&dir);
                eprintln!("cache: removed {id}");
            }
        });
        let gone: Vec<String> = self.handles.lock().unwrap().keys().filter(|id| !self.data.join(id).exists()).cloned().collect();
        let mut h = self.handles.lock().unwrap();
        for id in gone {
            h.remove(&id);
        }
    }
}

/// A download link answered with a web page, a sign-in page most often.
#[derive(Debug)]
pub struct WebPage(pub String);

impl std::fmt::Display for WebPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} returned a web page instead of a torrent file", self.0)
    }
}

impl std::error::Error for WebPage {}

/// No peer gave a magnet's metadata in time.
#[derive(Debug)]
pub struct NoPeers;

impl std::fmt::Display for NoPeers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no peer gave this torrent's metadata")
    }
}

impl std::error::Error for NoPeers {}

/// Error of [`Engine::meta`] when the metadata is still on its way.
#[derive(Debug)]
pub struct Pending;

impl std::fmt::Display for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("still looking for this torrent's peers")
    }
}

impl std::error::Error for Pending {}

fn magnet_of(r: &Release) -> Option<String> {
    r.magnet.clone().or_else(|| r.infohash.as_ref().map(|h| format!("magnet:?xt=urn:btih:{h}")))
}

fn touch(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.set_modified(SystemTime::now());
    }
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            // Allocated blocks: files are sparse while downloading.
            Ok(_) => e.metadata().map_or(0, |m| {
                use std::os::unix::fs::MetadataExt;
                m.blocks() * 512
            }),
            Err(_) => 0,
        })
        .sum()
}

/// Release directories to delete to fit `limit`, oldest first.
fn pick_victims(data: &Path, limit: u64, busy: &HashSet<String>) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(data) else { return Vec::new() };
    let mut dirs: Vec<(SystemTime, String, PathBuf, u64)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let id = e.file_name().to_str()?.to_string();
            catalog::valid_id(&id).then_some(())?;
            let at = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            let path = e.path();
            let size = dir_size(&path);
            Some((at, id, path, size))
        })
        .collect();
    let mut total: u64 = dirs.iter().map(|d| d.3).sum();
    dirs.sort_by_key(|d| d.0);
    let mut out = Vec::new();
    for (_, id, path, size) in dirs {
        if total <= limit {
            break;
        }
        if busy.contains(&id) {
            continue;
        }
        total = total.saturating_sub(size);
        out.push((id, path));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn victims() {
        let root = std::env::temp_dir().join(format!("torznab-stream-evict-{}", std::process::id()));
        let ids = ["l0000000000000001", "l0000000000000002", "l0000000000000003"];
        for (i, id) in ids.iter().enumerate() {
            let d = root.join(id);
            std::fs::create_dir_all(&d).unwrap();
            let f = std::fs::File::create(d.join("a.flac")).unwrap();
            std::io::Write::write_all(&mut &f, &vec![1u8; 64 * 1024]).unwrap();
            f.sync_all().unwrap();
            let f = std::fs::File::open(&d).unwrap();
            f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1000 + i as u64)).unwrap();
        }
        std::fs::create_dir_all(root.join("not-a-release")).unwrap();
        let busy: HashSet<String> = [ids[0].to_string()].into();
        // Room for all but the oldest one that is not busy.
        let sizes: Vec<u64> = ids.iter().map(|id| dir_size(&root.join(id))).collect();
        let limit = sizes.iter().sum::<u64>() - sizes[1];
        let v = pick_victims(&root, limit, &busy);
        assert_eq!(v.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), [ids[1]]);
        assert!(pick_victims(&root, 1 << 30, &busy).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
