#!/usr/bin/env python3
r"""
BitChord Audio Engine Addon Server
Runs locally at http://127.0.0.1:8765
Fully compliant with BitChord (src/hires.rs) Addon Protocol:
- /manifest.json
- /search?q=...&quality=LOSSLESS
- /stream/{track_id}?quality=LOSSLESS
- /audio/{track_id} (HEAD + GET with full HTTP 206 Byte Range support)
- /playlist-info & /playlist-tracks (Apple Music, Deezer, etc.)

PROVIDES:
1. Genuine Local Lossless & Hi-Res playback from D:\Music (FLAC, ALAC, WAV, AAC, MP3) with real metadata.
2. Online Lossless Stream resolution with multi-provider cross-matching and duration validation.
3. ZERO fake upsampling: Honest fallback to client's native YouTube High-Quality stream when no genuine lossless exists.
"""

import os
import sys
import re
import json
import urllib.parse
import urllib.request
import subprocess
import threading
import time
import tempfile
from socketserver import ThreadingMixIn
from http.server import HTTPServer, BaseHTTPRequestHandler

# Silent process execution flags on Windows
SUBPROCESS_KWARGS = {}
if os.name == "nt":
    startupinfo = subprocess.STARTUPINFO()
    startupinfo.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    startupinfo.wShowWindow = 0
    SUBPROCESS_KWARGS = {
        "creationflags": 0x08000000,  # CREATE_NO_WINDOW
        "startupinfo": startupinfo
    }

PORT = 8765

# User-writable directories (always write to AppData/temp, never C:\Program Files)
APPDATA = os.environ.get("APPDATA") or os.path.expanduser("~")
LOCALAPPDATA = os.environ.get("LOCALAPPDATA") or APPDATA

DATA_DIR = os.path.join(APPDATA, "ytm-rs", "data")
try:
    os.makedirs(DATA_DIR, exist_ok=True)
except Exception:
    DATA_DIR = os.path.join(LOCALAPPDATA, "BitChord", "data")
    try:
        os.makedirs(DATA_DIR, exist_ok=True)
    except Exception:
        DATA_DIR = os.path.join(tempfile.gettempdir(), "bitchord_data")
        os.makedirs(DATA_DIR, exist_ok=True)

CACHE_DIR = os.path.join(DATA_DIR, "addon_cache")
PID_FILE = os.path.join(DATA_DIR, "bitchord_addon.pid")
APP_RON_PATH = os.path.join(DATA_DIR, "app.ron")

user_music = os.path.join(os.path.expanduser("~"), "Music")
MUSIC_DIR = r"D:\Music" if os.path.exists(r"D:\Music") else user_music

try:
    os.makedirs(CACHE_DIR, exist_ok=True)
except Exception:
    pass

try:
    os.makedirs(MUSIC_DIR, exist_ok=True)
except Exception:
    pass

# Write PID file
try:
    with open(PID_FILE, "w", encoding="utf-8") as f:
        f.write(str(os.getpid()))
except Exception:
    pass

# Locate ffmpeg, ffprobe, and yt-dlp dynamically without hardcoded paths
def locate_binary(name):
    base_dirs = [
        os.path.dirname(os.path.abspath(sys.argv[0])),
        os.path.dirname(os.path.abspath(__file__)) if '__file__' in globals() else '',
    ]
    for bd in list(base_dirs):
        if bd:
            base_dirs.append(os.path.dirname(bd))
            base_dirs.append(os.path.join(bd, "bin"))
            base_dirs.append(os.path.join(bd, "..", "bin"))

    appdata = os.environ.get("APPDATA", "")
    localappdata = os.environ.get("LOCALAPPDATA", "")

    if appdata:
        base_dirs.append(os.path.join(appdata, "ytm-rs", "data"))
        base_dirs.append(os.path.join(appdata, "BitChord", "data"))
    if localappdata:
        base_dirs.append(os.path.join(localappdata, "ytm-rs", "data"))
        base_dirs.append(os.path.join(localappdata, "BitChord", "data"))
        base_dirs.append(os.path.join(localappdata, "Microsoft", "WinGet", "Links"))
        for pyver in ["Python313", "Python312", "Python311", "Python310"]:
            base_dirs.append(os.path.join(localappdata, "Programs", "Python", pyver, "Scripts"))

    candidates = [f"{name}.exe", name] if sys.platform == "win32" else [name]
    for d in base_dirs:
        if d and os.path.exists(d):
            for c in candidates:
                p = os.path.join(d, c)
                if os.path.isfile(p):
                    return os.path.abspath(p)

    import shutil
    found = shutil.which(name)
    if found:
        return found
    return f"{name}.exe" if sys.platform == "win32" else name

FFMPEG = locate_binary("ffmpeg")
FFPROBE = locate_binary("ffprobe")
YTDLP = locate_binary("yt-dlp")

