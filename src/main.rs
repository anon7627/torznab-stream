//! Torznab source plugin (source plugin protocol 1).
//!
//! Speaks JSON-RPC over stdin/stdout with the player, Torznab with the
//! indexer set in the plugin's settings (Prowlarr, Jackett, bitmagnet…), and
//! BitTorrent with the swarm. A release is shown as an album, its audio
//! files as tracks. Tracks play while they download: a local relay serves
//! each file with its original bytes, and the reader's position decides
//! which pieces come first.
//!
//! The indexer gives no library: favourites stay in the plugin's data
//! directory and make up its library.

mod catalog;
mod covers;
mod durations;
mod engine;
mod favorites;
mod items;
mod names;
mod relay;
mod torznab;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use catalog::Catalog;
use engine::{Engine, NoPeers, Pending, WebPage};
use favorites::Favorites;
use items::{Output, Ref};
use relay::Relay;
use torznab::{Caps, Client, Indexer, Query};

const PROTOCOL: u64 = 1;
const PAGE: u64 = 200;
/// How long `browse.list` waits for a release's metadata before asking the
/// user to come back.
const META_WAIT: Duration = Duration::from_secs(8);
/// How long the background reading of an album's FLAC headers waits for
/// each one.
const HEADERS_WAIT: Duration = Duration::from_secs(120);
/// How long `track.resolve` waits for the torrent and a FLAC header, in
/// all: the host expects an answer within 5 s.
const HEADER_WAIT: Duration = Duration::from_millis(4500);

struct RpcError {
    code: i64,
    message: String,
    retry_after: Option<u64>,
}

fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError { code, message: message.into(), retry_after: None }
}

impl From<torznab::Error> for RpcError {
    fn from(e: torznab::Error) -> RpcError {
        let message = e.to_string();
        match e {
            // No sign-in to offer: the key is a setting.
            torznab::Error::Auth(_) => rpc_err(-32603, message),
            torznab::Error::RateLimited => RpcError { code: -32004, message, retry_after: Some(60) },
            torznab::Error::Network(_) => rpc_err(-32005, message),
            torznab::Error::Indexer(_) => rpc_err(-32603, message),
        }
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> RpcError {
        if e.downcast_ref::<Pending>().is_some() {
            return rpc_err(-32005, e.to_string());
        }
        rpc_err(-32005, format!("{e:#}"))
    }
}

type Reply = Result<Value, RpcError>;

struct Out(Mutex<std::io::Stdout>);

impl Out {
    fn send(&self, v: Value) {
        use std::io::Write;
        let mut out = self.0.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }
}

// ---------------------------------------------------------------- settings

#[derive(Clone, Debug, PartialEq)]
struct Settings {
    /// The Torznab endpoint, normalized; `None` until the URL is set.
    indexer: Option<Indexer>,
    category: Category,
    min_seeders: u64,
    share: bool,
    upload_kbps: u32,
    download_kbps: u32,
    cache_gb: u64,
    online_covers: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { indexer: None, category: Category::Music, min_seeders: 1, share: true, upload_kbps: 0, download_kbps: 0, cache_gb: 10, online_covers: true }
    }
}

impl Settings {
    fn from_json(v: &Value) -> Settings {
        let d = Settings::default();
        let num = |k: &str, min: u64, max: u64, def: u64| v[k].as_f64().map_or(def, |n| (n.max(0.0) as u64).clamp(min, max));
        let text = |k: &str| v[k].as_str().unwrap_or("").trim().to_string();
        Settings {
            indexer: Indexer::from_settings(&text("url"), &text("apikey")),
            category: Category::from_settings(v["category"].as_str().unwrap_or(""), &text("custom_categories")),
            min_seeders: num("min_seeders", 0, 1000, d.min_seeders),
            share: v["share"].as_bool().unwrap_or(d.share),
            upload_kbps: num("upload_kbps", 0, 1_000_000, u64::from(d.upload_kbps)) as u32,
            download_kbps: num("download_kbps", 0, 1_000_000, u64::from(d.download_kbps)) as u32,
            online_covers: v["online_covers"].as_bool().unwrap_or(d.online_covers),
            cache_gb: num("cache_gb", 1, 10_000, d.cache_gb),
        }
    }


