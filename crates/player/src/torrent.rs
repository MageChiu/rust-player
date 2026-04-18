use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use librqbit::api::TorrentIdOrHash;
use librqbit::{AddTorrent, AddTorrentOptions, ManagedTorrent, Session, SessionOptions};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::RwLock;
use tokio_util::io::ReaderStream;
use tracing::{error, info};

use crate::streamer::{DownloadStats, FileInfo, MediaStreamer};

/// HTTP 流服务器状态
#[derive(Clone)]
struct HttpServerState {
    streamer: Arc<TorrentStreamerInner>,
}

/// 内部结构体，避免 TorrentStreamer 的递归定义
struct TorrentStreamerInner {
    session: Arc<Session>,
    download_dir: PathBuf,
}

pub struct TorrentStreamer {
    inner: Arc<TorrentStreamerInner>,
    server_port: RwLock<u16>,
    server_handle: RwLock<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait::async_trait]
impl MediaStreamer for TorrentStreamer {
    fn supports(&self, url: &str) -> bool {
        url.starts_with("magnet:?")
    }

    async fn add_task(&self, url: &str) -> Result<usize> {
        self.add_magnet(url).await
    }

    fn get_stats(&self, task_id: usize) -> DownloadStats {
        self.get_stats(task_id)
    }

    fn get_files(&self, task_id: usize) -> Result<Vec<FileInfo>> {
        self.get_files(task_id)
    }

    async fn get_stream_url(&self, task_id: usize, file_index: usize) -> Result<String> {
        self.get_stream_url(task_id, file_index).await
    }

    async fn pause(&self, task_id: usize) -> Result<()> {
        self.pause(task_id).await
    }

    async fn resume(&self, task_id: usize) -> Result<()> {
        self.resume(task_id).await
    }

    async fn remove(&self, task_id: usize, delete_files: bool) -> Result<()> {
        self.remove(task_id, delete_files).await
    }
}

impl TorrentStreamer {
    pub async fn new(download_dir: PathBuf) -> Result<Self> {
        info!("Initializing TorrentStreamer, download dir: {:?}", download_dir);

        tokio::fs::create_dir_all(&download_dir)
            .await
            .context("Failed to create download directory")?;

        let session_opts = SessionOptions {
            disable_dht: false,
            disable_dht_persistence: false,
            dht_config: None,
            persistence: None,
            enable_upnp_port_forwarding: true,
            ..Default::default()
        };

        let session = Session::new_with_opts(download_dir.clone().into(), session_opts)
            .await
            .context("Failed to create librqbit Session")?;

        info!("TorrentStreamer initialized successfully");

        let streamer = Self {
            inner: Arc::new(TorrentStreamerInner {
                session,
                download_dir,
            }),
            server_port: RwLock::new(0),
            server_handle: RwLock::new(None),
        };

        // 启动 HTTP 服务器
        streamer.start_http_server(0).await?;

        Ok(streamer)
    }