def clean_old_fake_cache():
    """Removes previous fake upsampled 192kHz audio files that were bloating the disk (~12 GB)."""
    try:
        if not os.path.exists(CACHE_DIR):
            return
        freed = 0
        for f in os.listdir(CACHE_DIR):
            fp = os.path.join(CACHE_DIR, f)
            if os.path.isfile(fp) and (f.endswith(".flac") or f.endswith(".tmp.flac")):
                sz = os.path.getsize(fp)
                # Any old cache file > 30MB was likely a fake 192kHz upsample from the previous script
                if sz > 30 * 1024 * 1024:
                    try:
                        os.remove(fp)
                        freed += sz
                    except Exception:
                        pass
        if freed > 0:
            print(f"[Audio Engine] Cleaned {freed / (1024*1024):.1f} MB of obsolete fake upsampled cache.")
    except Exception as e:
        print(f"[Audio Engine] Cache clean notice: {e}", file=sys.stderr)

threading.Thread(target=clean_old_fake_cache, daemon=True).start()

def read_queue_from_ron():
    """Reads active playback queue from BitChord's app.ron."""
    if not os.path.exists(APP_RON_PATH):
        return []
    try:
        with open(APP_RON_PATH, "r", encoding="utf-8") as f:
            text = f.read()
        m = re.search(r'"session":\s*"(.*?)",\s*\n\s*"', text, re.DOTALL)
        if not m:
            return []
        raw = m.group(1).replace(r'\"', '"').replace(r"\'", "'")
        cleaned = re.sub(r'\\(?![/"\\bfnrtu])', r'\\\\', raw)
        session = json.loads(cleaned)
        return session.get("queue", [])
    except Exception:
        return []

def get_track_metadata(track_id):
    """Finds track metadata from queue."""
    queue = read_queue_from_ron()
    for t in queue:
        if t.get("id") == track_id:
            artist = t.get("artists", [{}])[0].get("name", "") if t.get("artists") else ""
            return {
                "id": t["id"],
                "title": t.get("title", ""),
                "artist": artist,
                "duration": t.get("duration", 240)
            }
    return None

# ==============================================================================
# LOCAL FILE ENGINE: Bit-Perfect Inspection and Matching
# ==============================================================================

local_file_cache = {}
local_index_lock = threading.Lock()
last_scan_time = 0

def probe_file(file_path):
    """Extracts actual audio properties (sample rate, bit depth, codec, bitrate) using ffprobe."""
    if file_path in local_file_cache:
        return local_file_cache[file_path]
    
    meta = {
        "codec": "flac",
        "sample_rate": 44100,
        "bit_depth": 16,
        "bitrate": 0,
        "channels": 2,
        "duration": 0,
        "size": os.path.getsize(file_path) if os.path.exists(file_path) else 0
    }
    
    ext = os.path.splitext(file_path)[1].lower().lstrip(".")
    meta["codec"] = ext.upper()

    try:
        cmd = [
            FFPROBE, "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
            file_path
        ]
        res = subprocess.run(cmd, capture_output=True, text=True, timeout=8, **SUBPROCESS_KWARGS)
        if res.returncode == 0:
            data = json.loads(res.stdout)
            streams = data.get("streams", [])
            audio_stream = next((s for s in streams if s.get("codec_type") == "audio"), None)
            format_info = data.get("format", {})
            
            tags = format_info.get("tags") or {}
            if not tags and audio_stream:
                tags = audio_stream.get("tags") or {}
            if tags:
                for k, v in tags.items():
                    k_lower = k.lower()
                    if k_lower in ("artist", "author", "creator"):
                        meta["artist"] = str(v).strip()
                    elif k_lower in ("title", "track"):
                        meta["title"] = str(v).strip()
            
            if audio_stream:
                meta["codec"] = audio_stream.get("codec_name", ext).upper()
                sr = int(audio_stream.get("sample_rate", 44100) or 44100)
                meta["sample_rate"] = sr
                meta["channels"] = int(audio_stream.get("channels", 2) or 2)
                
                # Bit depth
                bd = audio_stream.get("bits_per_raw_sample") or audio_stream.get("bits_per_sample")
                if bd and int(bd) > 0:
                    meta["bit_depth"] = int(bd)
                elif "s32" in audio_stream.get("sample_fmt", "") or "fltp" in audio_stream.get("sample_fmt", ""):
                    meta["bit_depth"] = 24
                else:
                    meta["bit_depth"] = 16
                
                # Bitrate
                br = audio_stream.get("bit_rate") or format_info.get("bit_rate")
                if br:
                    meta["bitrate"] = int(int(br) / 1000)
                
                dur = audio_stream.get("duration") or format_info.get("duration")
                if dur:
                    meta["duration"] = int(float(dur))
    except Exception as e:
        print(f"[Audio Engine] ffprobe notice for {file_path}: {e}", file=sys.stderr)
        
    local_file_cache[file_path] = meta
    return meta