    /// The settings, with the indexer's own audio categories in the
    /// Categories menu.
    fn declaration(fr: bool, source: &[(u32, String)]) -> Value {
        let t = |en: &'static str, f: &'static str| if fr { f } else { en };
        let d = Settings::default();
        let mut options = vec![
            json!({"value": "music", "label": t("Music (no audiobooks, no videos)", "Musique (sans livres audio ni vidéos)")}),
            json!({"value": "lossless", "label": t("Lossless music only", "Musique sans perte uniquement")}),
            json!({"value": "audiobooks", "label": t("Audiobooks", "Livres audio")}),
            json!({"value": "audio", "label": t("All audio", "Tout l'audio")}),
        ];
        // The protocol allows 50 options: the presets, Custom, and 45 of
        // the indexer's.
        let prefix = t("Indexer: ", "Indexeur : ");
        options.extend(source.iter().take(45).map(|(id, name)| {
            let name: String = name.chars().take(150).collect();
            json!({"value": format!("cat:{id}"), "label": format!("{prefix}{name} ({id})")})
        }));
        options.push(json!({"value": "custom", "label": t("Custom (below)", "Personnalisé (ci-dessous)")}));
        json!([
            {"key": "url", "type": "string", "section": t("Indexer", "Indexeur"),
             "label": t("Torznab API URL", "URL de l'API Torznab"),
             "description": t("Prowlarr: the indexer's Torznab URL (http://localhost:9696/1/api). Jackett: Copy Torznab Feed. bitmagnet: http://localhost:3333/torznab. /api is added when missing.",
                              "Prowlarr : l'URL Torznab de l'indexeur (http://localhost:9696/1/api). Jackett : Copy Torznab Feed. bitmagnet : http://localhost:3333/torznab. /api est ajouté s'il manque."),
             "placeholder": "http://localhost:9696/1/api", "max_length": 1024, "default": ""},
            {"key": "apikey", "type": "string", "section": t("Indexer", "Indexeur"),
             "label": t("API key", "Clé API"),
             "description": t("Prowlarr: Settings → General. Jackett: at the top of the dashboard. Leave empty if the indexer needs none.",
                              "Prowlarr : Settings → General. Jackett : en haut du tableau de bord. Vide si l'indexeur n'en demande pas."),
             "max_length": 256, "default": ""},
            {"key": "category", "type": "choice", "section": t("Search", "Recherche"),
             "label": t("Categories", "Catégories"),
             "description": t("The indexer's own audio categories are listed after the presets once its URL is set.",
                              "Les catégories audio de l'indexeur suivent les préréglages une fois son URL renseignée."),
             "options": options,
             "default": "music"},
            {"key": "custom_categories", "type": "string", "section": t("Search", "Recherche"),
             "label": t("Custom categories", "Catégories personnalisées"),
             "description": t("With Custom: category ids separated by commas, to search several at once, e.g. 3040,100123. The ids are shown in the Categories menu.",
                              "Avec Personnalisé : des ids de catégories séparés par des virgules, pour en chercher plusieurs à la fois, par ex. 3040,100123. Les ids figurent dans le menu Catégories."),
             "placeholder": "3050,100123", "max_length": 200, "default": ""},
            {"key": "min_seeders", "type": "number", "section": t("Search", "Recherche"),
             "label": t("Hide releases with fewer seeders than", "Masquer les releases ayant moins de sources que"),
             "description": t("Releases nobody shares cannot play.", "Une release que personne ne partage ne peut pas être lue."),
             "integer": true, "min": 0, "max": 1000, "default": d.min_seeders},
            {"key": "share", "type": "bool", "section": t("Sharing", "Partage"),
             "label": t("Share what I play with other peers", "Partager ce que j'écoute avec les autres pairs"),
             "description": t("BitTorrent works because people upload. Your IP address is visible to the peers either way.",
                              "BitTorrent fonctionne parce que chacun partage. Votre adresse IP est visible des pairs dans tous les cas."),
             "restart": true, "default": d.share},
            {"key": "upload_kbps", "type": "number", "section": t("Sharing", "Partage"),
             "label": t("Upload limit", "Limite d'envoi"),
             "description": t("0: no limit.", "0 : pas de limite."),
             "integer": true, "min": 0, "max": 1_000_000, "unit": "KiB/s", "restart": true, "default": d.upload_kbps},
            {"key": "download_kbps", "type": "number", "section": t("Storage", "Stockage"),
             "label": t("Download limit", "Limite de téléchargement"),
             "description": t("0: no limit. A limit spreads disk writes, for computers that stall while a release downloads at full speed.",
                              "0 : pas de limite. Une limite étale les écritures sur le disque, pour les ordinateurs qui se figent quand une release se télécharge à pleine vitesse."),
             "integer": true, "min": 0, "max": 1_000_000, "unit": "KiB/s", "restart": true, "default": d.download_kbps},
            {"key": "online_covers", "type": "bool", "section": t("Covers", "Pochettes"),
             "label": t("Find missing covers on MusicBrainz", "Chercher les pochettes manquantes sur MusicBrainz"),
             "description": t("For releases without a cover in their torrent: the artist and album names are sent to MusicBrainz, and the image comes from Cover Art Archive.",
                              "Pour les releases sans pochette dans leur torrent : les noms d'artiste et d'album sont envoyés à MusicBrainz, et l'image vient de Cover Art Archive."),
             "default": d.online_covers},
            {"key": "cache_gb", "type": "number", "section": t("Storage", "Stockage"),
             "label": t("Download cache", "Cache des téléchargements"),
             "description": t("Played releases stay on disk up to this size, so they play again without the swarm.",
                              "Les releases écoutées restent sur le disque jusqu'à cette taille, et se relisent sans l'essaim."),
             "integer": true, "min": 1, "max": 10_000, "unit": t("GB", "Go"), "restart": true, "default": d.cache_gb},
        ])
    }
}

/// Which releases a search asks for.
#[derive(Clone, Debug, PartialEq)]
enum Category {
    /// Audio, without the audiobook and music video subcategories.
    Music,
    Lossless,
    Audiobooks,
    AllAudio,
    Custom(Vec<u32>),
}