    /// 启动 HTTP 流媒体服务器
    async fn start_http_server(&self, port: u16) -> Result<()> {
        let state = HttpServerState {
            streamer: Arc::clone(&self.inner),
        };

        // 创建路由
        let app = Router::new()
            .route("/stream/{torrent_id}/{file_index}", get(stream_file_handler))
            .route("/health", get(health_handler))
            .layer(
                tower_http::cors::CorsLayer::new()
                    .allow_origin(tower_http::cors::Any)
                    .allow_methods([axum::http::Method::GET, axum::http::Method::HEAD, axum::http::Method::OPTIONS])
                    .allow_headers([header::RANGE, header::ACCEPT_RANGES, header::CONTENT_TYPE]),
            )
            .with_state(state);

        // 绑定到地址
        let addr = if port == 0 {
            "127.0.0.1:0".parse::<SocketAddr>()?
        } else {
            SocketAddr::from(([127, 0, 0, 1], port))
        };

        let listener = tokio::net::TcpListener::bind(addr).await?;
        let actual_port = listener.local_addr()?.port();

        info!("HTTP streaming server started on port {}", actual_port);

        // 保存端口
        *self.server_port.write().await = actual_port;

        // 启动服务器
        let server_handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!("HTTP server error: {}", e);
            }
        });

        *self.server_handle.write().await = Some(server_handle);

        Ok(())
    }

    /// 获取流媒体 URL
    pub async fn get_stream_url(&self, torrent_id: usize, file_index: usize) -> Result<String> {
        let port = *self.server_port.read().await;
        if port == 0 {
            anyhow::bail!("HTTP server not started");
        }

        // 验证 torrent 和文件是否存在
        let _ = self.get_file_path(torrent_id, file_index)?;

        let url = format!("http://127.0.0.1:{}/stream/{}/{}", port, torrent_id, file_index);
        Ok(url)
    }

    pub async fn add_magnet(&self, magnet_url: &str) -> Result<usize> {
        info!("Adding magnet link: {}", magnet_url);

        if !magnet_url.starts_with("magnet:?") {
            anyhow::bail!("Invalid magnet link format");
        }

        let mut magnet = librqbit::Magnet::parse(magnet_url)
            .map_err(|e| anyhow::anyhow!("Invalid magnet link: {}", e))?;

        const DEFAULT_TRACKERS: &[&str] = &[
            "udp://tracker.opentrackr.org:1337/announce",
            "udp://open.demonii.com:1337/announce",
            "udp://tracker.openbittorrent.com:80/announce",
            "udp://exodus.desync.com:6969/announce",
            "udp://tracker.torrent.eu.org:451/announce",
            "udp://9.rarbg.com:2810/announce",
            "udp://tracker.dler.org:6969/announce",
        ];
        
        for tr in DEFAULT_TRACKERS {
            let tr_str = tr.to_string();
            if !magnet.trackers.contains(&tr_str) {
                magnet.trackers.push(tr_str);
            }
        }

        let opts = AddTorrentOptions {
            paused: false,
            overwrite: true,
            ..Default::default()
        };

        let response = self
            .inner
            .session
            .add_torrent(AddTorrent::from_url(&magnet.to_string()), Some(opts))
            .await
            .context("Failed to add torrent")?;

        match response {
            librqbit::AddTorrentResponse::AlreadyManaged(id, _) => {
                info!("Torrent already managed, ID: {}", id);
                Ok(id)
            }
            librqbit::AddTorrentResponse::ListOnly(_) => {
                anyhow::bail!("List-only mode not supported")
            }
            librqbit::AddTorrentResponse::Added(id, _) => {
                info!("Torrent added successfully, ID: {}", id);
                Ok(id)
            }
        }
    }

    fn get_handle(&self, id: usize) -> Option<Arc<ManagedTorrent>> {
        self.inner.session.get(TorrentIdOrHash::Id(id))
    }

    pub fn get_stats(&self, id: usize) -> DownloadStats {
        let Some(handle) = self.get_handle(id) else {
            return DownloadStats {
                error: Some("Torrent not found".to_string()),
                ..Default::default()
            };
        };

        let stats = handle.stats();

        let progress_percent = if stats.total_bytes > 0 {
            (stats.progress_bytes as f64 / stats.total_bytes as f64 * 100.0) as f32
        } else {
            0.0
        };

        let (download_speed, upload_speed, peers_count) = if let Some(live) = stats.live {
            let download = live.download_speed.mbps * 1024.0 * 1024.0;
            let upload = live.upload_speed.mbps * 1024.0 * 1024.0;
            let peers = live.snapshot.peer_stats.live;
            (download, upload, peers)
        } else {
            (0.0, 0.0, 0)
        };

        DownloadStats {
            progress_percent,
            downloaded_bytes: stats.progress_bytes,
            total_bytes: stats.total_bytes,
            download_speed,
            upload_speed,
            is_finished: stats.finished,
            error: stats.error.clone(),
            peers_count,
        }
    }

    pub fn get_files(&self, id: usize) -> Result<Vec<FileInfo>> {
        let handle = self.get_handle(id).context("Torrent not found")?;

        let stats = handle.stats();

        let files: Vec<FileInfo> = handle
            .with_metadata(|metadata| {
                let info = &metadata.info;

                if let Some(files_list) = &info.files {
                    files_list
                        .iter()
                        .enumerate()
                        .map(|(idx, file)| {
                            let name = file
                                .path
                                .iter()
                                .map(|p| String::from_utf8_lossy(p.as_ref()))
                                .collect::<Vec<_>>()
                                .join("/");

                            let size = file.length;
                            let downloaded = stats.file_progress.get(idx).copied().unwrap_or(0);
                            let is_video = is_video_file(&name);

                            FileInfo {
                                index: idx,
                                name,
                                size,
                                downloaded,
                                is_video,
                            }
                        })
                        .collect()
                } else {
                    let name = info
                        .name
                        .as_ref()
                        .map(|n| String::from_utf8_lossy(n.as_ref()).to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    let size = info.length.unwrap_or(0);
                    let downloaded = stats.progress_bytes;
                    let is_video = is_video_file(&name);

                    vec![FileInfo {
                        index: 0,
                        name,
                        size,
                        downloaded,
                        is_video,
                    }]
                }
            })
            .context("Failed to get metadata")?;

        Ok(files)
    }

    pub fn get_video_files(&self, id: usize) -> Result<Vec<FileInfo>> {
        let files = self.get_files(id)?;
        Ok(files.into_iter().filter(|f| f.is_video).collect())
    }

    pub fn get_main_video_file(&self, id: usize) -> Result<FileInfo> {
        let video_files = self.get_video_files(id)?;

        video_files
            .into_iter()
            .max_by_key(|f| f.size)
            .context("No video file found")
    }

    pub fn get_file_path(&self, id: usize, file_index: usize) -> Result<PathBuf> {
        let handle = self.get_handle(id).context("Torrent not found")?;

        let torrent_name = handle.name().unwrap_or_default();
        let files = self.get_files(id)?;

        let file = files.get(file_index).context("Invalid file index")?;

        let path = if files.len() == 1 {
            self.inner.download_dir.join(&torrent_name)
        } else {
            self.inner.download_dir.join(&torrent_name).join(&file.name)
        };

        Ok(path)
    }

    pub async fn pause(&self, id: usize) -> Result<()> {
        let handle = self.get_handle(id).context("Torrent not found")?;

        self.inner
            .session
            .pause(&handle)
            .await
            .context("Failed to pause download")?;
        info!("Torrent {} paused", id);
        Ok(())
    }

    pub async fn resume(&self, id: usize) -> Result<()> {
        let handle = self.get_handle(id).context("Torrent not found")?;

        self.inner
            .session
            .unpause(&handle)
            .await
            .context("Failed to resume download")?;
        info!("Torrent {} resumed", id);
        Ok(())
    }

    pub async fn remove(&self, id: usize, delete_files: bool) -> Result<()> {
        self.inner
            .session
            .delete(TorrentIdOrHash::Id(id), delete_files)
            .await
            .context("Failed to delete torrent")?;
        info!("Torrent {} removed (delete files: {})", id, delete_files);
        Ok(())
    }

    pub fn is_ready_for_playback(&self, id: usize, min_percent: f32) -> bool {
        let stats = self.get_stats(id);
        stats.progress_percent >= min_percent || stats.is_finished
    }

    pub fn session(&self) -> Arc<Session> {
        Arc::clone(&self.inner.session)
    }
}