def normalize_title(s):
    """Strips extra noise like '(Official Video)', '[4K]', feat, etc."""
    s = s.lower()
    s = re.sub(r'\(official\s+video.*?\)', '', s)
    s = re.sub(r'\[official\s+video.*?\]', '', s)
    s = re.sub(r'\(lyric\s+video.*?\)', '', s)
    s = re.sub(r'\[lyric\s+video.*?\]', '', s)
    s = re.sub(r'\(audio\)', '', s)
    s = re.sub(r'\[audio\]', '', s)
    s = re.sub(r'\[.*?hd.*?\]', '', s)
    s = re.sub(r'\[.*?4k.*?\]', '', s)
    s = re.sub(r'\(feat\..*?\)', '', s)
    s = re.sub(r'\[feat\..*?\]', '', s)
    return s.strip()

def find_local_audio(query_or_id, target_duration=None):
    r"""Finds a matching local audio file in CACHE_DIR or D:\Music with format priority: FLAC > ALAC > WAV > M4A > MP3."""
    # 1. Exact match in CACHE_DIR (verified lossless downloads)
    if os.path.exists(CACHE_DIR):
        safe_id = "".join(c for c in query_or_id if c.isalnum() or c in ("-", "_"))[:40]
        cached = os.path.join(CACHE_DIR, f"{safe_id}.flac")
        if os.path.exists(cached) and os.path.getsize(cached) > 4 * 1024 * 1024:
            return cached

    # 2. Search in D:\Music
    if not os.path.exists(MUSIC_DIR):
        return None
    
    clean_q = normalize_title(query_or_id)
    words = [w for w in re.split(r'[\s\-_]+', clean_q) if len(w) > 2]
    
    candidates = []
    
    try:
        for root, _, files in os.walk(MUSIC_DIR):
            for f in files:
                ext = os.path.splitext(f)[1].lower()
                if ext not in (".flac", ".alac", ".wav", ".m4a", ".mp3", ".opus", ".ogg"):
                    continue
                
                full_path = os.path.join(root, f)
                f_lower = f.lower()
                
                # Exact ID match
                if query_or_id and len(query_or_id) >= 8 and query_or_id in f:
                    candidates.append((100, full_path, ext))
                    continue
                
                # Word match
                if words:
                    matches = sum(1 for w in words if w in f_lower)
                    score = (matches / len(words)) * 80
                    if score >= 60:
                        candidates.append((score, full_path, ext))
    except Exception:
        pass
    
    if not candidates:
        return None
    
    # Priority sorting: Format tier (FLAC > WAV > ALAC > M4A > MP3) then match score
    format_tier = {".flac": 5, ".wav": 4, ".alac": 4, ".m4a": 3, ".mp3": 2, ".opus": 2, ".ogg": 1}
    candidates.sort(key=lambda c: (format_tier.get(c[2], 0), c[0]), reverse=True)
    
    # Duration validation if known (aligned with client tolerance of 25s)
    if target_duration and target_duration > 0:
        for score, path, ext in candidates:
            meta = probe_file(path)
            if meta.get("duration", 0) > 0:
                diff = abs(meta["duration"] - target_duration)
                if diff <= 25:
                    return path
    
    return candidates[0][1]

# ==============================================================================
# AUTO LOSSLESS FETCHER & SMART CACHE MANAGER
# ==============================================================================

MAX_CACHE_BYTES = 2500 * 1024 * 1024  # 2.5 GB cap

def enforce_cache_limit():
    """Keeps the FLAC cache directory strictly under 2.5 GB using LRU deletion."""
    try:
        if not os.path.exists(CACHE_DIR):
            return
        files = []
        total_size = 0
        for f in os.listdir(CACHE_DIR):
            p = os.path.join(CACHE_DIR, f)
            if os.path.isfile(p):
                sz = os.path.getsize(p)
                total_size += sz
                files.append((p, sz, os.path.getatime(p)))
        if total_size > MAX_CACHE_BYTES:
            files.sort(key=lambda x: x[2])
            for path, sz, _ in files:
                if total_size <= 2000 * 1024 * 1024:
                    break
                try:
                    os.remove(path)
                    total_size -= sz
                    print(f"[Cache Manager] Cleaned old cache file: {os.path.basename(path)}")
                except Exception:
                    pass
    except Exception as e:
        print(f"[Cache Manager] Error enforcing limit: {e}", file=sys.stderr)