impl Category {
    fn from_settings(choice: &str, custom: &str) -> Category {
        match choice {
            "lossless" => Category::Lossless,
            "audiobooks" => Category::Audiobooks,
            "audio" => Category::AllAudio,
            c if c.starts_with("cat:") => match c[4..].parse() {
                Ok(id) => Category::Custom(vec![id]),
                Err(_) => Category::Music,
            },
            "custom" => {
                let ids: Vec<u32> = custom.split([',', ' ', ';']).filter_map(|c| c.trim().parse().ok()).take(50).collect();
                if ids.is_empty() { Category::Music } else { Category::Custom(ids) }
            }
            _ => Category::Music,
        }
    }

    /// The `cat` parameter.
    fn param(&self) -> String {
        match self {
            Category::Music | Category::AllAudio => torznab::CAT_AUDIO.to_string(),
            Category::Lossless => torznab::CAT_LOSSLESS.to_string(),
            Category::Audiobooks => torznab::CAT_AUDIOBOOK.to_string(),
            Category::Custom(ids) => ids.iter().map(u32::to_string).collect::<Vec<_>>().join(","),
        }
    }

    /// Whether a release the indexer returned belongs here. Asking for
    /// Audio returns its subcategories too.
    fn keeps(&self, r: &torznab::Release) -> bool {
        match self {
            Category::Music => !r.categories.iter().any(|c| [torznab::CAT_VIDEO, torznab::CAT_AUDIOBOOK].contains(c)),
            _ => true,
        }
    }
}

// ------------------------------------------------------------------ plugin

