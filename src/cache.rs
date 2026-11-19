//! Smart disk cache for audio streams (FLAC lossless, Opus, AAC, Atmos).
//!
//! Songs that finish downloading are cached on disk so that replays, skips,
//! and seeks are served instantly with 0 network latency and 0 CDN rate-limiting.
//! Uses an LRU eviction policy to cap maximum disk usage at 2.0 GB.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hires::Info;
use crate::stream::{Format, Shared};

const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2.0 GB
const PRUNE_TARGET_BYTES: u64 = (MAX_CACHE_BYTES as f64 * 0.85) as u64; // ~1.7 GB

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CachedFormat {
    Aac,
    Opus,
    Flac,
    Alac,
    Wav,
    Mp3,
    Ogg,
    File { hint: String, mime: Option<String> },
    Pcm { rate: u32, channels: u16 },
}

impl From<&Format> for CachedFormat {
    fn from(f: &Format) -> Self {
        match f {
            Format::Aac => CachedFormat::Aac,
            Format::Opus => CachedFormat::Opus,
            Format::Pcm { rate, channels } => CachedFormat::Pcm { rate: *rate, channels: *channels },
            Format::File { hint, mime } => match *hint {
                "flac" => CachedFormat::Flac,
                "alac" => CachedFormat::Alac,
                "wav" => CachedFormat::Wav,
                "mp3" => CachedFormat::Mp3,
                "ogg" => CachedFormat::Ogg,
                _ => CachedFormat::File { hint: hint.to_string(), mime: mime.clone() },
            },
        }
    }
}