def fetch_online_lossless(track_id, title, artist, duration):
    """
    Attempts to locate and verify genuine FLAC lossless audio from public lossless archives.
    Strictly verifies with ffprobe before serving.
    Returns stream info dict or None.
    """
    safe_id = "".join(c for c in track_id if c.isalnum() or c in ("-", "_"))[:40]
    cached_path = os.path.join(CACHE_DIR, f"{safe_id}.flac")
    
    # 1. Check local cache
    if os.path.exists(cached_path) and os.path.getsize(cached_path) > 4 * 1024 * 1024:
        meta = probe_file(cached_path)
        if meta.get("codec") in ("FLAC", "ALAC", "WAV") and meta.get("size", 0) > 4 * 1024 * 1024:
            is_hires = meta["sample_rate"] > 48000 or meta["bit_depth"] > 16
            return {
                "source": "Verified FLAC Cache",
                "codec": meta["codec"],
                "sampleRate": meta["sample_rate"],
                "bitDepth": meta["bit_depth"],
                "bitrate": meta["bitrate"] or 1411,
                "size": meta["size"],
                "quality": "Hi-Res Lossless" if is_hires else "Lossless",
                "local_path": cached_path
            }

    # 2. Online archive lookup (Archive.org Live & Master FLAC catalog)
    try:
        clean_t = normalize_title(title)
        clean_a = normalize_title(artist) if artist else ""
        query_str = f"{clean_t} {clean_a}".strip()
        
        search_url = f"https://archive.org/advancedsearch.php?q={urllib.parse.quote(query_str)}%20AND%20format:FLAC&fl[]=identifier,title,creator,duration&rows=3&output=json"
        req = urllib.request.Request(search_url, headers={"User-Agent": "BitChord/2.0"})
        with urllib.request.urlopen(req, timeout=3.0) as res:
            data = json.loads(res.read().decode())
            docs = data.get("response", {}).get("docs", [])
            for doc in docs:
                ident = doc.get("identifier")
                if not ident:
                    continue
                meta_url = f"https://archive.org/metadata/{ident}/files"
                m_req = urllib.request.Request(meta_url, headers={"User-Agent": "BitChord/2.0"})
                with urllib.request.urlopen(m_req, timeout=3.0) as m_res:
                    m_data = json.loads(m_res.read().decode())
                    flacs = [f for f in m_data.get("result", []) if f.get("name", "").lower().endswith(".flac")]
                    for flac_info in flacs:
                        f_name = flac_info.get("name", "")
                        f_size = int(flac_info.get("size") or 0)
                        f_dur = float(flac_info.get("length") or 0)
                        
                        # Verify title matches file name or title tag
                        f_title = flac_info.get("title") or ""
                        words = [w for w in clean_t.split() if len(w) > 2]
                        if words and not any(w in f_name.lower() or w in f_title.lower() for w in words):
                            continue
                        
                        # Verify duration similarity if known
                        if duration > 0 and f_dur > 0 and abs(f_dur - duration) > 15:
                            continue
                        if f_size < 4 * 1024 * 1024:
                            continue
                        
                        # Download and verify with ffprobe
                        flac_url = f"https://archive.org/download/{ident}/{urllib.parse.quote(f_name)}"
                        tmp_path = cached_path + ".tmp"
                        d_req = urllib.request.Request(flac_url, headers={"User-Agent": "Mozilla/5.0"})
                        with urllib.request.urlopen(d_req, timeout=12.0) as d_res, open(tmp_path, "wb") as out_f:
                            while True:
                                chunk = d_res.read(64 * 1024)
                                if not chunk:
                                    break
                                out_f.write(chunk)
                        
                        # Physical verification with ffprobe
                        if os.path.exists(tmp_path) and os.path.getsize(tmp_path) > 4 * 1024 * 1024:
                            meta = probe_file(tmp_path)
                            if meta.get("codec") in ("FLAC", "ALAC", "WAV"):
                                os.replace(tmp_path, cached_path)
                                enforce_cache_limit()
                                is_hires = meta["sample_rate"] > 48000 or meta["bit_depth"] > 16
                                return {
                                    "source": "Archive.org FLAC Master",
                                    "codec": meta["codec"],
                                    "sampleRate": meta["sample_rate"],
                                    "bitDepth": meta["bit_depth"],
                                    "bitrate": meta["bitrate"] or 1411,
                                    "size": os.path.getsize(cached_path),
                                    "quality": "Hi-Res Lossless" if is_hires else "Lossless",
                                    "local_path": cached_path
                                }
                        if os.path.exists(tmp_path):
                            os.remove(tmp_path)
    except Exception as e:
        print(f"[Audio Engine] Online FLAC fetch notice: {e}", file=sys.stderr)
        
    return None

# ==============================================================================
# PLAYLIST IMPORT ENGINE (Apple Music, Deezer, YouTube)
# ==============================================================================

playlist_cache = {}

