#!/usr/bin/env python3
"""End-to-end test of the plugin, all on 127.0.0.1: a Torznab indexer and
an HTTP tracker (this script), a seeder (examples/seed.rs) sharing generated
FLAC files, and the plugin driven over JSON-RPC as a host would.

Needs ffmpeg, and `cargo build --release --bins --examples` first."""
import hashlib, http.server, json, os, queue, shutil, socketserver, subprocess, urllib.error, sys, tempfile, threading, time
import urllib.parse, urllib.request

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
BIN = os.path.join(ROOT, "target", "release", "torznab-stream")
SEED = os.path.join(ROOT, "target", "release", "examples", "seed")
KEY = "k3y"
WALL_MBID = "0d4cf3a9-7d6c-3d7b-ae7a-3b8a2a0a6b1e"
W = tempfile.mkdtemp(prefix="torznab-stream-e2e-")
ALBUM = os.path.join(W, "share", "Glenn Gould - Goldberg Variations (1955)")
TORRENT = os.path.join(W, "album.torrent")
# The DAC: up to 48 kHz / 24 bits.
OUT = {"device": "hw:9,0", "bit_perfect": True, "max_rate": 48000, "max_bits": 24, "rates": [44100, 48000]}

ok = 0
def check(c, msg):
    global ok
    print(("PASS " if c else "FAIL ") + msg, flush=True)
    ok += 0 if c else 1

# ------------------------------------------------------------------ media

os.makedirs(os.path.join(ALBUM, "CD2"))
def ff(*args):
    subprocess.run(["ffmpeg", "-loglevel", "error", "-y", *args], check=True)
ff("-f", "lavfi", "-i", "anoisesrc=d=12:a=0.3", "-ar", "44100", "-ac", "2", "-sample_fmt", "s16", os.path.join(ALBUM, "01 - Aria.flac"))
ff("-f", "lavfi", "-i", "sine=f=440:d=10", "-ar", "44100", "-ac", "2", "-sample_fmt", "s16", os.path.join(ALBUM, "02 - Variatio 1.flac"))
ff("-f", "lavfi", "-i", "sine=f=660:d=5", "-ar", "96000", "-ac", "2", "-sample_fmt", "s32", "-bits_per_raw_sample", "24", os.path.join(ALBUM, "CD2", "01 - Hi-Res.flac"))
ff("-f", "lavfi", "-i", "color=c=red:s=64x64", "-frames:v", "1", os.path.join(ALBUM, "cover.jpg"))
with open(os.path.join(ALBUM, "notes.txt"), "w") as f:
    f.write("not music\n")

def sha(path):
    return hashlib.sha256(open(path, "rb").read()).hexdigest()

# ------------------------------------------------- indexer and tracker

peers = {}  # info_hash bytes -> set of (ip, port)
requests = []

def caps():
    return """<?xml version="1.0"?><caps><limits max="100" default="50"/><searching>
<search available="yes" supportedParams="q"/><music-search available="yes" supportedParams="q,artist,album"/>
</searching><categories><category id="3000" name="Audio"><subcat id="3040" name="Audio/Lossless"/></category>
<category id="2000" name="Movies"/><category id="100123" name="Podcasts"/></categories></caps>"""

def feed(port, q):
    size = sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(ALBUM) for f in fs)
    every = [
        f"""<item><title>Glenn Gould - Goldberg Variations (1955) [FLAC]</title>
<guid>gould</guid><prowlarrindexer id="1">Public Domain Test</prowlarrindexer>
<link>http://127.0.0.1:{port}/details/gould</link>
<enclosure url="http://127.0.0.1:{port}/dl/gould" type="application/x-bittorrent"/><size>{size}</size>
<torznab:attr name="category" value="3040"/><torznab:attr name="seeders" value="1"/><torznab:attr name="peers" value="1"/></item>""",
        f"""<item><title>Login.Wall.1930.FLAC-GRP</title><link>http://127.0.0.1:{port}/details/wall</link>
<enclosure url="http://127.0.0.1:{port}/dl/wall" type="application/x-bittorrent"/><torznab:attr name="seeders" value="3"/></item>""",
        f"""<item><title>Jane Reader - Moby Dick (Audiobook) [MP3]</title><link>http://127.0.0.1:{port}/dl/none</link>
<category>3000</category><category>3030</category><torznab:attr name="seeders" value="4"/></item>""",
        f"""<item><title>Nobody - Seeds This (1920) [MP3]</title><link>http://127.0.0.1:{port}/dl/none</link>
<torznab:attr name="seeders" value="0"/></item>""",
    ]
    words = q.lower().split()
    items = [i for i in every if all(w in i.lower() for w in words)]
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel><title>Test</title>
<newznab:response xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/" offset="0" total="{len(items)}"/>{"".join(items)}</channel></rss>"""

