use eframe::egui;
use egui_video::Player;
use std::sync::Arc;
use tokio::sync::mpsc;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info};

mod bilibili_streamer;
mod http_streamer;
mod local_file_streamer;
mod streamer;
mod torrent;
mod youtube_streamer;

use crate::bilibili_streamer::BilibiliStreamer;
use crate::http_streamer::HttpStreamer;
use crate::local_file_streamer::LocalFileStreamer;
use crate::streamer::{DownloadStats, StreamerManager};
use crate::torrent::TorrentStreamer;
use crate::youtube_streamer::YouTubeStreamer;

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let exp = (bytes as f64).log(1024.0).min(UNITS.len() as f64 - 1.0) as usize;
    let value = bytes as f64 / 1024_f64.powi(exp as i32);
    format!("{:.2} {}", value, UNITS[exp])
}

enum AppMessage {
    TaskAdded(usize),
    StreamUrlReady(String, String),
    Error(String),
    StatsUpdated(DownloadStats),
    FileSelected(String),
}

struct RustPlayerApp {
    streamer: Arc<StreamerManager>,
    input_url: String,
    player: Option<Player>,
    audio_device: Option<egui_video::AudioDevice>,
    status: String,
    current_task_id: Option<usize>,
    stats: DownloadStats,
    tx: mpsc::UnboundedSender<AppMessage>,
    rx: mpsc::UnboundedReceiver<AppMessage>,
    last_stats_update: std::time::Instant,
}

impl RustPlayerApp {
    fn new(cc: &eframe::CreationContext<'_>, streamer: Arc<StreamerManager>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        
        // Add a default system font that supports Chinese (e.g. PingFang SC or similar system fallbacks)
        fonts.font_data.insert(
            "custom_chinese".to_owned(),
            egui::FontData::from_static(include_bytes!("/System/Library/Fonts/Hiragino Sans GB.ttc")),
        );
        
        // Put the custom font first for Proportional text
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "custom_chinese".to_owned());
            
        // Also put it first for Monospace text
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "custom_chinese".to_owned());
            
        cc.egui_ctx.set_fonts(fonts);

        let (tx, rx) = mpsc::unbounded_channel();
        let audio_device = match egui_video::AudioDevice::new() {
            Ok(device) => Some(device),
            Err(e) => {
                error!("Failed to initialize audio device: {}", e);
                None
            }
        };

        Self {
            streamer,
            input_url: String::new(),
            player: None,
            audio_device,
            status: "准备就绪".to_string(),
            current_task_id: None,
            stats: DownloadStats::default(),
            tx,
            rx,
            last_stats_update: std::time::Instant::now(),
        }
    }

    fn start_playback(&mut self, ctx: &egui::Context) {
        let url = self.input_url.trim().to_string();
        if url.is_empty() {
            self.status = "❌ 请输入有效的链接".to_string();
            return;
        }

        let streamer = self.streamer.clone();
        let tx = self.tx.clone();
        let ctx_clone = ctx.clone();
        
        self.status = "🚀 正在初始化...".to_string();
        self.player = None;
        
        tokio::spawn(async move {
            match streamer.add_task(&url).await {
                Ok(id) => {
                    let _ = tx.send(AppMessage::TaskAdded(id));
                    if let Ok(video_file) = streamer.get_main_video_file(id).await {
                        match streamer.get_stream_url(id, video_file.index).await {
                            Ok(stream_url) => {
                                let _ = tx.send(AppMessage::StreamUrlReady(stream_url, video_file.name));
                            }
                            Err(e) => {
                                let _ = tx.send(AppMessage::Error(format!("⚠️ 无法获取流 URL: {}", e)));
                            }
                        }
                    } else {
                        let _ = tx.send(AppMessage::Error("⚠️ 未找到视频文件".to_string()));
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppMessage::Error(format!("❌ 任务添加失败: {}", e)));
                }
            }
            ctx_clone.request_repaint();
        });
    }

    fn poll_stats(&mut self, ctx: &egui::Context) {
        if let Some(id) = self.current_task_id {
            if self.last_stats_update.elapsed() > Duration::from_secs(1) {
                let streamer = self.streamer.clone();
                let tx = self.tx.clone();
                let ctx_clone = ctx.clone();
                tokio::spawn(async move {
                    let stats = streamer.get_stats(id).await;
                    let _ = tx.send(AppMessage::StatsUpdated(stats));
                    ctx_clone.request_repaint();
                });
                self.last_stats_update = std::time::Instant::now();
            }
        }
    }
}

