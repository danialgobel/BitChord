//! The multi-platform onboarding and account connection screen:
//! Connect Google & YouTube, Spotify, Apple Music, or continue as Guest.

use egui::{Color32, Rect, RichText, Sense, Ui, pos2, vec2};
use crate::theme;
use super::widgets::{self, pill};
use super::{Action, App, Page};

const APPLE_RED: Color32 = Color32::from_rgb(250, 36, 60);
const SPOTIFY_GREEN: Color32 = Color32::from_rgb(30, 215, 96);
const YOUTUBE_RED: Color32 = Color32::from_rgb(255, 0, 0);

/// Google official 4-color 'G' icon badge
pub fn draw_google_logo(ui: &Ui, rect: Rect) {
    let c = rect.center();
    let s = rect.width() / 24.0;
    ui.painter().rect_filled(rect, rect.width() * 0.27, Color32::from_rgb(255, 255, 255));
    let stroke_w = 2.4 * s;
    let r = 6.8 * s;
    // Center horizontal bar (Blue)
    ui.painter().rect_filled(Rect::from_min_max(pos2(c.x - 0.5 * s, c.y - 1.2 * s), pos2(c.x + r, c.y + 1.2 * s)), 0.0, Color32::from_rgb(66, 133, 244));
    // Red arc (top)
    let p_red: Vec<egui::Pos2> = (0..=8).map(|k| {
        let a = -0.8 + (k as f32 / 8.0) * 1.8;
        pos2(c.x + r * a.cos(), c.y - r * a.sin())
    }).collect();
    ui.painter().add(egui::Shape::line(p_red, egui::Stroke::new(stroke_w, Color32::from_rgb(234, 67, 53))));
    // Yellow arc (left)
    let p_yel: Vec<egui::Pos2> = (0..=8).map(|k| {
        let a = 1.0 + (k as f32 / 8.0) * 1.4;
        pos2(c.x + r * a.cos(), c.y - r * a.sin())
    }).collect();
    ui.painter().add(egui::Shape::line(p_yel, egui::Stroke::new(stroke_w, Color32::from_rgb(251, 188, 5))));
    // Green arc (bottom)
    let p_grn: Vec<egui::Pos2> = (0..=8).map(|k| {
        let a = 2.4 + (k as f32 / 8.0) * 1.5;
        pos2(c.x + r * a.cos(), c.y - r * a.sin())
    }).collect();
    ui.painter().add(egui::Shape::line(p_grn, egui::Stroke::new(stroke_w, Color32::from_rgb(52, 168, 83))));
}

/// Official YouTube play button logo
pub fn draw_youtube_logo(ui: &Ui, rect: Rect) {
    ui.painter().rect_filled(rect, rect.width() * 0.27, YOUTUBE_RED);
    let s = rect.width() / 24.0;
    let c = rect.center();
    let white = Color32::WHITE;
    let tri = vec![
        pos2(c.x - 3.0 * s, c.y - 4.5 * s),
        pos2(c.x - 3.0 * s, c.y + 4.5 * s),
        pos2(c.x + 4.8 * s, c.y),
    ];
    ui.painter().add(egui::Shape::convex_polygon(tri, white, egui::Stroke::NONE));
}

/// Official Spotify badge
pub fn draw_spotify_logo(ui: &Ui, rect: Rect) {
    ui.painter().rect_filled(rect, rect.width() * 0.27, SPOTIFY_GREEN);
    let s = rect.width() / 24.0;
    let c = rect.center();
    let ink = Color32::from_rgb(11, 11, 11);
    for (i, (w, y, curve)) in [(14.0, -3.2, 2.0), (11.6, 0.6, 1.6), (9.0, 4.0, 1.2)].into_iter().enumerate() {
        let a = pos2(c.x - w / 2.0 * s, c.y + y * s);
        let b = pos2(c.x + w / 2.0 * s, c.y + (y + 1.2) * s);
        let mid = pos2(c.x, c.y + (y - curve) * s);
        let pts: Vec<egui::Pos2> = (0..=12)
            .map(|k| {
                let t = k as f32 / 12.0;
                let u = 1.0 - t;
                pos2(u * u * a.x + 2.0 * u * t * mid.x + t * t * b.x, u * u * a.y + 2.0 * u * t * mid.y + t * t * b.y)
            })
            .collect();
        ui.painter().add(egui::Shape::line(pts, egui::Stroke::new((2.3 - 0.25 * i as f32) * s, ink)));
    }
}