def bencode(v):
    if isinstance(v, int): return b"i%de" % v
    if isinstance(v, bytes): return b"%d:%s" % (len(v), v)
    if isinstance(v, str): return bencode(v.encode())
    if isinstance(v, dict): return b"d" + b"".join(bencode(k) + bencode(v[k]) for k in sorted(v)) + b"e"
    raise TypeError(v)

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def send(self, code, body, ctype="application/xml"):
        body = body.encode() if isinstance(body, str) else body
        self.send_response(code); self.send_header("Content-Type", ctype); self.send_header("Content-Length", str(len(body))); self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        u = urllib.parse.urlsplit(self.path)
        qs = urllib.parse.parse_qs(u.query, keep_blank_values=True)
        requests.append(self.path)
        if u.path == "/mb/release-group/":
            q = qs.get("query", [""])[0]
            found = 'releasegroup:"Wall"' in q and 'artist:"Login"' in q
            groups = [{"id": WALL_MBID, "score": 100}] if found else []
            return self.send(200, json.dumps({"release-groups": groups}), "application/json")
        if u.path == f"/caa/release-group/{WALL_MBID}/front-500":
            return self.send(200, open(os.path.join(ALBUM, "cover.jpg"), "rb").read(), "image/jpeg")
        if u.path == "/announce":
            raw = urllib.parse.parse_qs(u.query.encode().decode("latin-1"), encoding="latin-1")
            h = raw["info_hash"][0].encode("latin-1"); port = int(qs["port"][0])
            me = (self.client_address[0], port)
            known = peers.setdefault(h, set())
            if qs.get("event", [""])[0] != "stopped": known.add(me)
            compact = b"".join(bytes(map(int, ip.split("."))) + p.to_bytes(2, "big") for ip, p in known if (ip, p) != me and "." in ip)
            return self.send(200, bencode({"interval": 5, "peers": compact}), "text/plain")
        if u.path == "/api":
            if qs.get("apikey", [""])[0] != KEY:
                return self.send(200, '<error code="100" description="Incorrect user credentials"/>')
            t = qs.get("t", [""])[0]
            if t == "caps": return self.send(200, caps())
            if t in ("search", "music"):
                return self.send(200, feed(self.server.server_address[1], qs.get("q", qs.get("artist", [""]))[0]))
            return self.send(200, '<error code="202" description="No such function"/>')
        if u.path == "/dl/wall" or (u.path == "/dl/gould" and qs.get("apikey", [""])[0] != KEY):
            # A tracker's download without its key: the sign-in page.
            return self.send(200, "<!DOCTYPE html><html><body>Sign in</body></html>", "text/html")
        if u.path == "/dl/gould":
            # Like Prowlarr: the download link redirects to the file.
            self.send_response(302); self.send_header("Location", "/files/album.torrent"); self.send_header("Content-Length", "0"); self.end_headers(); return
        if u.path == "/files/album.torrent":
            return self.send(200, open(TORRENT, "rb").read(), "application/x-bittorrent")
        self.send(404, "no")

class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
srv = Server(("127.0.0.1", 0), H)
PORT = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

seeder = subprocess.Popen([SEED, ALBUM, f"http://127.0.0.1:{PORT}/announce", TORRENT], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(os.path.join(W, "seed.log"), "w"), text=True)
line = seeder.stdout.readline().strip()
check(line.startswith("seeding "), "seeder: " + line)
deadline = time.time() + 20
while time.time() < deadline and not peers:
    time.sleep(0.2)
check(bool(peers), "seeder announced to the tracker")

# ------------------------------------------------------------------ plugin