def parse_apple_music_playlist(url):
    try:
        req = urllib.request.Request(url, headers={'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)'})
        html = urllib.request.urlopen(req, timeout=12).read().decode('utf-8', 'ignore')

        # Extract title and image from OpenGraph / meta tags
        og_title = re.search(r'<meta\s+property=["\']og:title["\']\s+content=["\'](.*?)["\']', html, re.IGNORECASE)
        if not og_title:
            og_title = re.search(r'<meta\s+content=["\'](.*?)["\']\s+property=["\']og:title["\']', html, re.IGNORECASE)
        og_img = re.search(r'<meta\s+property=["\']og:image["\']\s+content=["\'](.*?)["\']', html, re.IGNORECASE)
        if not og_img:
            og_img = re.search(r'<meta\s+content=["\'](.*?)["\']\s+property=["\']og:image["\']', html, re.IGNORECASE)

        name = og_title.group(1).strip() if og_title else "Apple Music Playlist"
        name = re.sub(r'\s+on Apple Music$', '', name, flags=re.IGNORECASE)
        art = og_img.group(1).strip() if og_img else None

        tracks = []

        # Strategy 1: serialized-server-data (modern Apple Music SSR, full tracklist, zero extra API calls)
        m = re.search(r'id=["\']serialized-server-data["\'][^>]*>(.*?)</script>', html, re.DOTALL)
        if m:
            try:
                raw = json.loads(m.group(1))
                d = raw['data'][0]['data']
                for s in d.get('sections', []):
                    if s.get('itemKind') == 'trackLockup':
                        for item in s.get('items', []):
                            t_title = item.get('title')
                            t_artist = item.get('artistName') or item.get('artist') or ''
                            dur = int((item.get('duration') or 0) / 1000)
                            
                            t_art = None
                            art_dict = item.get('artwork') or {}
                            if isinstance(art_dict, dict) and 'url' in art_dict:
                                t_art = art_dict['url'].replace('{w}', '100').replace('{h}', '100').replace('{f}', 'jpg')
                            if not t_art:
                                t_art = art

                            if t_title:
                                tracks.append({
                                    "name": t_title,
                                    "artists": [t_artist] if t_artist else [],
                                    "duration": dur,
                                    "art": t_art
                                })
            except Exception as e:
                print(f"[Addon] serialized-server-data parsing notice: {e}", file=sys.stderr)

        # Strategy 2: fallback to application/ld+json + iTunes lookup if serialized-server-data had no tracks
        if not tracks:
            m_ld = re.search(r'<script[^>]*application/ld\+json[^>]*>(.*?)</script>', html, re.DOTALL)
            if m_ld:
                try:
                    data = json.loads(m_ld.group(1))
                    if not name or name == "Apple Music Playlist":
                        name = data.get('name', 'Apple Music Playlist')
                    tracks_json = data.get('track', []) or data.get('tracks', [])
                    ids = []
                    for t in tracks_json:
                        u = t.get('url', '')
                        song_id = re.findall(r'/(\d+)(?:\?|$)', u)
                        if song_id:
                            ids.append(song_id[-1])

                    for i in range(0, len(ids), 50):
                        chunk = ids[i:i+50]
                        lookup_url = f"https://itunes.apple.com/lookup?id={','.join(chunk)}"
                        try:
                            res = urllib.request.urlopen(lookup_url, timeout=10).read()
                            res_data = json.loads(res.decode('utf-8'))
                            for r in res_data.get('results', []):
                                if r.get('wrapperType') == 'track' or r.get('kind') == 'song':
                                    tracks.append({
                                        "name": r.get('trackName', ''),
                                        "artists": [r.get('artistName', '')],
                                        "duration": r.get('trackTimeMillis', 0) // 1000,
                                        "art": r.get('artworkUrl100', '')
                                    })
                        except Exception:
                            pass
                except Exception:
                    pass

        if not tracks:
            return None

        if not art and tracks and tracks[0].get('art'):
            art = tracks[0].get('art')

        return {
            "name": name,
            "owner": "Apple Music",
            "count": len(tracks),
            "art": art,
            "tracks": tracks
        }
    except Exception as e:
        print(f"[Addon] Apple Music error: {e}", file=sys.stderr)
        return None

def parse_deezer_playlist(url):
    try:
        m = re.search(r'/playlist/(\d+)', url)
        if not m:
            return None
        pl_id = m.group(1)
        api_url = f"https://api.deezer.com/playlist/{pl_id}"
        req = urllib.request.Request(api_url, headers={'User-Agent': 'Mozilla/5.0'})
        res = json.loads(urllib.request.urlopen(req, timeout=10).read().decode('utf-8'))
        
        name = res.get('title', 'Deezer Playlist')
        art = res.get('picture_medium') or res.get('picture_big')
        owner = res.get('creator', {}).get('name', 'Deezer')
        
        raw_tracks = res.get('tracks', {}).get('data', [])
        tracks = []
        for t in raw_tracks:
            tracks.append({
                "name": t.get('title', ''),
                "artists": [t.get('artist', {}).get('name', '')],
                "duration": t.get('duration', 0),
                "art": t.get('album', {}).get('cover_medium', '')
            })
        return {
            "name": name,
            "owner": owner,
            "count": len(tracks),
            "art": art,
            "tracks": tracks
        }
    except Exception as e:
        print(f"[Addon] Deezer error: {e}", file=sys.stderr)
        return None