/// `<cache_dir>/categories.json`: the audio categories the indexer at
/// `url` declared.
fn load_categories(dir: &std::path::Path, ix: &Indexer) -> Vec<(u32, String)> {
    let v: Value = std::fs::read_to_string(dir.join("categories.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if v["url"].as_str() != Some(ix.url.as_str()) {
        return Vec::new();
    }
    serde_json::from_value(v["categories"].clone()).unwrap_or_default()
}

fn save_categories(dir: &std::path::Path, ix: &Indexer, cats: &[(u32, String)]) {
    let v = json!({"url": ix.url, "categories": cats});
    if let Err(e) = std::fs::write(dir.join("categories.json"), v.to_string()) {
        eprintln!("cannot save categories.json: {e}");
    }
}

struct Plugin {
    fr: AtomicBool,
    output: Mutex<Output>,
    settings: Mutex<Settings>,
    /// The capabilities of the indexer they were read from.
    caps: tokio::sync::Mutex<Option<(Indexer, Caps)>>,
    client: Client,
    catalog: Mutex<Catalog>,
    favorites: Mutex<Favorites>,
    durations: Arc<Mutex<durations::Durations>>,
    /// Releases whose FLAC headers are being read.
    reading_headers: Arc<Mutex<std::collections::HashSet<String>>>,
    engine: OnceLock<Arc<Engine>>,
    relay: OnceLock<Relay>,
    out: OnceLock<Arc<Out>>,
    /// The indexer's audio categories, as last declared (and the locale
    /// of the labels).
    declared: Mutex<(Vec<(u32, String)>, bool)>,
    cache_dir: OnceLock<PathBuf>,
}

impl Plugin {
    fn new() -> Plugin {
        Plugin {
            fr: AtomicBool::new(false),
            output: Mutex::new(Output::default()),
            settings: Mutex::new(Settings::default()),
            caps: tokio::sync::Mutex::new(None),
            client: Client::new(),
            catalog: Mutex::new(Catalog::default()),
            favorites: Mutex::new(Favorites::default()),
            durations: Arc::default(),
            reading_headers: Arc::default(),
            engine: OnceLock::new(),
            relay: OnceLock::new(),
            out: OnceLock::new(),
            declared: Mutex::new((Vec::new(), false)),
            cache_dir: OnceLock::new(),
        }
    }

    fn fr(&self) -> bool {
        self.fr.load(Ordering::Relaxed)
    }

    fn t(&self, en: &'static str, fr: &'static str) -> &'static str {
        if self.fr() { fr } else { en }
    }

    fn set_locale(&self, l: &Value) {
        self.fr.store(l.as_str().is_some_and(|l| l.starts_with("fr")), Ordering::Relaxed);
    }

    async fn initialize(&self, p: &Value) -> Reply {
        let dir = |k: &str, sub: &str| {
            p[k].as_str().map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("torznab-stream").join(sub))
        };
        let data_dir = dir("data_dir", "data");
        let cache_dir = dir("cache_dir", "cache");
        let _ = std::fs::create_dir_all(&data_dir);
        let _ = std::fs::create_dir_all(&cache_dir);
        self.set_locale(&p["locale"]);
        *self.output.lock().unwrap() = Output::from_json(&p["output"]);
        let settings = Settings::from_json(&p["settings"]);
        *self.settings.lock().unwrap() = settings.clone();
        *self.catalog.lock().unwrap() = Catalog::load(&data_dir);
        *self.favorites.lock().unwrap() = Favorites::load(&data_dir);
        *self.durations.lock().unwrap() = durations::Durations::load(&data_dir);
        // Left by 0.1.0, which signed in instead of using settings.
        let _ = std::fs::remove_file(data_dir.join("indexer.json"));
        if settings.indexer.is_none() {
            eprintln!("no indexer URL in the settings yet");
        }
        let proto = p["protocol"].as_u64().unwrap_or(0);
        if proto != PROTOCOL {
            eprintln!("host speaks protocol {proto}, this plugin {PROTOCOL}");
        }
        let opts_cache_dir = cache_dir.clone();
        let _ = self.cache_dir.set(cache_dir.clone());
        // What the indexer declared last time, so that a stored choice of
        // one of its categories shows at once.
        let source = settings.indexer.as_ref().map(|ix| load_categories(&cache_dir, ix)).unwrap_or_default();
        *self.declared.lock().unwrap() = (source.clone(), self.fr());
        let opts = engine::Options {
            data_dir: data_dir.clone(),
            cache_dir,
            cache_limit: settings.cache_gb.saturating_mul(1_000_000_000),
            share: settings.share,
            upload_kbps: settings.upload_kbps,
            download_kbps: settings.download_kbps,
        };
        match Engine::start(opts).await {
            Ok(e) => {
                let e = Arc::new(e);
                let covers = Arc::new(covers::Covers::new(&opts_cache_dir));
                match Relay::start(e.clone(), Some(covers), &data_dir).await {
                    Ok(r) => {
                        let _ = self.relay.set(r);
                    }
                    Err(err) => eprintln!("cannot start the relay: {err:#}"),
                }
                let _ = self.engine.set(e);
            }
            Err(err) => eprintln!("cannot start BitTorrent: {err:#}"),
        }
        Ok(json!({
            "protocol": PROTOCOL,
            "plugin": {"id": "torznab", "name": "Torznab", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "auth": false, "browse": true, "search": true, "resolve": true,
                "favorites": true, "reporting": false, "remote_control": false,
                "library": true
            },
            "settings": Settings::declaration(self.fr(), &source),
        }))
    }

    fn engine(&self) -> Result<(&Arc<Engine>, &Relay), RpcError> {
        match (self.engine.get(), self.relay.get()) {
            (Some(e), Some(r)) => Ok((e, r)),
            _ => Err(rpc_err(-32003, "BitTorrent could not start; see the log")),
        }
    }

    // ------------------------------------------------------------- search

    fn indexer(&self) -> Result<Indexer, RpcError> {
        self.settings.lock().unwrap().indexer.clone().ok_or_else(|| {
            rpc_err(-32603, self.t(
                "set the Torznab API URL in the plugin's settings",
                "indiquez l'URL de l'API Torznab dans les réglages du plugin"))
        })
    }

    /// A Torznab error, worded for the user where the settings can fix it.
    fn indexer_error(&self, e: torznab::Error) -> RpcError {
        match e {
            torznab::Error::Auth(_) => rpc_err(-32603, self.t(
                "the indexer refused the API key: check it in the plugin's settings",
                "l'indexeur refuse la clé API : vérifiez-la dans les réglages du plugin")),
            e => e.into(),
        }
    }

    async fn caps(&self, ix: &Indexer) -> Result<Caps, RpcError> {
        let mut caps = self.caps.lock().await;
        if let Some((of, c)) = &*caps
            && of == ix
        {
            return Ok(c.clone());
        }
        let c = self.client.caps(ix).await.map_err(|e| self.indexer_error(e))?;
        *caps = Some((ix.clone(), c.clone()));
        drop(caps);
        if let Some(dir) = self.cache_dir.get() {
            save_categories(dir, ix, &c.audio_categories());
        }
        self.declare(c.audio_categories());
        Ok(c)
    }

    /// Send the settings again when the indexer's categories or the
    /// language changed.
    fn declare(&self, source: Vec<(u32, String)>) {
        let fr = self.fr();
        {
            let mut d = self.declared.lock().unwrap();
            if d.0 == source && d.1 == fr {
                return;
            }
            *d = (source.clone(), fr);
        }
        if let Some(out) = self.out.get() {
            out.send(json!({"jsonrpc": "2.0", "method": "settings.declared",
                            "params": {"settings": Settings::declaration(fr, &source)}}));
        }
    }

    /// Read the indexer's capabilities (and so its categories) after a
    /// start or a change of settings.
    async fn refresh_caps(&self) {
        let Ok(ix) = self.indexer() else {
            let fr = self.fr();
            if *self.declared.lock().unwrap() != (Vec::new(), fr) {
                self.declare(Vec::new());
            }
            return;
        };
        if let Err(e) = self.caps(&ix).await {
            eprintln!("indexer capabilities: {}", e.message);
            // The language may have changed all the same.
            let source = self.declared.lock().unwrap().0.clone();
            self.declare(source);
        }
    }

    /// One page of releases as albums, remembered in the catalogue.
    async fn releases(&self, q: Option<&str>, artist: Option<&str>, offset: u64, limit: u64) -> Reply {
        let ix = self.indexer()?;
        let caps = self.caps(&ix).await?;
        let (category, min_seeders) = {
            let s = self.settings.lock().unwrap();
            (s.category.clone(), s.min_seeders)
        };
        let cat = &category.param();
        let page = self
            .client
            .search(&ix, &caps, &Query { q, artist, cat, offset, limit })
            .await
            .map_err(|e| self.indexer_error(e))?;
        let got = page.releases.len() as u64;
        let fr = self.fr();
        let list: Vec<Value> = {
            let mut cat = self.catalog.lock().unwrap();
            let list = page
                .releases
                .into_iter()
                .filter(|r| r.seeders.is_none_or(|s| s >= min_seeders) && category.keeps(r))
                .map(|mut r| {
                    // Titles that do not say who the artist is.
                    if r.artist.is_none() {
                        let info = items::album_info(&r);
                        let split = info.artist.is_none().then(|| names::split_artist(&info.album, artist.or(q)?)).flatten();
                        if let Some((a, album)) = split {
                            r.artist = Some(a);
                            r.album = Some(album);
                            r.year = info.year;
                        }
                    }
                    let id = catalog::id_of(&r);
                    let mut v = items::release(&id, &r, fr);
                    self.album_art(&id, &mut v);
                    cat.put(r);
                    v
                })
                .collect();
            if let Err(e) = cat.save() {
                eprintln!("cannot save the catalogue: {e}");
            }
            list
        };
        // Filtering may shorten a page: whether more follow is the
        // indexer's answer, not ours.
        let has_more = match page.total {
            Some(t) => offset + got < t,
            None => got >= limit,
        };
        let mut v = json!({"items": list, "has_more": has_more && got > 0});
        if let Some(t) = page.total {
            v["total"] = t.into();
        }
        Ok(v)
    }

    fn root(&self) -> Reply {
        let recent = json!({"ref": "recent", "kind": "folder", "title": self.t("Latest releases", "Dernières releases"), "browsable": true});
        let favs = json!({"ref": "favorites", "kind": "folder", "title": self.t("Favourites", "Favoris"), "browsable": true});
        Ok(json!({"sections": [recent, favs], "home": [recent]}))
    }

    /// Why a release's metadata could not be had, worded for the user.
    /// These are plain errors: the host shows their text, where `network`
    /// would only say "offline".
    fn meta_error(&self, err: anyhow::Error) -> RpcError {
        let msg = if err.downcast_ref::<Pending>().is_some() {
            self.t(
                "looking for this torrent's peers: try again in a moment",
                "recherche des pairs de ce torrent en cours : réessayez dans un instant").to_string()
        } else if err.downcast_ref::<NoPeers>().is_some() {
            self.t(
                "no peer answered for this torrent: nobody may be sharing it",
                "aucun pair n'a répondu pour ce torrent : personne ne le partage peut-être").to_string()
        } else if let Some(WebPage(url)) = err.downcast_ref::<WebPage>() {
            format!("{}{url}", self.t(
                "the indexer's download link gives a web page (a sign-in page?) instead of a torrent file: ",
                "le lien de téléchargement de l'indexeur renvoie une page web (de connexion ?) au lieu d'un fichier torrent : "))
        } else {
            return RpcError::from(err);
        };
        rpc_err(-32603, msg)
    }

    /// A release's metadata: what the catalogue knows of it, and its files.
    async fn release_files(&self, id: &str, wait: Duration) -> Result<(items::AlbumInfo, Arc<engine::Meta>, Option<torznab::Release>), RpcError> {
        let (e, _) = self.engine()?;
        let release = self.catalog.lock().unwrap().get(id).cloned();
        // The key goes on the download link here, never into the catalogue.
        let ix = self.settings.lock().unwrap().indexer.clone();
        let fetchable = release.clone().map(|mut r| {
            if let (Some(ix), Some(link)) = (&ix, &r.link) {
                r.link = Some(ix.download_url(link));
            }
            r
        });
        let meta = e.meta(id, fetchable, wait).await.map_err(|err| self.meta_error(err))?;
        let info = release.as_ref().map_or_else(|| items::album_info_from_meta(&meta), items::album_info);
        Ok((info, meta, release))
    }

    /// A release's cover: the torrent's image, else the indexer's, else
    /// one looked up online.
    fn cover_url(&self, id: &str, meta: &engine::Meta, release: Option<&torznab::Release>, info: &items::AlbumInfo) -> Option<String> {
        let files: Vec<(usize, String, u64)> = meta.files.iter().map(|f| (f.index, f.path.clone(), f.len)).collect();
        match (names::cover(&files), self.relay.get()) {
            (Some(i), Some(relay)) => Some(relay.url(id, i, &files.iter().find(|f| f.0 == i)?.1)),
            _ => release.and_then(|r| r.cover.clone()).or_else(|| self.online_cover(info.artist.as_deref(), &info.album)),
        }
    }

    /// Where the relay serves the cover found online for `artist`'s
    /// `album`, when that is wanted.
    fn online_cover(&self, artist: Option<&str>, album: &str) -> Option<String> {
        let artist = artist.filter(|a| !a.trim().is_empty())?;
        if !self.settings.lock().unwrap().online_covers || album.trim().is_empty() {
            return None;
        }
        Some(self.relay.get()?.cover_url(artist, album))
    }

    /// The cover of an album item: the torrent's when its metadata is on
    /// disk, else the indexer's, else one looked up online.
    fn album_art(&self, id: &str, v: &mut Value) {
        if let Some(art) = self.known_cover(id) {
            v["art"] = art.into();
        } else if v.get("art").is_none()
            && let Some(art) = self.online_cover(v["artist"].as_str(), v["title"].as_str().unwrap_or(""))
        {
            v["art"] = art.into();
        }
    }

    /// The cover inside the torrent of release `id`, once its metadata is
    /// on disk: albums show it too, not only their tracks.
    fn known_cover(&self, id: &str) -> Option<String> {
        let meta = self.engine.get()?.known(id)?;
        let files: Vec<(usize, String, u64)> = meta.files.iter().map(|f| (f.index, f.path.clone(), f.len)).collect();
        let i = names::cover(&files)?;
        Some(self.relay.get()?.url(id, i, &files.iter().find(|f| f.0 == i)?.1))
    }

    async fn tracks(&self, id: &str, wait: Duration) -> Result<Vec<Value>, RpcError> {
        let (info, meta, release) = self.release_files(id, wait).await?;
        let art = self.cover_url(id, &meta, release.as_ref(), &info);
        let tracks = items::tracks(id, &meta, &info, art.as_deref(), &self.durations.lock().unwrap());
        // Join the swarm now with the cover and the first track selected,
        // so that playback starts without waiting for peers. With nothing
        // left to fetch, librqbit would drop them again.
        let files: Vec<(usize, String, u64)> = meta.files.iter().map(|f| (f.index, f.path.clone(), f.len)).collect();
        let first = tracks.first().and_then(|t| match items::parse_ref(t["ref"].as_str()?) {
            Some(Ref::Track(_, i)) => Some(i),
            _ => None,
        });
        // Then read the FLAC headers of the tracks whose length is not
        // known yet: only their first pieces are fetched. The lengths show
        // the next time the album is listed.
        let unknown: Vec<(usize, u64)> = tracks
            .iter()
            .filter(|t| t.get("duration_ms").is_none() && t["format"]["codec"] == "flac")
            .filter_map(|t| match items::parse_ref(t["ref"].as_str()?) {
                Some(Ref::Track(_, i)) => Some((i, meta.file(i)?.len)),
                _ => None,
            })
            .collect();
        if let Ok((engine, _)) = self.engine() {
            let warm: Vec<usize> = names::cover(&files).into_iter().chain(first).collect();
            let (engine, id) = (engine.clone(), id.to_string());
            let (durations, reading) = (self.durations.clone(), self.reading_headers.clone());
            tokio::spawn(async move {
                for i in warm {
                    if let Err(e) = engine.open(&id, i).await {
                        eprintln!("{id}: {e:#}");
                    }
                }
                if unknown.is_empty() || !reading.lock().unwrap().insert(id.clone()) {
                    return;
                }
                for (index, len) in unknown {
                    let head = tokio::time::timeout(HEADERS_WAIT, engine.head(&id, index, 8192.min(len as usize))).await;
                    let Ok(Ok(buf)) = head else {
                        eprintln!("{id}/{index}: no FLAC header yet, lengths left for later");
                        break;
                    };
                    if let Some((rate, _, _, samples)) = items::streaminfo(&buf).filter(|s| s.0 > 0) {
                        durations.lock().unwrap().set(&id, index, samples * 1000 / u64::from(rate));
                    }
                }
                reading.lock().unwrap().remove(&id);
            });
        }
        Ok(tracks)
    }

    async fn list(&self, p: &Value) -> Reply {
        let r = p["ref"].as_str().unwrap_or("");
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(PAGE).clamp(1, PAGE);
        match items::parse_ref(r).ok_or_else(|| rpc_err(-32002, "no such list"))? {
            Ref::Section("recent") => self.releases(None, None, offset, limit).await,
            Ref::Section("favorites") => Ok(page(self.favorites.lock().unwrap().all().to_vec(), offset, limit)),
            Ref::Artist(name) => self.releases(None, Some(name), offset, limit).await,
            Ref::Release(id) => Ok(page(self.tracks(id, META_WAIT).await?, offset, limit)),
            _ => Err(rpc_err(-32002, "no such list")),
        }
    }

    async fn search(&self, p: &Value) -> Reply {
        let query = p["query"].as_str().unwrap_or("").trim();
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, PAGE);
        let wanted: Vec<&str> = p["kinds"]
            .as_array()
            .map(|k| k.iter().filter_map(Value::as_str).collect())
            .unwrap_or_else(|| vec!["artist", "album"]);
        if query.is_empty() || !(wanted.contains(&"album") || wanted.contains(&"artist")) {
            return Ok(json!({ "groups": [] }));
        }
        let mut albums = self.releases(Some(query), None, offset, limit).await?;
        let mut groups = Vec::new();
        // Indexers know releases only: the artists are those of the
        // releases found, on the first page.
        if wanted.contains(&"artist") && offset == 0 {
            let mut seen = Vec::<String>::new();
            for a in albums["items"].as_array().into_iter().flatten() {
                if let Some(name) = a["artist"].as_str()
                    && name.to_lowercase().contains(&query.to_lowercase())
                    && !seen.iter().any(|s| s.eq_ignore_ascii_case(name))
                {
                    seen.push(name.to_string());
                }
            }
            let artists: Vec<Value> = seen.iter().map(|n| items::artist(n)).collect();
            groups.push(json!({"kind": "artist", "items": artists, "total": artists.len(), "has_more": false}));
        }
        if wanted.contains(&"album") {
            albums["kind"] = "album".into();
            groups.push(albums);
        }
        Ok(json!({ "groups": groups }))
    }

    async fn item(&self, r: &str) -> Reply {
        match items::parse_ref(r).ok_or_else(|| rpc_err(-32002, "no such item"))? {
            Ref::Artist(name) => Ok(items::artist(name)),
            Ref::Release(id) => {
                let known = self.catalog.lock().unwrap().get(id).cloned();
                if let Some(rel) = known {
                    let mut v = items::release(id, &rel, self.fr());
                    self.album_art(id, &mut v);
                    return Ok(v);
                }
                let (info, meta, _) = self.release_files(id, Duration::ZERO).await?;
                let rel = torznab::Release {
                    title: meta.name.clone(),
                    artist: info.artist,
                    album: Some(info.album),
                    year: info.year,
                    ..Default::default()
                };
                let mut v = items::release(id, &rel, self.fr());
                self.album_art(id, &mut v);
                Ok(v)
            }
            Ref::Track(id, _) => self
                .tracks(id, Duration::from_secs(5))
                .await?
                .into_iter()
                .find(|t| t["ref"] == r)
                .ok_or_else(|| rpc_err(-32002, "no such track")),
            Ref::Section(_) => Err(rpc_err(-32002, "not a music item")),
        }
    }

    async fn favorite(&self, p: &Value) -> Reply {
        let r = p["ref"].as_str().unwrap_or("");
        let saved = if p["on"].as_bool().unwrap_or(false) {
            let it = self.item(r).await?;
            self.favorites.lock().unwrap().add(it)
        } else {
            self.favorites.lock().unwrap().remove(r)
        };
        saved.map_err(|e| rpc_err(-32603, format!("cannot save the favourites: {e}")))?;
        Ok(Value::Null)
    }

    fn library(&self, method: &str, p: &Value) -> Reply {
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(PAGE).clamp(1, PAGE);
        let kind = match method {
            "library.albums" => "album",
            "library.artists" => "artist",
            _ => "track",
        };
        Ok(page(self.favorites.lock().unwrap().of_kind(kind), offset, limit))
    }

    // ------------------------------------------------------------ resolve

    async fn resolve(&self, p: &Value) -> Reply {
        let r = p["ref"].as_str().unwrap_or("");
        let Some(Ref::Track(id, index)) = items::parse_ref(r) else {
            return Err(rpc_err(-32002, "not a track"));
        };
        let (engine, relay) = self.engine()?;
        let (_, meta, _) = self.release_files(id, Duration::from_secs(3)).await?;
        let file = meta.file(index).filter(|f| names::is_audio(&f.path)).ok_or_else(|| rpc_err(-32002, "no such track"))?.clone();
        let codec = names::codec(&file.path);
        let mut v = json!({
            "url": relay.url(id, index, &file.path),
            "format": {"codec": codec},
            "live": false,
            "delivery": "proxied",
        });
        // Start the download now, for a preload as much as for a play.
        let deadline = tokio::time::Instant::now() + HEADER_WAIT;
        let opened = tokio::time::timeout_at(deadline, engine.open(id, index)).await;
        let handle = match opened {
            Ok(Ok(h)) => Some(h),
            Ok(Err(e)) => return Err(rpc_err(-32003, format!("{e:#}"))),
            Err(_) => {
                // Opening goes on; the relay picks it up.
                let (engine, id) = (engine.clone(), id.to_string());
                tokio::spawn(async move {
                    let _ = engine.open(&id, index).await;
                });
                None
            }
        };
        if codec == Some("flac")
            && let Some(h) = handle
        {
            let head = async {
                let mut s = h.stream(index).await?;
                let mut buf = vec![0u8; 8192.min(file.len as usize)];
                s.read_exact(&mut buf).await?;
                anyhow::Ok(buf)
            };
            match tokio::time::timeout_at(deadline, head).await {
                Ok(Ok(buf)) => {
                    if let Some((rate, bits, channels, samples)) = items::streaminfo(&buf) {
                        if !self.output.lock().unwrap().takes(rate, bits) {
                            return Err(rpc_err(-32003, format!("the DAC cannot take {rate} Hz / {bits} bits")));
                        }
                        v["format"] = json!({"sample_rate": rate, "bits": bits, "channels": channels, "codec": "flac"});
                        if samples > 0 {
                            let ms = samples * 1000 / u64::from(rate);
                            v["duration_ms"] = ms.into();
                            self.durations.lock().unwrap().set(id, index, ms);
                        }
                    }
                }
                Ok(Err(e)) => eprintln!("{id}/{index}: cannot read the FLAC header: {e:#}"),
                Err(_) => eprintln!("{id}/{index}: FLAC header not there yet, format left to the engine"),
            }
        }
        Ok(v)
    }

    // ----------------------------------------------------------- dispatch

    async fn handle(&self, method: &str, p: &Value) -> Reply {
        match method {
            "initialize" => self.initialize(p).await,
            "browse.root" => self.root(),
            "browse.list" => self.list(p).await,
            "search" => self.search(p).await,
            "item.get" => self.item(p["ref"].as_str().unwrap_or("")).await,
            "favorites.set" => self.favorite(p).await,
            "library.albums" | "library.artists" | "library.tracks" => self.library(method, p),
            "track.resolve" => self.resolve(p).await,
            _ => Err(rpc_err(-32601, format!("method not found: {method}"))),
        }
    }

    fn notify(&self, method: &str, p: &Value) {
        match method {
            "output.changed" => *self.output.lock().unwrap() = Output::from_json(&p["output"]),
            "locale.changed" => self.set_locale(&p["locale"]),
            // Settings with `restart` come back through `initialize`.
            "settings.changed" => *self.settings.lock().unwrap() = Settings::from_json(&p["settings"]),
            _ => {}
        }
    }
}