class P:
    def __init__(s):
        env = {**os.environ, "TORZNAB_STREAM_MUSICBRAINZ": f"http://127.0.0.1:{PORT}/mb", "TORZNAB_STREAM_COVER_ART_ARCHIVE": f"http://127.0.0.1:{PORT}/caa"}
        s.p = subprocess.Popen([BIN], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(os.path.join(W, "plugin.log"), "a"), text=True)
        s.q = {}; s.n = 0; s.notes = queue.Queue()
        threading.Thread(target=s.read, daemon=True).start()
    def read(s):
        for l in s.p.stdout:
            m = json.loads(l)
            if "id" in m: s.q[m["id"]].put(m)
            else: s.notes.put(m)
    def note(s, method, timeout=15):
        end = time.time() + timeout
        while time.time() < end:
            try:
                m = s.notes.get(timeout=max(0.1, end - time.time()))
            except queue.Empty:
                break
            if m.get("method") == method: return m["params"]
        return None
    def call(s, method, params=None, timeout=30):
        s.n += 1; i = s.n; s.q[i] = queue.Queue()
        s.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params or {}}) + "\n"); s.p.stdin.flush()
        m = s.q[i].get(timeout=timeout); return m.get("result", m.get("error"))
    def notify(s, method, params):
        s.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params}) + "\n"); s.p.stdin.flush()

DATA, CACHE = os.path.join(W, "pdata"), os.path.join(W, "pcache")
p = P()
# The URL as a user may type it: no /api, a trailing slash.
BASE = f"http://127.0.0.1:{PORT}/"
SETTINGS = {"cache_gb": 1, "url": BASE, "apikey": KEY}
init = p.call("initialize", {"protocol": 1, "data_dir": DATA, "cache_dir": CACHE, "locale": "fr-FR", "output": OUT, "settings": {"cache_gb": 1}})
caps_ = init["capabilities"]
check(init["plugin"]["id"] == "torznab" and init["plugin"]["name"] == "Torznab" and not caps_["auth"] and caps_["library"] and caps_["resolve"], "initialize, no sign-in")
sets = {x["key"]: x for x in init["settings"]}
check(list(sets)[:2] == ["url", "apikey"] and set(sets) == {"url", "apikey", "category", "custom_categories", "min_seeders", "share", "upload_kbps", "download_kbps", "online_covers", "cache_gb"}
      and sets["url"]["label"] == "URL de l'API Torznab" and not sets["url"].get("restart"), "settings declared (fr), URL and key first")
e = p.call("search", {"query": "gould", "offset": 0, "limit": 20})
check(e.get("code") == -32603 and "réglages" in e.get("message", ""), "search without URL -> asks for the settings: " + e.get("message", ""))
p.notify("settings.changed", {"settings": {**SETTINGS, "apikey": "wrong"}})
e = p.call("search", {"query": "gould", "offset": 0, "limit": 20})
check(e.get("code") == -32603 and "clé API" in e.get("message", ""), "wrong API key -> says so: " + e.get("message", ""))
p.notify("settings.changed", {"settings": SETTINGS})
decl = p.note("settings.declared")
opts = [o for x in (decl or {}).get("settings", []) if x["key"] == "category" for o in x["options"]]
check([o["value"] for o in opts] == ["music", "lossless", "audiobooks", "audio", "cat:3000", "cat:3040", "cat:100123", "custom"]
      and opts[6]["label"] == "Indexeur : Podcasts (100123)", "indexer's audio categories offered: " + str([o["label"] for o in opts[4:7]]))
p.notify("settings.changed", {"settings": {**SETTINGS, "category": "cat:100123"}})
p.call("browse.list", {"ref": "recent", "offset": 0, "limit": 50})
check("cat=100123" in requests[-1], "an indexer category chosen in the menu is searched")
p.notify("settings.changed", {"settings": SETTINGS})
recent = p.call("browse.list", {"ref": "recent", "offset": 0, "limit": 50})
titles = [x["title"] for x in recent["items"]]
check(titles == ["Goldberg Variations", "Login Wall"], "music hides audiobooks and releases without seeders: " + str(titles))
alb = recent["items"][0]
check(alb["artist"] == "Glenn Gould" and alb["year"] == 1955 and alb["subtitle"].startswith("FLAC · ") and "1 source" in alb["subtitle"], "album fields: " + alb["subtitle"])
check(not any(r.startswith("/details") for r in requests), "details pages never fetched")
check(any("cat=3000" in r for r in requests), "music asks for cat=3000")
p.notify("settings.changed", {"settings": {**SETTINGS, "category": "audiobooks"}})
books = p.call("browse.list", {"ref": "recent", "offset": 0, "limit": 50})
check("Moby Dick" in [x["title"] for x in books["items"]] and "cat=3030" in requests[-1], "audiobooks: cat=3030, audiobook listed")
p.notify("settings.changed", {"settings": {**SETTINGS, "category": "custom", "custom_categories": "3050, 100123"}})
p.call("browse.list", {"ref": "recent", "offset": 0, "limit": 50})
check("cat=3050%2C100123" in requests[-1], "custom categories sent as given")
p.notify("settings.changed", {"settings": SETTINGS})