/// Official Apple Music badge
pub fn draw_apple_music_logo(ui: &Ui, rect: Rect) {
    ui.painter().rect_filled(rect, rect.width() * 0.27, APPLE_RED);
    let s = rect.width() / 24.0;
    let c = rect.center();
    let white = Color32::WHITE;
    let n1 = pos2(c.x - 3.8 * s, c.y + 4.2 * s);
    ui.painter().circle_filled(n1, 2.6 * s, white);
    let n2 = pos2(c.x + 3.8 * s, c.y + 2.0 * s);
    ui.painter().circle_filled(n2, 2.6 * s, white);
    ui.painter().rect_filled(Rect::from_min_max(pos2(n1.x + 1.2 * s, c.y - 5.0 * s), pos2(n1.x + 2.7 * s, n1.y)), 0.0, white);
    ui.painter().rect_filled(Rect::from_min_max(pos2(n2.x + 1.2 * s, c.y - 7.2 * s), pos2(n2.x + 2.7 * s, n2.y)), 0.0, white);
    let beam_pts = vec![
        pos2(n1.x + 1.2 * s, c.y - 3.0 * s),
        pos2(n2.x + 2.7 * s, c.y - 5.2 * s),
        pos2(n2.x + 2.7 * s, c.y - 7.5 * s),
        pos2(n1.x + 1.2 * s, c.y - 5.3 * s),
    ];
    ui.painter().add(egui::Shape::convex_polygon(beam_pts, white, egui::Stroke::NONE));
}

/// Neutral Guest badge
pub fn draw_guest_logo(ui: &Ui, rect: Rect) {
    ui.painter().rect_filled(rect, rect.width() * 0.27, theme::glass(0.18));
    let s = rect.width() / 24.0;
    let c = rect.center();
    let white = Color32::WHITE;
    // Headphones arch
    let pts: Vec<egui::Pos2> = (0..=12).map(|k| {
        let a = std::f32::consts::PI * (k as f32 / 12.0);
        pos2(c.x - 6.8 * s * a.cos(), c.y + 1.5 * s - 7.0 * s * a.sin())
    }).collect();
    ui.painter().add(egui::Shape::line(pts, egui::Stroke::new(2.2 * s, white)));
    // Earcups
    ui.painter().rect_filled(Rect::from_center_size(pos2(c.x - 6.8 * s, c.y + 2.5 * s), vec2(3.0 * s, 6.0 * s)), 1.5 * s, white);
    ui.painter().rect_filled(Rect::from_center_size(pos2(c.x + 6.8 * s, c.y + 2.5 * s), vec2(3.0 * s, 6.0 * s)), 1.5 * s, white);
}

fn platform_card(
    ui: &mut Ui,
    title: &str,
    desc: &str,
    connected: bool,
    btn_text: &str,
    accent: Color32,
    draw_icons: impl FnOnce(&Ui, Rect),
) -> bool {
    let w = ui.available_width();
    let btn_font = theme::semibold(13.5);
    // Measure button width dynamically so it never overflows or collides with text
    let btn_text_w = ui.fonts_mut(|f| f.layout_no_wrap(btn_text.to_owned(), btn_font.clone(), Color32::WHITE)).size().x;
    let btn_w = (btn_text_w + 34.0).max(110.0);

    let text_left = 80.0;
    let right_pad = btn_w + 22.0;
    let text_w = (w - text_left - right_pad).max(150.0);

    let title_font = theme::semibold(15.5);
    let desc_font = theme::regular(12.5);

    let title_color = theme::text();
    let title_g = widgets::galley(ui, title, title_font, title_color, text_w);
    let desc_g = widgets::galley_wrapped(ui, desc, desc_font, theme::text_dim(), text_w, 3);

    let content_h = title_g.size().y + 3.0 + desc_g.size().y;
    let h = (content_h + 28.0).max(84.0);

    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let hov = theme::anim_bool(ui.ctx(), resp.id.with("hov"), resp.hovered(), 0.15);
    
    // Background fill & border with BitChord's signature glassmorphism
    let fill = theme::glass(0.06 + 0.05 * hov);
    let border = if connected {
        theme::with_alpha(accent, 0.45)
    } else {
        theme::glass(0.08 + 0.12 * hov)
    };
    ui.painter().rect_filled(rect, 14.0, fill);
    ui.painter().rect_stroke(rect, 14.0, egui::Stroke::new(1.0, border), egui::StrokeKind::Inside);

    // Left Icon area
    let icon_rect = Rect::from_center_size(pos2(rect.left() + 42.0, rect.center().y), vec2(46.0, 46.0));
    draw_icons(ui, icon_rect);

    // Text area (safely bounded between icon and button)
    let text_x = rect.left() + text_left;
    let text_y = rect.center().y - content_h / 2.0;

    ui.painter().galley(pos2(text_x, text_y), title_g, title_color);
    ui.painter().galley(pos2(text_x, text_y + content_h - desc_g.size().y), desc_g, theme::text_dim());

    // Right Action / Pill area (clean non-colliding boundary)
    let ctl_rect = Rect::from_min_max(pos2(rect.right() - btn_w - 14.0, rect.top()), pos2(rect.right() - 8.0, rect.bottom()));
    let mut cui = ui.new_child(egui::UiBuilder::new().max_rect(ctl_rect).layout(egui::Layout::right_to_left(egui::Align::Center)));

    pill(&mut cui, btn_text, None, !connected).clicked() || resp.clicked()
}