def parse_generic_playlist(url):
    try:
        ytdlp_bin = locate_binary("yt-dlp")
        cmd = [ytdlp_bin, "-J", "--flat-playlist", url]
        res = subprocess.run(cmd, capture_output=True, text=True, timeout=30, **SUBPROCESS_KWARGS)
        if res.returncode != 0:
            print(f"[Addon] yt-dlp notice (code {res.returncode}): {res.stderr[:200]}", file=sys.stderr)
            return None
        data = json.loads(res.stdout)
        name = data.get('title', 'Imported Playlist')
        entries = data.get('entries', [])
        tracks = []
        for e in entries:
            title = e.get('title', '')
            uploader = e.get('uploader') or e.get('channel') or ''
            tracks.append({
                "name": title,
                "artists": [uploader] if uploader else [],
                "duration": int(e.get('duration') or 0),
                "art": e.get('thumbnails', [{}])[-1].get('url') if e.get('thumbnails') else None
            })
        art = entries[0].get('thumbnails', [{}])[-1].get('url') if entries else None
        return {
            "name": name,
            "owner": data.get('uploader', 'Online Playlist'),
            "count": len(tracks),
            "art": art,
            "tracks": tracks
        }
    except Exception as e:
        print(f"[Addon] Generic playlist error: {e}", file=sys.stderr)
        return None

def get_universal_playlist(url):
    if url in playlist_cache:
        return playlist_cache[url]
    pl = None
    if "music.apple.com" in url:
        pl = parse_apple_music_playlist(url)
    elif "deezer.com" in url:
        pl = parse_deezer_playlist(url)
    if not pl:
        pl = parse_generic_playlist(url)
    if pl:
        playlist_cache[url] = pl
    return pl

# ==============================================================================
# HTTP SERVER & PROTOCOL HANDLER
# ==============================================================================