fn page(all: Vec<Value>, offset: u64, limit: u64) -> Value {
    let total = all.len() as u64;
    let items: Vec<Value> = all.into_iter().skip(offset as usize).take(limit as usize).collect();
    json!({"items": items, "total": total, "has_more": offset + limit < total})
}

fn reply(id: Value, r: Reply) -> Value {
    match r {
        Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
        Err(e) => {
            let mut err = json!({"code": e.code, "message": e.message});
            if let Some(s) = e.retry_after {
                err["data"] = json!({ "retry_after": s });
            }
            json!({"jsonrpc": "2.0", "id": id, "error": err})
        }
    }
}

#[tokio::main]
async fn main() {
    if let Some(a) = std::env::args().nth(1) {
        if a == "--version" {
            println!("torznab-stream {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        eprintln!("unknown option {a}");
    }
    let out = Arc::new(Out(Mutex::new(std::io::stdout())));
    let plugin = Arc::new(Plugin::new());
    let _ = plugin.out.set(out.clone());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = msg["method"].as_str().map(str::to_string) else {
            continue; // an answer; this plugin sends no requests
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = msg.get("id").cloned() else {
            plugin.notify(&method, &params);
            if matches!(method.as_str(), "settings.changed" | "locale.changed") {
                let plugin = plugin.clone();
                tokio::spawn(async move { plugin.refresh_caps().await });
            }
            continue;
        };
        if method == "shutdown" {
            out.send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
            break;
        }
        // The handshake first, in order; everything else may overlap.
        if method == "initialize" {
            out.send(reply(id, plugin.handle(&method, &params).await));
            let plugin = plugin.clone();
            tokio::spawn(async move { plugin.refresh_caps().await });
        } else {
            let (plugin, out) = (plugin.clone(), out.clone());
            tokio::spawn(async move {
                let r = plugin.handle(&method, &params).await;
                out.send(reply(id, r));
            });
        }
    }
    if let Some(e) = plugin.engine.get() {
        e.stop().await;
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        let s = Settings::from_json(&json!({"category": "lossless", "min_seeders": 3, "share": false, "cache_gb": 0, "x": 1,
                                            "url": " http://localhost:9696/1 ", "apikey": "k"}));
        let ix = Indexer { url: "http://localhost:9696/1/api".into(), apikey: Some("k".into()) };
        assert_eq!(s, Settings { indexer: Some(ix), category: Category::Lossless, min_seeders: 3, share: false, upload_kbps: 0, download_kbps: 0, cache_gb: 1, online_covers: true });
        assert_eq!(Settings::from_json(&json!({"url": "nope"})).indexer, None);
        let cat = |c: &str, x: &str| Settings::from_json(&json!({"category": c, "custom_categories": x})).category;
        assert_eq!(cat("custom", "3050, 100123;x"), Category::Custom(vec![3050, 100123]));
        assert_eq!(cat("custom", "").param(), "3000");
        assert_eq!(cat("custom", "3050,100123").param(), "3050,100123");
        assert_eq!(cat("audiobooks", "").param(), "3030");
        let rel = |c: Vec<u32>| torznab::Release { categories: c, ..Default::default() };
        assert!(Category::Music.keeps(&rel(vec![3000, 3040])) && Category::Music.keeps(&rel(vec![])));
        assert!(!Category::Music.keeps(&rel(vec![3000, 3030])) && Category::AllAudio.keeps(&rel(vec![3030])));
        assert_eq!(Settings::from_json(&Value::Null), Settings::default());
        assert_eq!(cat("cat:100123", "").param(), "100123");
        assert_eq!(cat("cat:x", ""), Category::Music);
        let d = Settings::declaration(false, &[(3040, "Audio/Lossless".into()), (100123, "Podcasts".into())]);
        let opts: Vec<&str> = d[2]["options"].as_array().unwrap().iter().map(|o| o["value"].as_str().unwrap()).collect();
        assert_eq!(opts, ["music", "lossless", "audiobooks", "audio", "cat:3040", "cat:100123", "custom"]);
        assert_eq!(d[2]["options"][5]["label"], "Indexer: Podcasts (100123)");
        let many: Vec<(u32, String)> = (0..80).map(|i| (100_000 + i, format!("Music {i}"))).collect();
        assert_eq!(Settings::declaration(false, &many)[2]["options"].as_array().unwrap().len(), 50);
        for e in d.as_array().unwrap() {
            assert!(e["key"].is_string() && e["label"].is_string() && !e["default"].is_null());
        }
    }

    #[tokio::test]
    async fn without_indexer() {
        let p = Plugin::new();
        let e = p.handle("search", &json!({"query": "x"})).await.err().unwrap();
        assert_eq!(e.code, -32603);
        assert!(e.message.contains("settings"));
        let e = p.handle("library.playlists", &Value::Null).await.err().unwrap();
        assert_eq!(e.code, -32601);
        let root = p.root().ok().unwrap();
        assert_eq!(root["sections"][0]["ref"], "recent");
        assert_eq!(root["home"][0]["ref"], "recent");
    }
}
