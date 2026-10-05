# torznab-stream

**Torznab** source plugin for players that implement the source plugin
protocol v1 (separate process, JSON-RPC over stdin/stdout). It searches a
[Torznab](https://torznab.github.io/spec-1.3-draft/) indexer and plays music
straight from BitTorrent, while it downloads.

Torznab is the API that Prowlarr, Jackett, bitmagnet and many indexers
speak, so any of them works. The plugin ships with no indexer: you point it
at yours.

**Use it for content you have the right to share and download**: public
domain recordings, free-licensed music, your own releases. You are
responsible for what your indexer lists and what you download.

- **Search:** releases (shown as albums) and their artists. **Latest
  releases** lists what the indexer added last; it is also a shelf on
  the player's Home page.
- **Albums are torrents, tracks are files.** Opening a release fetches its
  `.torrent` (or its metadata from the swarm, for a magnet) and lists its
  audio files, in disc and track order. The cover is the torrent's
  `cover`/`folder`/`front` image when it has one; albums show it too once
  opened.
- **Covers for the rest:** search results come without images. For a
  release whose cover is not known, the plugin looks the artist and album
  up on [MusicBrainz](https://musicbrainz.org) and shows the release
  group's front cover from [Cover Art Archive](https://coverartarchive.org).
  Lookups happen only for the covers on screen, one a second as MusicBrainz
  asks, and their answers are cached (`covers.json` in the cache
  directory; a miss is tried again after a week). The setting turns it
  off; nothing is then sent to MusicBrainz.
- **Plays while downloading.** Playing a track selects that file only;
  the pieces at the playing position come first, so playback starts after
  the first piece and seeking ahead works. The next track starts
  downloading as soon as the current one plays (gapless preload).
- **Bit-perfect:** a local relay serves each file's original bytes, and the
  signal path says so (*relayed locally, original codec unchanged*). For
  FLAC files, the plugin reads the header before answering, so the player
  knows the real sample rate and depth, and refuses what the DAC cannot
  take.
- **Favourites:** star releases, artists and tracks. An indexer has no
  user library, so the plugin keeps them itself; they make up its library
  in the player's Albums, Artists and Tracks pages. A starred release keeps
  its `.torrent`, so it plays even once the indexer forgets it.

## Set up

In the plugin's settings, under *Indexer*:

- **Torznab API URL**:
  - Prowlarr: the indexer's Torznab URL, `http://localhost:9696/<n>/api`;
  - Jackett: *Copy Torznab Feed* on the indexer,
    `http://localhost:9117/api/v2.0/indexers/<name>/results/torznab/`;
  - bitmagnet: `http://localhost:3333/torznab`.

  `/api` is added when the URL does not end with it.
- **API key**: Prowlarr, Settings → General; Jackett, at the top of the
  dashboard. Empty for indexers that need none. A key left in the URL
  (`?apikey=…`) works too; the setting wins.

Changes apply at once, without a restart.

## Settings

| Setting | Default | |
|---|---|---|
| Torznab API URL | | See above. |
| API key | | See above. |
| Categories | Music | Presets: *Music*, `3000` (Audio) without audiobooks (`3030`) and music videos (`3020`); *Lossless music only*, `3040`; *Audiobooks*, `3030`; *All audio*, `3000`. Then the indexer's own audio categories, read from its `t=caps` (see below). *Custom*: the ids below. |
| Custom categories | | With *Custom*: ids separated by commas, to search several at once, e.g. `3040,100123`. |
| Hide releases with fewer seeders than | 1 | Nobody seeding means nothing to play. |
| Share what I play | on | Upload to other peers while the plugin runs. |
| Upload limit | 0 (none) | KiB/s. |
| Download limit | 0 (none) | KiB/s. Spreads disk writes, for computers that stall while a release downloads at full speed. |
| Find missing covers on MusicBrainz | on | See below. |
| Download cache | 10 GB | Played releases stay on disk up to this size, least recently played removed first. |

The sharing, download limit and cache settings restart the plugin.

**Categories from the indexer.** Each indexer declares its categories in
its capabilities (`t=caps`), its own included (`Podcasts`, `FLAC Hi-Res`,
often with ids from 100000). Once the URL is set, the plugin reads them and
lists the audio ones in the *Categories* menu, as *Indexer: Podcasts
(100123)*: Newznab's 3000 to 3999, and the indexer's own whose name is
about sound. The list follows the indexer: another URL, another list. It is
kept in the cache directory (`categories.json`), so the menu is complete
at start even while the indexer is down. The menu takes 45 of them at
most; *Custom* reaches the others.

## Install

**From the player's plugin hub:** install *Torznab (unofficial)*.

**By hand:** download `torznab-stream-x86_64` or `torznab-stream-aarch64`
from the [releases](https://github.com/anon7627/torznab-stream/releases),
check it against its `.sha256` file, make it executable, and declare it as
a source plugin in your player's configuration, with the id `torznab`.

**From source:**

```sh
cargo build --release
# target/release/torznab-stream
```

## Notes

- Your IP address is visible to the other peers of every torrent you play,
  as with any BitTorrent client. Turning sharing off does not change that.
- The first play of a release waits for its metadata: instant with a
  `.torrent` link, up to a minute for a bare magnet (the plugin then asks
  you to try again in a moment, and keeps looking meanwhile). A release
  with no seeders cannot play.
- Release titles are free text. The plugin reads `Artist - Album (Year)
  [tags]`, and prefers the indexer's `artist`, `album` and `year`
  attributes when it gives them.
- Files: the plugin's data directory (given by the player) holds the
  favourites, the releases seen in searches and the
  `.torrent` files; its cache directory holds the downloads.
- The relay listens on `127.0.0.1` only, on a port kept across restarts so
  that cover URLs stay valid.

## Protocol

Source plugin protocol 1, with `favorites`, `library` and `settings`, without `auth`. Tracks resolve with
`delivery: "proxied"`. There is no `library.playlists`.

| Ref | Meaning |
|---|---|
| `recent`, `favorites` | Top-level sections |
| `a/<name>` | Artist: a search by artist (`t=music&artist=` when the indexer supports it) |
| `r/<id>` | Release (shown as an album): its audio files |
| `t/<id>/<index>` | Track: file `<index>` of the torrent |

`<id>` is the release's info hash, or `l` and a hash of its link when the
indexer gives no info hash.

Error codes: a missing URL or a rejected API key answers a plain error
whose message points at the settings; an
indexer asking to slow down `rate_limited`; an unreachable indexer, or
metadata still on its way from the swarm, `network`; a FLAC file the DAC
cannot take `unavailable`.

## Development

```sh
cargo test
cargo clippy --all-targets
cargo build --release --bins --examples && tests/e2e.py   # local indexer, tracker and seeder; needs ffmpeg
```

The CI runs the tests on every push, and on every tag `v*` builds static
binaries (musl) for x86_64 and aarch64 and attaches them, with their
SHA-256, to a GitHub release. `contrib/hub-entry.toml` is the entry for the
plugin hub.

## Licence

MIT.