s = p.call("search", {"query": "Gould", "offset": 0, "limit": 20})
s2 = p.call("search", {"query": "login", "offset": 0, "limit": 20})
lw = [x for x in s2["groups"] if x["kind"] == "album"][0]["items"]
check([(x["title"], x.get("artist"), x.get("year")) for x in lw if "Wall" in x["title"]] == [("Wall", "Login", 1930)], "dotted title split with the search words")
wall_art = [x.get("art", "") for x in lw if x["title"] == "Wall"][0]
check(wall_art.startswith("http://127.0.0.1:") and "/cover?artist=Login&album=Wall" in wall_art, "no cover in the torrent: an online one, through the relay")
img = urllib.request.urlopen(wall_art, timeout=20).read()
check(img == open(os.path.join(ALBUM, "cover.jpg"), "rb").read(), "relay -> MusicBrainz -> Cover Art Archive image")
n_mb = sum(r.startswith("/mb/") for r in requests)
urllib.request.urlopen(wall_art, timeout=20).read()
check(n_mb == 1 and sum(r.startswith("/mb/") for r in requests) == 1, "MusicBrainz asked once, then cached")
try:
    urllib.request.urlopen(wall_art.replace("album=Wall", "album=Unknown"), timeout=20); check(False, "unknown album -> 404")
except urllib.error.HTTPError as e:
    check(e.code == 404, "unknown album -> 404")
p.notify("settings.changed", {"settings": {**SETTINGS, "online_covers": False}})
lw2 = [x for x in p.call("search", {"query": "login", "offset": 0, "limit": 20})["groups"] if x["kind"] == "album"][0]["items"]
check(not [x for x in lw2 if x["title"] == "Wall"][0].get("art"), "online covers off: none")
p.notify("settings.changed", {"settings": SETTINGS})
g = {x["kind"]: x for x in s["groups"]}
check([a["ref"] for a in g["artist"]["items"]] == ["a/Glenn Gould"] and g["album"]["items"][0]["ref"] == alb["ref"], "search: artist and album")
art_list = p.call("browse.list", {"ref": "a/Glenn Gould", "offset": 0, "limit": 20})
check([x["ref"] for x in art_list["items"]] == [alb["ref"]] and any("t=music" in r and "artist=Glenn" in r for r in requests), "artist page uses t=music&artist=")

wall = [x for x in recent["items"] if x["title"] == "Login Wall"][0]
e = p.call("browse.list", {"ref": wall["ref"], "offset": 0, "limit": 50})
check(e.get("code") == -32603 and "page web" in e.get("message", "") and KEY not in e.get("message", ""), "web page instead of a torrent -> says so: " + e.get("message", ""))
tr = p.call("browse.list", {"ref": alb["ref"], "offset": 0, "limit": 50})
check(any(r.startswith("/dl/gould?apikey=" + KEY) for r in requests), "API key added to the indexer's own download link")
names = [(x["title"], x.get("disc_no"), x.get("track_no")) for x in tr["items"]]
check(names == [("Aria", None, 1), ("Variatio 1", None, 2), ("Hi-Res", 2, 1)], "tracks in order, audio only: " + str(names))
t1 = tr["items"][0]
check(t1["album"] == "Goldberg Variations" and t1["album_ref"] == alb["ref"] and t1["format"]["codec"] == "flac", "track fields")
art = t1.get("art", "")
check(art.startswith("http://127.0.0.1:") and art.endswith("/cover.jpg"), "cover from the torrent: " + art)
check(os.path.exists(os.path.join(DATA, "torrents", alb["ref"][2:] + ".torrent")), ".torrent kept in the data directory")
check(p.call("item.get", {"ref": alb["ref"]}).get("art") == art, "album carries the torrent's cover once known")
again = p.call("browse.list", {"ref": "recent", "offset": 0, "limit": 50})["items"]
check([x.get("art") for x in again if x["ref"] == alb["ref"]] == [art], "search results too")
got = p.call("item.get", {"ref": t1["ref"]})
check(got.get("title") == "Aria", "item.get on a track")