pub fn page(app: &mut App, ui: &mut Ui) {
    let accent = app.accent_color();
    let avail = ui.available_width();
    let w = avail.min(760.0);

    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.add_space(((avail - w) / 2.0).max(0.0));
        ui.vertical(|ui| {
            ui.set_width(w);

            // Title & Subtitle
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Hubungkan Platform Musik Kamu").font(theme::bold(26.0)).color(theme::text()));
                ui.add_space(6.0);
                ui.label(RichText::new("Pilih akun musik utama untuk menyinkronkan seluruh playlist, liked songs, dan memutar audio Hi-Res Studio Master 24-bit/192kHz.")
                    .font(theme::regular(13.5))
                    .color(theme::text_dim()));
            });

            ui.add_space(20.0);

            let ctx = ui.ctx().clone();

            // 1. Google & YouTube Card
            let google_connected = app.logged_in;
            let google_btn = if google_connected { "Tersambung ✓" } else { "Masuk Akun" };
            if platform_card(
                ui,
                "Google & YouTube",
                "Playlist video YouTube dan YouTube Music kamu otomatis terdeteksi & tersinkronisasi.",
                google_connected,
                google_btn,
                accent,
                |ui, r| {
                    let sz = 28.0;
                    let r1 = Rect::from_center_size(pos2(r.center().x - 14.0, r.center().y), vec2(sz, sz));
                    draw_google_logo(ui, r1);
                    let r2 = Rect::from_center_size(pos2(r.center().x + 14.0, r.center().y), vec2(sz, sz));
                    draw_youtube_logo(ui, r2);
                },
            ) {
                app.onboarded = true;
                if google_connected {
                    app.actions.push(Action::Go(Page::Home));
                } else {
                    app.start_sign_in(&ctx);
                }
            }

            ui.add_space(10.0);

            // 2. Spotify Card
            let spotify_connected = matches!(&*app.transfer.account.lock().unwrap(), crate::transfer::Account::SignedIn { .. });
            let spotify_btn = if spotify_connected { "Tersambung ✓" } else { "Masuk Spotify" };
            if platform_card(
                ui,
                "Spotify",
                "Sinkronkan Liked Songs dan Playlist Spotify kamu dengan audio Studio Master FLAC 192kHz.",
                spotify_connected,
                spotify_btn,
                accent,
                |ui, r| {
                    let sz = 38.0;
                    draw_spotify_logo(ui, Rect::from_center_size(r.center(), vec2(sz, sz)));
                },
            ) {
                app.onboarded = true;
                if spotify_connected {
                    app.actions.push(Action::Go(Page::Transfer));
                } else {
                    app.start_spotify_sign_in(&ctx);
                }
            }

            ui.add_space(10.0);

            // 3. Apple Music Card
            if platform_card(
                ui,
                "Apple Music",
                "Impor dan putar seluruh playlist & library Apple Music kamu tanpa kompresi.",
                false,
                "Buka Import",
                accent,
                |ui, r| {
                    let sz = 38.0;
                    draw_apple_music_logo(ui, Rect::from_center_size(r.center(), vec2(sz, sz)));
                },
            ) {
                app.onboarded = true;
                app.actions.push(Action::Go(Page::Transfer));
            }

            ui.add_space(10.0);

            // 4. Continue as Guest / Offline
            if platform_card(
                ui,
                "Lanjut Tanpa Akun (Guest)",
                "Langsung cari lagu apa saja, putar dengan master 192kHz, atau buka file audio lokal kamu.",
                false,
                "Mulai Mendengarkan →",
                accent,
                |ui, r| {
                    let sz = 38.0;
                    draw_guest_logo(ui, Rect::from_center_size(r.center(), vec2(sz, sz)));
                },
            ) {
                app.onboarded = true;
                app.actions.push(Action::Go(Page::Home));
            }

            ui.add_space(22.0);

            // Bottom action: Go to Home
            ui.vertical_centered(|ui| {
                if pill(ui, "Lanjut ke Beranda →", None, true).clicked() {
                    app.onboarded = true;
                    app.actions.push(Action::Go(Page::Home));
                }
            });

            ui.add_space(40.0);
        });
    });
}