impl eframe::App for RustPlayerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AppMessage::TaskAdded(id) => {
                    self.current_task_id = Some(id);
                    self.status = "✅ 任务添加成功, 获取流地址...".to_string();
                }
                AppMessage::StreamUrlReady(url, title) => {
                    info!("Ready to play: {} -> {}", title, url);
                    self.status = format!("🎬 正在播放: {}", title);
                    match Player::new(ctx, &url) {
                        Ok(mut player) => {
                            if let Some(audio_device) = &mut self.audio_device {
                                player = match player.with_audio(audio_device) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        error!("Failed to enable audio: {}", e);
                                        self.status = format!("❌ 音频初始化失败: {}", e);
                                        return;
                                    }
                                };
                            }
                            player.start();
                            self.player = Some(player);
                        }
                        Err(e) => {
                            error!("Player error: {}", e);
                            self.status = format!("❌ 播放器初始化失败: {}", e);
                        }
                    }
                }
                AppMessage::Error(e) => {
                    self.status = e;
                    self.current_task_id = None;
                }
                AppMessage::StatsUpdated(stats) => {
                    self.stats = stats;
                }
                AppMessage::FileSelected(path) => {
                    self.input_url = format!("file://{}", path);
                }
            }
        }

        self.poll_stats(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(10.0);
                ui.heading(egui::RichText::new("Rust Magnet Player").size(32.0).color(egui::Color32::from_rgb(100, 150, 255)));
                ui.label("高性能磁力与网络流播放器");
                ui.add_space(20.0);

                ui.horizontal(|ui| {
                    ui.add_space(ui.available_width() * 0.1);
                    let input_width = ui.available_width() * 0.6;
                    ui.add(egui::TextEdit::singleline(&mut self.input_url)
                        .hint_text("输入 Bilibili / YouTube / 磁力 / HTTP 链接...")
                        .desired_width(input_width));
                    
                    if ui.button("📁 打开本地").clicked() {
                        let tx = self.tx.clone();
                        let ctx_clone = ctx.clone();
                        tokio::spawn(async move {
                            if let Some(file) = rfd::AsyncFileDialog::new()
                                .set_title("选择本地视频文件")
                                .add_filter("视频文件", &["mp4", "mkv", "avi", "webm", "flv", "mov"])
                                .pick_file()
                                .await
                            {
                                let _ = tx.send(AppMessage::FileSelected(file.path().to_string_lossy().to_string()));
                                ctx_clone.request_repaint();
                            }
                        });
                    }

                    if ui.button("▶️ 播放").clicked() {
                        self.start_playback(ctx);
                    }
                });

                ui.add_space(10.0);
                ui.label(egui::RichText::new(&self.status).color(egui::Color32::GRAY));
                ui.add_space(10.0);

                let available_size = ui.available_size();
                let player_height = available_size.y - 60.0;
                let (_, rect) = ui.allocate_space([available_size.x, player_height.max(200.0)].into());
                
                if let Some(player) = &mut self.player {
                    let mut player_rect = rect;
                    
                    if player.size.x > 0.0 && player.size.y > 0.0 {
                        let video_ratio = player.size.x / player.size.y;
                        let available_ratio = player_rect.width() / player_rect.height();
                        
                        if video_ratio > available_ratio {
                            // Video is wider, fit to width
                            let new_height = player_rect.width() / video_ratio;
                            let y_offset = (player_rect.height() - new_height) / 2.0;
                            player_rect.min.y += y_offset;
                            player_rect.max.y = player_rect.min.y + new_height;
                        } else {
                            // Video is taller, fit to height
                            let new_width = player_rect.height() * video_ratio;
                            let x_offset = (player_rect.width() - new_width) / 2.0;
                            player_rect.min.x += x_offset;
                            player_rect.max.x = player_rect.min.x + new_width;
                        }
                    }

                    // Use ui_at to draw inside the exact allocated rectangle without pushing the cursor further down
                    player.ui_at(ui, player_rect);
                    
                    ui.horizontal(|ui| {
                        let is_playing = player.player_state.get() == egui_video::PlayerState::Playing;
                        if ui.button(if is_playing { "⏸ 暂停" } else { "▶ 继续" }).clicked() {
                            if is_playing { player.pause(); } else { player.start(); }
                        }
                        if ui.button("⏹ 停止").clicked() {
                            player.stop();
                        }
                        let pos_ms = player.elapsed_ms() as f64;
                        let pos = pos_ms / 1000.0;
                        let duration_ms = player.duration_ms as f64;
                        let duration = duration_ms / 1000.0;
                        let mut slider_pos = pos;
                        if ui.add(egui::Slider::new(&mut slider_pos, 0.0..=duration)
                            .text(format!("{:.1}s / {:.1}s", pos, duration))
                            .show_value(false))
                            .changed() 
                        {
                            let seek_frac = if duration > 0.0 { (slider_pos / duration) as f32 } else { 0.0 };
                            player.seek(seek_frac);
                        }
                    });
                } else {
                    ui.painter().rect_filled(rect, 4.0, egui::Color32::from_black_alpha(100));
                    ui.put(rect, egui::Label::new("等待视频加载..."));
                }

                if self.current_task_id.is_some() {
                    ui.add_space(10.0);
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!("进度: {:.1}%", self.stats.progress_percent));
                            ui.separator();
                            ui.label(format!("速度: {}/s", format_bytes(self.stats.download_speed as u64)));
                            ui.separator();
                            ui.label(format!("已下载: {} / {}", format_bytes(self.stats.downloaded_bytes), format_bytes(self.stats.total_bytes)));
                            ui.separator();
                            ui.label(format!("连接数: {}", self.stats.peers_count));
                        });
                    });
                }
            });
        });

        if self.player.is_some() {
            ctx.request_repaint();
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), eframe::Error> {
    tracing_subscriber::fmt::init();

    let download_path = dirs::download_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| String::from("./downloads"));
    
    let mut manager = StreamerManager::new();
    let download_path_buf = PathBuf::from(&download_path);

    if let Ok(ts) = TorrentStreamer::new(download_path_buf.clone()).await {
        manager.register_backend(ts);
    }
    if let Ok(ys) = YouTubeStreamer::new(download_path_buf.clone()).await {
        manager.register_backend(ys);
    }
    if let Ok(bs) = BilibiliStreamer::new(download_path_buf.clone()).await {
        manager.register_backend(bs);
    }
    // HttpStreamer registered last as a fallback
    if let Ok(hs) = HttpStreamer::new(download_path_buf.clone()).await {
        manager.register_backend(hs);
    }
    if let Ok(ls) = LocalFileStreamer::new().await {
        manager.register_backend(ls);
    }
    let streamer = Arc::new(manager);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([900.0, 650.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Rust Magnet Player",
        native_options,
        Box::new(|cc| {
            Ok(Box::new(RustPlayerApp::new(cc, streamer)))
        }),
    )
}