# Opening the album joined the swarm; a user takes a few seconds to press
# play, by which time the first track is on its way.
time.sleep(6)
t0 = time.time()
r = p.call("track.resolve", {"ref": t1["ref"], "purpose": "play"})
took = time.time() - t0
check(r.get("delivery") == "proxied" and r["url"].startswith("http://127.0.0.1:") and took < 1, "resolve: proxied relay URL, at once (%.2fs)" % took)
check(r["format"] == {"sample_rate": 44100, "bits": 16, "channels": 2, "codec": "flac"} and abs(r.get("duration_ms", 0) - 12000) < 50, "resolve: real format and duration " + json.dumps(r["format"]))

src = os.path.join(ALBUM, "01 - Aria.flac")
size = os.path.getsize(src)
head = urllib.request.urlopen(urllib.request.Request(r["url"], method="HEAD"))
check(int(head.headers["Content-Length"]) == size and head.headers["Accept-Ranges"] == "bytes", "HEAD: length and ranges")
mid = size // 2
req = urllib.request.Request(r["url"], headers={"Range": f"bytes={mid}-{mid + 99999}"})
part = urllib.request.urlopen(req, timeout=60)
body = part.read()
check(part.status == 206 and part.headers["Content-Range"] == f"bytes {mid}-{mid + 99999}/{size}" and body == open(src, "rb").read()[mid:mid + 100000], "range from the middle: 206, exact bytes")
full = urllib.request.urlopen(r["url"], timeout=60).read()
check(hashlib.sha256(full).hexdigest() == sha(src), "whole file: bit for bit")
cover = urllib.request.urlopen(art, timeout=60).read()
check(cover == open(os.path.join(ALBUM, "cover.jpg"), "rb").read(), "cover bytes")

hi = tr["items"][2]
e = p.call("track.resolve", {"ref": hi["ref"], "purpose": "preload"})
check(e.get("code") == -32003 and "96000" in e.get("message", ""), "96 kHz refused by a 48 kHz DAC: " + e.get("message", ""))
r2 = p.call("track.resolve", {"ref": tr["items"][1]["ref"], "purpose": "preload"})
check(r2.get("format", {}).get("sample_rate") == 44100, "preload of the next track")
check(p.call("track.resolve", {"ref": alb["ref"] }).get("code") == -32002, "resolve of an album -> not_found")
check(p.call("browse.list", {"ref": "r/" + "0" * 40, "offset": 0, "limit": 5}).get("code") is not None, "unknown release -> error")

check(p.call("favorites.set", {"ref": alb["ref"], "on": True}) is None, "favourite an album")
check(p.call("favorites.set", {"ref": t1["ref"], "on": True}) is None, "favourite a track")
la = p.call("library.albums", {"offset": 0, "limit": 50})
lt = p.call("library.tracks", {"offset": 0, "limit": 50})
check([x["ref"] for x in la["items"]] == [alb["ref"]] and [x["ref"] for x in lt["items"]] == [t1["ref"]], "library from favourites")
check(p.call("library.playlists", {"offset": 0, "limit": 5}).get("code") == -32601, "no playlists")
p.call("shutdown")
p.p.wait(timeout=10)

# Restart: the release plays from the cache and the saved .torrent, with
# the indexer gone and nobody seeding.
seeder.stdin.close(); seeder.wait(timeout=10)
srv.shutdown()
p = P()
init2 = p.call("initialize", {"protocol": 1, "data_dir": DATA, "cache_dir": CACHE, "locale": "en", "output": OUT, "settings": SETTINGS})
opts2 = [o["value"] for x in init2["settings"] if x["key"] == "category" for o in x["options"]]
check("cat:100123" in opts2, "categories remembered: offered right at start, indexer down")
tr2 = p.call("browse.list", {"ref": alb["ref"], "offset": 0, "limit": 50})
check([x["ref"] for x in tr2.get("items", [])] == [x["ref"] for x in tr["items"]], "release lists offline")
r = p.call("track.resolve", {"ref": t1["ref"], "purpose": "play"})
full = urllib.request.urlopen(r["url"], timeout=60).read()
check(hashlib.sha256(full).hexdigest() == sha(src), "cached track plays offline, bit for bit")
p.call("shutdown")
p.p.wait(timeout=10)

print("\n%s (%d failure%s). Logs in %s" % ("OK" if not ok else "FAILED", ok, "" if ok == 1 else "s", W))
if not ok:
    shutil.rmtree(W)
sys.exit(1 if ok else 0)