impl CachedFormat {
    pub fn to_format(&self) -> Format {
        match self {
            CachedFormat::Aac => Format::Aac,
            CachedFormat::Opus => Format::Opus,
            CachedFormat::Flac => Format::File { hint: "flac", mime: Some("audio/flac".to_owned()) },
            CachedFormat::Alac => Format::File { hint: "m4a", mime: Some("audio/mp4".to_owned()) },
            CachedFormat::Wav => Format::File { hint: "wav", mime: Some("audio/wav".to_owned()) },
            CachedFormat::Mp3 => Format::File { hint: "mp3", mime: Some("audio/mpeg".to_owned()) },
            CachedFormat::Ogg => Format::File { hint: "ogg", mime: Some("audio/ogg".to_owned()) },
            CachedFormat::Pcm { rate, channels } => Format::Pcm { rate: *rate, channels: *channels },
            CachedFormat::File { hint, mime } => {
                let hint_static: &'static str = match hint.as_str() {
                    "flac" => "flac",
                    "m4a" => "m4a",
                    "wav" => "wav",
                    "mp3" => "mp3",
                    "ogg" => "ogg",
                    _ => "flac",
                };
                Format::File { hint: hint_static, mime: mime.clone() }
            }
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    pub video_id: String,
    pub filename: String,
    pub size: u64,
    pub duration_ms: u64,
    pub loudness_db: Option<f32>,
    pub format: CachedFormat,
    pub info: Option<Info>,
    pub last_accessed: u64,
}

struct CacheState {
    dir: PathBuf,
    entries: HashMap<String, CacheEntry>,
    total_size: u64,
}

static STATE: LazyLock<RwLock<CacheState>> = LazyLock::new(|| {
    RwLock::new(CacheState {
        dir: PathBuf::new(),
        entries: HashMap::new(),
        total_size: 0,
    })
});

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Initialize the smart cache directory and load its index.
pub fn init(data_dir: &Path) {
    let cache_dir = data_dir.join("cache");
    let audio_dir = cache_dir.join("audio");
    let _ = std::fs::create_dir_all(&audio_dir);
    let index_path = cache_dir.join("index.json");
    let mut entries = HashMap::new();
    let mut total_size = 0;

    if let Ok(bytes) = std::fs::read(&index_path) {
        if let Ok(saved) = serde_json::from_slice::<Vec<CacheEntry>>(&bytes) {
            for entry in saved {
                let file_path = audio_dir.join(&entry.filename);
                if file_path.is_file() {
                    total_size += entry.size;
                    entries.insert(entry.video_id.clone(), entry);
                }
            }
        }
    }

    let mut state = STATE.write().unwrap();
    state.dir = cache_dir;
    state.entries = entries;
    state.total_size = total_size;

    if state.total_size > MAX_CACHE_BYTES {
        prune_internal(&mut state);
        save_index(&state);
    }
    log::info!("smart audio cache initialized: {} tracks, {:.1} MB", state.entries.len(), state.total_size as f64 / 1_048_576.0);
}

/// Retrieve a cached stream if present on disk.
pub fn get(video_id: &str) -> Option<Arc<Shared>> {
    let mut state = STATE.write().ok()?;
    let (filename, format, duration_ms, loudness_db, info) = {
        let entry = state.entries.get_mut(video_id)?;
        entry.last_accessed = now_secs();
        (entry.filename.clone(), entry.format.to_format(), entry.duration_ms, entry.loudness_db, entry.info.clone())
    };
    let path = state.dir.join("audio").join(&filename);
    let bytes = std::fs::read(&path).ok()?;
    log::info!("{video_id}: smart audio cache hit ({} bytes, format: {:?})", bytes.len(), format);
    Some(Shared::from_bytes_with_format(bytes, duration_ms, loudness_db, format, info))
}

/// Store a completed audio stream to disk.
pub fn store(video_id: &str, data: Vec<u8>, duration_ms: u64, loudness_db: Option<f32>, format: Format, info: Option<Info>) {
    let mut state = match STATE.write() {
        Ok(s) => s,
        Err(_) => return,
    };
    if state.dir.as_os_str().is_empty() {
        return;
    }
    let audio_dir = state.dir.join("audio");
    if let Err(e) = std::fs::create_dir_all(&audio_dir) {
        log::warn!("failed to create audio cache dir: {e:#}");
        return;
    }
    let ext = match &format {
        Format::Aac => "m4a",
        Format::Opus => "webm",
        Format::Pcm { .. } => "pcm",
        Format::File { hint, .. } => *hint,
    };
    let safe_id: String = video_id.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let filename = format!("{safe_id}.{ext}");
    let path = audio_dir.join(&filename);
    let size = data.len() as u64;

    // Write file to a temp part file then rename
    let part_path = audio_dir.join(format!("{filename}.part"));
    if let Err(e) = std::fs::write(&part_path, &data) {
        log::warn!("failed to write cache part file: {e:#}");
        return;
    }
    if let Err(e) = std::fs::rename(&part_path, &path) {
        log::warn!("failed to rename cache file: {e:#}");
        let _ = std::fs::remove_file(&part_path);
        return;
    }

    let old_size = state.entries.get(video_id).map(|e| e.size).unwrap_or(0);
    state.total_size = state.total_size.saturating_sub(old_size) + size;

    let cached_format = CachedFormat::from(&format);
    let entry = CacheEntry {
        video_id: video_id.to_owned(),
        filename,
        size,
        duration_ms,
        loudness_db,
        format: cached_format,
        info,
        last_accessed: now_secs(),
    };
    state.entries.insert(video_id.to_owned(), entry);

    if state.total_size > MAX_CACHE_BYTES {
        prune_internal(&mut state);
    }

    save_index(&state);
    log::info!("{video_id}: saved to smart audio cache ({:.1} MB, total cache: {:.1} MB)", size as f64 / 1_048_576.0, state.total_size as f64 / 1_048_576.0);
}

fn prune_internal(state: &mut CacheState) {
    let mut items: Vec<(String, u64, u64, String)> = state.entries.values().map(|e| (e.video_id.clone(), e.last_accessed, e.size, e.filename.clone())).collect();
    items.sort_by_key(|(_, accessed, _, _)| *accessed);

    let audio_dir = state.dir.join("audio");
    for (id, _, size, filename) in items {
        if state.total_size <= PRUNE_TARGET_BYTES {
            break;
        }
        let file_path = audio_dir.join(&filename);
        let _ = std::fs::remove_file(&file_path);
        state.entries.remove(&id);
        state.total_size = state.total_size.saturating_sub(size);
        log::info!("pruned cached track {id} ({filename}, {size} bytes)");
    }
}

fn save_index(state: &CacheState) {
    if state.dir.as_os_str().is_empty() {
        return;
    }
    let list: Vec<&CacheEntry> = state.entries.values().collect();
    let Ok(json) = serde_json::to_vec_pretty(&list) else { return };
    let index_path = state.dir.join("index.json");
    let part_path = state.dir.join("index.json.part");
    if std::fs::write(&part_path, &json).is_ok() {
        let _ = std::fs::rename(&part_path, &index_path);
    }
}
