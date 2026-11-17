# BitChord Addon Server

Companion local microservice for BitChord, providing:
1. **Universal Playlist Import**: Fast playlist parsing for Apple Music (direct SSR server data extraction), YouTube Music, Deezer, and Spotify.
2. **Local Hi-Res / Lossless Audio Streamer**: Streams local audio files (`D:\Music` or `~/Music`) directly into BitChord.

## Requirements
- Python 3.10+
- `psutil` (optional, for process monitoring)
- `yt-dlp` (bundled in binary releases or discovered from PATH)

## Development
Run directly with Python:
```bash
python bitchord_addon_server.py
```

## Compilation (PyInstaller)
To compile a standalone binary for distribution:
```bash
pyinstaller --noconfirm --onedir --windowed --name "bitchord_addon_server" bitchord_addon_server.py
```