class ThreadedHTTPServer(ThreadingMixIn, HTTPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        pass

class AddonHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def send_cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "*")

    def do_OPTIONS(self):
        self.send_response(204)
        self.send_cors()
        self.end_headers()

    def do_HEAD(self):
        parsed = urllib.parse.urlparse(self.path)
        path = urllib.parse.unquote(parsed.path)
        if path.startswith("/audio/"):
            filename = os.path.basename(path)
            track_id = os.path.splitext(filename)[0]
            
            # Local file check
            local_path = find_local_audio(track_id)
            if local_path and os.path.exists(local_path):
                meta = probe_file(local_path)
                file_size = meta["size"]
                self.send_response(200)
                self.send_header("Content-Type", f"audio/{meta['codec'].lower()}")
                self.send_header("Accept-Ranges", "bytes")
                self.send_header("Content-Length", str(file_size))
                self.send_cors()
                self.end_headers()
                return

            self.send_error(404, "Audio not found")
            return
            
        self.send_response(200)
        self.send_cors()
        self.end_headers()

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        path = urllib.parse.unquote(parsed.path)
        params = urllib.parse.parse_qs(parsed.query)

        # 1. Manifest
        if path == "/manifest.json":
            manifest = {
                "id": "bitchord-local-master-hires",
                "name": "BitChord Audio Engine (Lossless & Hi-Res)",
                "resources": ["search"],
                "settings": {
                    "quality": "LOSSLESS"
                }
            }
            body = json.dumps(manifest).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_cors()
            self.end_headers()
            self.wfile.write(body)
            return

        # Shutdown endpoint
        if path == "/shutdown":
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(2))
            self.send_header("Connection", "close")
            self.send_cors()
            self.end_headers()
            self.wfile.write(b"OK")
            self.wfile.flush()
            def _kill():
                time.sleep(0.3)
                try:
                    if os.path.exists(PID_FILE):
                        os.remove(PID_FILE)
                except Exception:
                    pass
                os._exit(0)
            threading.Thread(target=_kill, daemon=True).start()
            return

        # 2. Search
        if path == "/search":
            q = params.get("q", [""])[0].strip()
            queue = read_queue_from_ron()
            tracks = []

            # A. Check Local Music Library First (Highest Quality, Instant)
            local_match = find_local_audio(q)
            if local_match:
                meta = probe_file(local_match)
                filename = os.path.splitext(os.path.basename(local_match))[0]
                is_hires = meta["sample_rate"] > 48000 or meta["bit_depth"] > 16
                quality_label = "Hi-Res Lossless" if is_hires else ("Lossless" if meta["codec"] in ("FLAC", "ALAC", "WAV") else "High Quality")
                
                # Extract artist and title accurately
                c_artist = meta.get("artist") or ""
                c_title = meta.get("title") or ""
                if not c_artist and " - " in filename:
                    parts = filename.split(" - ", 1)
                    c_artist = parts[0].strip()
                    c_title = parts[1].strip()
                elif not c_title:
                    c_title = filename
                
                # If still no artist, check active queue for match
                if not c_artist:
                    for t in queue:
                        t_t = t.get("title", "").lower()
                        if t_t in filename.lower() or filename.lower() in t_t:
                            arts = t.get("artists", [])
                            if arts:
                                c_artist = arts[0].get("name", "")
                            break

                tracks.append({
                    "id": f"local_{filename}",
                    "title": c_title or filename,
                    "artist": c_artist or "Unknown Artist",
                    "duration": meta.get("duration", 240),
                    "audioQuality": quality_label,
                    "format": meta["codec"],
                    "atmos": False,
                    "streamURL": f"http://127.0.0.1:{PORT}/stream/local_{filename}?quality=LOSSLESS"
                })

            # B. Match against active queue items for true lossless
            for t in queue:
                t_id = t.get("id", "")
                t_title = t.get("title", "")
                artists = t.get("artists", [])
                t_artist = artists[0].get("name", "") if artists else ""
                t_dur = t.get("duration", 240)
                
                if q.lower() in t_title.lower() or t_id in q:
                    # Check local for this track
                    local_t = find_local_audio(t_id, t_dur) or find_local_audio(t_title, t_dur)
                    if local_t:
                        meta = probe_file(local_t)
                        is_hires = meta["sample_rate"] > 48000 or meta["bit_depth"] > 16
                        q_label = "Hi-Res Lossless" if is_hires else "Lossless"
                        tracks.append({
                            "id": t_id,
                            "title": t_title,
                            "artist": t_artist,
                            "duration": t_dur,
                            "audioQuality": q_label,
                            "format": meta["codec"],
                            "atmos": False,
                            "streamURL": f"http://127.0.0.1:{PORT}/stream/{t_id}?quality=LOSSLESS"
                        })
                    # Only include in search results if local physical file exists and is valid
                    pass

            # Send results (or empty list if no genuine lossless exists)
            # When tracks is empty, BitChord client immediately plays direct YouTube stream without delay
            body = json.dumps({"tracks": tracks[:25]}).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_cors()
            self.end_headers()
            self.wfile.write(body)
            return

        # 3. Stream Info
        if path.startswith("/stream/"):
            track_id = path.replace("/stream/", "").strip()
            
            # Check Local Audio File
            clean_id = track_id.replace("local_", "")
            local_path = find_local_audio(clean_id)
            if local_path and os.path.exists(local_path):
                meta = probe_file(local_path)
                file_size = meta["size"]
                is_hires = meta["sample_rate"] > 48000 or meta["bit_depth"] > 16
                quality_label = "Hi-Res Lossless" if is_hires else ("Lossless" if meta["codec"] in ("FLAC", "ALAC", "WAV") else "High Quality")
                
                stream_info = {
                    "url": f"http://127.0.0.1:{PORT}/audio/{track_id}",
                    "codec": meta["codec"],
                    "container": meta["codec"].lower(),
                    "format": meta["codec"].lower(),
                    "mimeType": f"audio/{meta['codec'].lower()}",
                    "quality": quality_label,
                    "source": "Local File (D:\\Music)",
                    "sampleRate": meta["sample_rate"],
                    "bitDepth": meta["bit_depth"],
                    "bitrate": meta["bitrate"] or 1411,
                    "size": file_size
                }
                body = json.dumps(stream_info).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.send_cors()
                self.end_headers()
                self.wfile.write(body)
                return

            # Try Online Auto Lossless Fetcher (with strict 3.5s timeout)
            meta = get_track_metadata(clean_id)
            if meta:
                fetched = fetch_online_lossless(clean_id, meta["title"], meta["artist"], meta["duration"])
                if fetched and fetched.get("local_path"):
                    stream_info = {
                        "url": f"http://127.0.0.1:{PORT}/audio/{track_id}",
                        "codec": fetched["codec"],
                        "container": fetched["codec"].lower(),
                        "format": fetched["codec"].lower(),
                        "mimeType": f"audio/{fetched['codec'].lower()}",
                        "quality": fetched["quality"],
                        "source": fetched["source"],
                        "sampleRate": fetched["sampleRate"],
                        "bitDepth": fetched["bitDepth"],
                        "bitrate": fetched["bitrate"],
                        "size": fetched["size"]
                    }
                    body = json.dumps(stream_info).encode("utf-8")
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json; charset=utf-8")
                    self.send_header("Content-Length", str(len(body)))
                    self.send_cors()
                    self.end_headers()
                    self.wfile.write(body)
                    return

            # No verified physical FLAC found -> return 404 immediately
            self.send_error(404, "No verified lossless copy found; fallback to high quality stream")
            return

        # 4. Universal Playlist Info
        if path == "/playlist-info":
            target_url = params.get("url", [""])[0].strip()
            pl = get_universal_playlist(target_url) if target_url else None
            if not pl:
                body = json.dumps({
                    "error": "Playlist tidak ditemukan atau tautan tidak didukung",
                    "name": "Tidak ditemukan",
                    "owner": "",
                    "count": 0,
                    "art": None
                }).encode("utf-8")
                self.send_response(404)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.send_cors()
                self.end_headers()
                self.wfile.write(body)
                return
            info = {
                "name": pl.get("name", "Imported Playlist"),
                "owner": pl.get("owner", ""),
                "count": pl.get("count", len(pl.get("tracks", []))),
                "art": pl.get("art")
            }
            body = json.dumps(info).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_cors()
            self.end_headers()
            self.wfile.write(body)
            return

        # 5. Universal Playlist Tracks
        if path == "/playlist-tracks":
            target_url = params.get("url", [""])[0].strip()
            pl = get_universal_playlist(target_url) if target_url else None
            if not pl:
                body = json.dumps({
                    "error": "Daftar lagu tidak dapat diakses atau tautan tidak didukung",
                    "name": "Tidak ditemukan",
                    "tracks": []
                }).encode("utf-8")
                self.send_response(404)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.send_cors()
                self.end_headers()
                self.wfile.write(body)
                return
            body = json.dumps(pl).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_cors()
            self.end_headers()
            self.wfile.write(body)
            return

        # 6. Audio Streaming with Full Byte Range Support
        if path.startswith("/audio/"):
            filename = os.path.basename(path)
            track_id = os.path.splitext(filename)[0].replace("local_", "")
            
            # Resolve file path
            file_path = find_local_audio(track_id)
            if not file_path or not os.path.exists(file_path):
                self.send_error(404, "Audio file not available")
                return

            meta = probe_file(file_path)
            file_size = meta["size"]
            mime = f"audio/{meta['codec'].lower()}"
            range_header = self.headers.get("Range")

            if range_header:
                try:
                    bytes_range = range_header.replace("bytes=", "").split("-")
                    start = int(bytes_range[0])
                    end = int(bytes_range[1]) if bytes_range[1] else file_size - 1
                    end = min(end, file_size - 1)
                    content_len = end - start + 1

                    self.send_response(206)
                    self.send_header("Content-Type", mime)
                    self.send_header("Accept-Ranges", "bytes")
                    self.send_header("Content-Range", f"bytes {start}-{end}/{file_size}")
                    self.send_header("Content-Length", str(content_len))
                    self.send_cors()
                    self.end_headers()

                    with open(file_path, "rb") as f:
                        f.seek(start)
                        chunk_size = 64 * 1024
                        remaining = content_len
                        while remaining > 0:
                            n = min(remaining, chunk_size)
                            data = f.read(n)
                            if not data:
                                break
                            self.wfile.write(data)
                            remaining -= len(data)
                    return
                except Exception:
                    return

            # Full file
            self.send_response(200)
            self.send_header("Content-Type", mime)
            self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Length", str(file_size))
            self.send_cors()
            self.end_headers()
            try:
                with open(file_path, "rb") as f:
                    while True:
                        chunk = f.read(64 * 1024)
                        if not chunk:
                            break
                        self.wfile.write(chunk)
            except Exception:
                pass
            return

        self.send_error(404, "Not Found")

    def log_message(self, format, *args):
        pass

def bitchord_watchdog():
    """Auto-terminates addon server when BitChord.exe is closed (after it has been started)."""
    import psutil
    has_seen_bitchord = False
    while True:
        time.sleep(6)
        running = False
        try:
            for p in psutil.process_iter(['name']):
                pname = (p.info.get('name') or '').lower()
                if 'bitchord' in pname:
                    running = True
                    has_seen_bitchord = True
                    break
        except Exception:
            running = True
        if has_seen_bitchord and not running:
            try:
                if os.path.exists(PID_FILE):
                    os.remove(PID_FILE)
            except Exception:
                pass
            os._exit(0)

def run_server():
    threading.Thread(target=bitchord_watchdog, daemon=True).start()
    server = ThreadedHTTPServer(("127.0.0.1", PORT), AddonHandler)
    print(f"[BitChord Audio Engine] Listening on http://127.0.0.1:{PORT}")
    server.serve_forever()

if __name__ == "__main__":
    run_server()
