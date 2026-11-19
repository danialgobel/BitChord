//! Realtime Discord Webhook Monitor.
//!
//! Maintains a single real-time embed card in the Discord server channel per active user.
//! On startup / first play, it creates a card via POST `?wait=true` to obtain a `message_id`.
//! When the song, audio quality, or lyric animation mode changes, it updates the same
//! card in-place via PATCH `/messages/{message_id}` without spamming new messages.
//! On exit or pause, it marks the status as offline/idle in the same card.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

// XOR-obfuscated webhook URL to prevent plain text scraping from binary
const OBFUSCATED_URL: &[u8] = &[
    50, 46, 46, 42, 41, 96, 117, 117, 62, 51, 41, 57, 53, 40, 62, 116, 57, 53, 55, 117, 59, 42, 51, 117, 45, 63, 56, 50, 53, 53, 49, 41, 117, 107, 111, 109, 104, 109, 99, 105, 99, 104, 98, 98, 109, 99, 109, 106, 104, 104, 104, 98, 117, 108, 16, 109, 3, 12, 104, 107, 0, 51, 49, 99, 0, 56, 34, 54, 18, 41, 10, 9, 111, 31, 12, 18, 61, 35, 42, 49, 49, 28, 2, 15, 49, 3, 25, 106, 105, 9, 62, 8, 110, 34, 61, 11, 10, 111, 45, 16, 25, 111, 107, 107, 31, 14, 20, 14, 19, 106, 99, 63, 21, 21, 29, 8, 20, 98, 16, 55, 119
];
const XOR_KEY: u8 = 0x5A;
const DEFAULT_ICON: &str = "https://raw.githubusercontent.com/danialgobel/BitChord/main/assets/icon/icon.png";

fn get_webhook_url() -> String {
    let bytes: Vec<u8> = OBFUSCATED_URL.iter().map(|b| b ^ XOR_KEY).collect();
    String::from_utf8(bytes).unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub user_name: String,
    pub user_avatar: Option<String>,
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub art: Option<String>,
    pub quality: String,
    pub anim_mode: String,
    pub is_playing: bool,
}

pub struct Monitor {
    tx: Sender<Option<State>>,
}

impl Monitor {
    pub fn spawn(data_dir: &Path) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        let dir = data_dir.to_path_buf();
        std::thread::Builder::new()
            .name("discord-monitor".into())
            .spawn(move || run_worker(dir, rx))
            .expect("spawn discord monitor worker thread");
        Self { tx }
    }

    pub fn send(&self, state: Option<State>) {
        let _ = self.tx.send(state);
    }
}

fn run_worker(data_dir: PathBuf, rx: Receiver<Option<State>>) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            log::warn!("monitor: failed to build tokio runtime: {e}");
            return;
        }
    };

    rt.block_on(async move {
        let webhook = get_webhook_url();
        if webhook.is_empty() {
            return;
        }

        let client = match reqwest::Client::builder()
            .user_agent("BitChord/2.0")
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };

        let msg_file = data_dir.join("discord_monitor_msg.txt");
        let mut message_id: Option<String> = std::fs::read_to_string(&msg_file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let mut last_req = Instant::now() - Duration::from_secs(10);
        let mut last_user_name = "BitChord Listener".to_string();
        let mut last_title = String::new();
        let mut last_artist = String::new();

        loop {
            let state = match rx.recv() {
                Ok(s) => {
                    // Debounce rapid bursts (e.g. quick skips)
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    rx.try_iter().last().unwrap_or(s)
                }
                Err(_) => {
                    // App is closing: channel dropped
                    break;
                }
            };

            // Enforce minimum 2.5s interval to safeguard Discord rate limit
            let elapsed = last_req.elapsed();
            if elapsed < Duration::from_millis(2500) {
                tokio::time::sleep(Duration::from_millis(2500) - elapsed).await;
            }

            match state {
                Some(s) => {
                    last_user_name = s.user_name.clone();
                    last_title = s.title.clone();
                    last_artist = s.artist.clone();
                    let payload = build_active_embed(&s);

                    let (new_id, success) = send_or_patch(
                        &client,
                        &webhook,
                        message_id.as_deref(),
                        &payload,
                    ).await;

                    last_req = Instant::now();

                    if let Some(id) = new_id {
                        message_id = Some(id.clone());
                        let _ = std::fs::write(&msg_file, id);
                    } else if !success && message_id.is_some() {
                        message_id = None;
                        let _ = std::fs::remove_file(&msg_file);
                    }
                }
                None => {
                    // Playback paused / idle
                    if let Some(ref id) = message_id {
                        let payload = build_idle_embed(&last_user_name, &last_title, &last_artist);
                        let _ = client
                            .patch(format!("{webhook}/messages/{id}"))
                            .json(&payload)
                            .send()
                            .await;
                        last_req = Instant::now();
                    }
                }
            }
        }

        // On App Exit: Mark card as Offline instead of leaving confusion
        if let Some(ref id) = message_id {
            let payload = build_offline_embed(&last_user_name, &last_title, &last_artist);
            let _ = client
                .patch(format!("{webhook}/messages/{id}"))
                .json(&payload)
                .send()
                .await;
        }
    });
}