fn is_video_file(name: &str) -> bool {
    let ext = name.to_lowercase();
    ext.ends_with(".mp4")
        || ext.ends_with(".mkv")
        || ext.ends_with(".avi")
        || ext.ends_with(".mov")
        || ext.ends_with(".wmv")
        || ext.ends_with(".flv")
        || ext.ends_with(".webm")
        || ext.ends_with(".m4v")
        || ext.ends_with(".mpg")
        || ext.ends_with(".mpeg")
        || ext.ends_with(".3gp")
        || ext.ends_with(".ts")
        || ext.ends_with(".m2ts")
}

/// 健康检查处理器
async fn health_handler() -> impl IntoResponse {
    "OK"
}

/// 文件流处理器 - 支持 Range 请求
async fn stream_file_handler(
    State(state): State<HttpServerState>,
    Path((torrent_id, file_index)): Path<(usize, usize)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // 获取文件路径
    let file_path = match state.streamer.session.get(TorrentIdOrHash::Id(torrent_id)) {
        Some(handle) => {
            let torrent_name = handle.name().unwrap_or_default();

            // 获取文件列表以确定路径
            let files: Vec<(String, u64)> = match handle.with_metadata(|metadata| {
                let info = &metadata.info;
                if let Some(files_list) = &info.files {
                    files_list
                        .iter()
                        .map(|file| {
                            let name = file
                                .path
                                .iter()
                                .map(|p| String::from_utf8_lossy(p.as_ref()))
                                .collect::<Vec<_>>()
                                .join("/");
                            (name, file.length)
                        })
                        .collect()
                } else {
                    let name = info
                        .name
                        .as_ref()
                        .map(|n| String::from_utf8_lossy(n.as_ref()).to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let size = info.length.unwrap_or(0);
                    vec![(name, size)]
                }
            }) {
                Ok(f) => f,
                Err(_) => {
                    return (StatusCode::NOT_FOUND, HeaderMap::new(), axum::body::Body::empty()).into_response();
                }
            };

            if file_index >= files.len() {
                return (StatusCode::NOT_FOUND, HeaderMap::new(), axum::body::Body::empty()).into_response();
            }

            let (file_name, file_size) = &files[file_index];
            let path = if files.len() == 1 {
                state.streamer.download_dir.join(&torrent_name)
            } else {
                state.streamer.download_dir.join(&torrent_name).join(file_name)
            };

            (path, *file_size)
        }
        None => {
            return (StatusCode::NOT_FOUND, HeaderMap::new(), axum::body::Body::empty()).into_response();
        }
    };

    // 检查文件是否存在
    if !file_path.0.exists() {
        return (StatusCode::NOT_FOUND, HeaderMap::new(), axum::body::Body::empty()).into_response();
    }

    // 获取文件元数据
    let metadata = match tokio::fs::metadata(&file_path.0).await {
        Ok(m) => m,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), axum::body::Body::empty()).into_response();
        }
    };

    let file_size = metadata.len();

    // 解析 Range 请求头
    let range_header = headers.get(header::RANGE);

    if let Some(range) = range_header {
        // 解析 range 头部
        let range_str = match range.to_str() {
            Ok(s) => s,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, HeaderMap::new(), axum::body::Body::empty()).into_response();
            }
        };

        // 支持 bytes=0-1023 格式
        if let Some(range_val) = range_str.strip_prefix("bytes=") {
            let parts: Vec<&str> = range_val.split('-').collect();
            if parts.len() == 2 {
                let start = parts[0].parse::<u64>().unwrap_or(0);
                let end = parts[1].parse::<u64>().unwrap_or(file_size - 1);
                let end = end.min(file_size - 1);
                let content_length = end.saturating_sub(start) + 1;

                let mut file = match File::open(&file_path.0).await {
                    Ok(f) => f,
                    Err(_) => {
                        return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), axum::body::Body::empty()).into_response();
                    }
                };

                if let Err(_) = file.seek(SeekFrom::Start(start)).await {
                    return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), axum::body::Body::empty()).into_response();
                }

                let stream = ReaderStream::new(file.take(content_length));
                let body = axum::body::Body::from_stream(stream);

                let mut response_headers = HeaderMap::new();
                response_headers.insert(
                    header::CONTENT_TYPE,
                    crate::streamer::get_mime_type_from_filename(&file_path.0.to_string_lossy()),
                );
                response_headers.insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, file_size)).unwrap(),
                );
                response_headers.insert(
                    header::CONTENT_LENGTH,
                    HeaderValue::from_str(&content_length.to_string()).unwrap(),
                );
                response_headers.insert(
                    header::ACCEPT_RANGES,
                    HeaderValue::from_static("bytes"),
                );

                return (StatusCode::PARTIAL_CONTENT, response_headers, body).into_response();
            }
        }
    }

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        crate::streamer::get_mime_type_from_filename(&file_path.0.to_string_lossy()),
    );
    response_headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&file_size.to_string()).unwrap(),
    );
    response_headers.insert(
        header::ACCEPT_RANGES,
        HeaderValue::from_static("bytes"),
    );

    match File::open(&file_path.0).await {
        Ok(file) => {
            let stream = ReaderStream::new(file);
            let body = axum::body::Body::from_stream(stream);
            (StatusCode::OK, response_headers, body).into_response()
        }
        Err(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), axum::body::Body::empty()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_video_file() {
        assert!(is_video_file("test.mp4"));
        assert!(is_video_file("test.MKV"));
        assert!(is_video_file("test.avi"));
        assert!(!is_video_file("test.txt"));
    }
}