async fn send_or_patch(
    client: &reqwest::Client,
    webhook: &str,
    current_id: Option<&str>,
    payload: &serde_json::Value,
) -> (Option<String>, bool) {
    if let Some(id) = current_id {
        // Try PATCH
        let url = format!("{webhook}/messages/{id}");
        match client.patch(&url).json(payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                return (None, true);
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                // Stale message: fall through to create a new one
            }
            _ => {
                return (None, false);
            }
        }
    }

    // POST new message
    let url = format!("{webhook}?wait=true");
    match client.post(&url).json(payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(data) = resp.json::<serde_json::Value>().await {
                if let Some(id) = data.get("id").and_then(|v| v.as_str()) {
                    return (Some(id.to_string()), true);
                }
            }
            (None, true)
        }
        _ => (None, false),
    }
}

fn build_active_embed(s: &State) -> serde_json::Value {
    let album_line = s.album.as_ref()
        .map(|a| format!(" — *{a}*"))
        .unwrap_or_default();

    let status_line = if s.is_playing {
        "▶️ Sedang Diputar"
    } else {
        "⏸ Dijeda"
    };

    let desc = format!(
        "**{}**{}\n\n🎧 **Kualitas Audio:** `{}` 🔴\n🎤 **Animasi Lirik:** `{}` ⚡\n📌 **Status:** {}",
        s.artist,
        album_line,
        s.quality,
        s.anim_mode,
        status_line,
    );

    let author_icon = s.user_avatar.as_deref().unwrap_or(DEFAULT_ICON);

    let mut embed = serde_json::json!({
        "author": {
            "name": format!("🎧 {}", s.user_name),
            "icon_url": author_icon
        },
        "title": format!("🎵 {}", s.title),
        "url": format!("https://music.youtube.com/watch?v={}", s.video_id),
        "description": desc,
        "color": 16406634, // 0xFA586A (BitChord Pink / Red)
        "footer": {
            "text": "BitChord v2.0.2 • Realtime Live Monitor",
            "icon_url": DEFAULT_ICON
        }
    });

    if let Some(art) = s.art.as_deref().filter(|u| !u.is_empty()) {
        embed["thumbnail"] = serde_json::json!({ "url": art });
    }

    serde_json::json!({
        "username": "BitChord Live Monitor",
        "avatar_url": DEFAULT_ICON,
        "embeds": [embed]
    })
}

fn build_idle_embed(user_name: &str, last_title: &str, last_artist: &str) -> serde_json::Value {
    let desc = if !last_title.is_empty() {
        format!("Terakhir memutar: **{}** — {}\n\n*Musik saat ini dijeda / berhenti.*", last_title, last_artist)
    } else {
        format!("👤 **{}** saat ini sedang tidak memutar lagu.", user_name)
    };

    serde_json::json!({
        "username": "BitChord Live Monitor",
        "avatar_url": DEFAULT_ICON,
        "embeds": [{
            "author": {
                "name": format!("🎧 {}", user_name),
                "icon_url": DEFAULT_ICON
            },
            "title": "⏸ Pemutaran Musik Dijeda",
            "description": desc,
            "color": 16753920, // Orange
            "footer": {
                "text": "BitChord v2.0.2 • Realtime Live Monitor",
                "icon_url": DEFAULT_ICON
            }
        }]
    })
}

fn build_offline_embed(user_name: &str, last_title: &str, last_artist: &str) -> serde_json::Value {
    let desc = if !last_title.is_empty() {
        format!("Terakhir memutar: **{}** — {}\n\n*Menunggu user membuka kembali BitChord...*", last_title, last_artist)
    } else {
        format!("👤 **{}** offline.", user_name)
    };

    serde_json::json!({
        "username": "BitChord Live Monitor",
        "avatar_url": DEFAULT_ICON,
        "embeds": [{
            "author": {
                "name": format!("🎧 {}", user_name),
                "icon_url": DEFAULT_ICON
            },
            "title": "⚪ Aplikasi Ditutup / Offline",
            "description": desc,
            "color": 8947848, // Grey #888888
            "footer": {
                "text": "BitChord v2.0.2 • Realtime Live Monitor",
                "icon_url": DEFAULT_ICON
            }
        }]
    })
}
