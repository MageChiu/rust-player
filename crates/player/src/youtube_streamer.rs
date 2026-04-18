use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use reqwest::Client;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncSeekExt, SeekFrom};
use tokio::time::Instant;
use tokio_util::io::ReaderStream;
use tracing::{error, info};
use rusty_ytdl::{Video, VideoOptions, VideoQuality, VideoSearchOptions};

use crate::streamer::{DownloadStats, FileInfo, MediaStreamer};

#[derive(Clone)]
struct YouTubeTask {
    url: String,
    file_name: String,
    total_size: u64,
    file_path: PathBuf,
    download_url: String,
}

#[derive(Clone, Default)]
struct TaskState {
    downloaded: u64,
    speed: f64,
    is_finished: bool,
    error: Option<String>,
}

struct YouTubeStreamerInner {
    tasks: RwLock<HashMap<usize, YouTubeTask>>,
    states: RwLock<HashMap<usize, TaskState>>,
    next_id: AtomicUsize,
    download_dir: PathBuf,
    client: Client,
}

pub struct YouTubeStreamer {
    inner: Arc<YouTubeStreamerInner>,
    server_port: tokio::sync::RwLock<u16>,
}

impl YouTubeStreamer {
    pub async fn new(download_dir: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&download_dir)
            .await
            .context("Failed to create download directory")?;

        let inner = Arc::new(YouTubeStreamerInner {
            tasks: RwLock::new(HashMap::new()),
            states: RwLock::new(HashMap::new()),
            next_id: AtomicUsize::new(1),
            download_dir,
            client: Client::new(),
        });

        let streamer = Self {
            inner,
            server_port: tokio::sync::RwLock::new(0),
        };

        streamer.start_http_server(0).await?;

        Ok(streamer)
    }

    async fn start_http_server(&self, port: u16) -> Result<()> {
        let state = Arc::clone(&self.inner);
        let app = Router::new()
            .route("/stream/{task_id}/{file_index}", get(stream_file_handler))
            .layer(
                tower_http::cors::CorsLayer::new()
                    .allow_origin(tower_http::cors::Any)
                    .allow_methods([axum::http::Method::GET, axum::http::Method::HEAD, axum::http::Method::OPTIONS])
                    .allow_headers([header::RANGE, header::ACCEPT_RANGES, header::CONTENT_TYPE]),
            )
            .with_state(state);

        let addr = if port == 0 {
            "127.0.0.1:0".parse::<SocketAddr>()?
        } else {
            SocketAddr::from(([127, 0, 0, 1], port))
        };

        let listener = tokio::net::TcpListener::bind(addr).await?;
        let actual_port = listener.local_addr()?.port();

        info!("YouTubeStreamer server started on port {}", actual_port);
        *self.server_port.write().await = actual_port;

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!("YouTubeStreamer server error: {}", e);
            }
        });

        Ok(())
    }
}

#[async_trait::async_trait]
impl MediaStreamer for YouTubeStreamer {
    fn supports(&self, url: &str) -> bool {
        url.contains("youtube.com") || url.contains("youtu.be")
    }

    async fn add_task(&self, url: &str) -> Result<usize> {
        info!("YouTubeStreamer: 开始解析链接 - {}", url);

        let video_options = VideoOptions {
            quality: VideoQuality::Highest,
            filter: VideoSearchOptions::VideoAudio,
            ..Default::default()
        };

        let video = Video::new_with_options(url, video_options.clone()).context("Invalid YouTube URL")?;
        let video_info = video.get_info().await.context("Failed to get YouTube video info")?;

        let title = video_info.video_details.title.clone();
        let file_name = format!("{}.mp4", title)
            .replace(&['/', '\\', ':', '*', '?', '"', '<', '>', '|'][..], "_");
        let file_path = self.inner.download_dir.join(&file_name);

        let format = rusty_ytdl::choose_format(&video_info.formats, &video_options)
            .context("No suitable format found")?;

        let download_url = format.url.clone();
        
        let total_size = format.content_length.clone().unwrap_or("0".to_string()).parse::<u64>().unwrap_or(0);

        let task_id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        
        let task = YouTubeTask {
            url: url.to_string(),
            file_name,
            total_size,
            file_path: file_path.clone(),
            download_url: download_url.clone(),
        };

        {
            let mut tasks = self.inner.tasks.write().unwrap();
            tasks.insert(task_id, task);
            let mut states = self.inner.states.write().unwrap();
            states.insert(task_id, TaskState::default());
        }

        let inner_clone = Arc::clone(&self.inner);
        tokio::spawn(async move {
            info!("YouTubeStreamer: 开始后台下载任务 {}", task_id);

            let result = direct_download_loop(inner_clone.clone(), task_id, download_url, file_path).await;

            if let Err(e) = result {
                if let Ok(mut states) = inner_clone.states.write() {
                    if let Some(state) = states.get_mut(&task_id) {
                        state.error = Some(e.to_string());
                        state.is_finished = true;
                    }
                }
                error!("YouTubeStreamer task {} failed: {}", task_id, e);
            } else {
                info!("YouTubeStreamer: 后台任务 {} 成功完成", task_id);
            }
        });

        Ok(task_id)
    }

    fn get_stats(&self, task_id: usize) -> DownloadStats {
        let (total_size, downloaded, speed, is_finished, error) = {
            let tasks = self.inner.tasks.read().unwrap();
            let states = self.inner.states.read().unwrap();

            let total_size = tasks.get(&task_id).map(|t| t.total_size).unwrap_or(0);

            if let Some(state) = states.get(&task_id) {
                (
                    total_size,
                    state.downloaded,
                    state.speed,
                    state.is_finished,
                    state.error.clone(),
                )
            } else {
                return DownloadStats::default();
            }
        };

        let progress_percent = if total_size > 0 {
            (downloaded as f64 / total_size as f64 * 100.0) as f32
        } else {
            0.0
        };

        DownloadStats {
            progress_percent,
            downloaded_bytes: downloaded,
            total_bytes: total_size,
            download_speed: speed,
            upload_speed: 0.0,
            is_finished,
            error,
            peers_count: 1,
        }
    }

    fn get_files(&self, task_id: usize) -> Result<Vec<FileInfo>> {
        let (file_name, total_size, downloaded) = {
            let tasks = self.inner.tasks.read().unwrap();
            let states = self.inner.states.read().unwrap();

            let task = tasks.get(&task_id).context("Task not found")?;
            let downloaded = states.get(&task_id).map(|s| s.downloaded).unwrap_or(0);

            (task.file_name.clone(), task.total_size, downloaded)
        };

        Ok(vec![FileInfo {
            index: 0,
            name: file_name,
            size: total_size,
            downloaded,
            is_video: true,
        }])
    }

    async fn get_stream_url(&self, task_id: usize, file_index: usize) -> Result<String> {
        let port = *self.server_port.read().await;
        if port == 0 {
            anyhow::bail!("HTTP server not started");
        }
        let url = format!("http://127.0.0.1:{}/stream/{}/{}", port, task_id, file_index);
        Ok(url)
    }

    async fn pause(&self, _task_id: usize) -> Result<()> {
        anyhow::bail!("Pause not supported for HTTP streams yet")
    }

    async fn resume(&self, _task_id: usize) -> Result<()> {
        anyhow::bail!("Resume not supported for HTTP streams yet")
    }

    async fn remove(&self, task_id: usize, delete_files: bool) -> Result<()> {
        let task = {
            let mut tasks = self.inner.tasks.write().unwrap();
            let mut states = self.inner.states.write().unwrap();
            states.remove(&task_id);
            tasks.remove(&task_id)
        };

        if let Some(task) = task {
            if delete_files {
                let _ = tokio::fs::remove_file(task.file_path).await;
            }
        }

        Ok(())
    }
}

async fn direct_download_loop(
    inner: Arc<YouTubeStreamerInner>,
    task_id: usize,
    download_url: String,
    file_path: PathBuf,
) -> Result<()> {
    let response = inner.client.get(&download_url).send().await.context("Failed to download")?;
    let mut file = File::create(&file_path).await?;
    let mut stream = response.bytes_stream();

    let mut downloaded = 0u64;
    let mut last_update = Instant::now();
    let mut bytes_since_last_update = 0u64;

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.context("Failed to read chunk")?;
        file.write_all(&chunk).await?;

        let chunk_len = chunk.len() as u64;
        downloaded += chunk_len;
        bytes_since_last_update += chunk_len;

        let now = Instant::now();
        let elapsed = now.duration_since(last_update).as_secs_f64();

        if elapsed >= 1.0 {
            let speed = bytes_since_last_update as f64 / elapsed;

            if let Ok(mut states) = inner.states.write() {
                if let Some(state) = states.get_mut(&task_id) {
                    state.downloaded = downloaded;
                    state.speed = speed;
                }
            }

            last_update = now;
            bytes_since_last_update = 0;
        }
    }

    file.flush().await?;

    if let Ok(mut states) = inner.states.write() {
        if let Some(state) = states.get_mut(&task_id) {
            state.downloaded = downloaded;
            state.speed = 0.0;
            state.is_finished = true;
        }
    }

    Ok(())
}

async fn stream_file_handler(
    State(inner): State<Arc<YouTubeStreamerInner>>,
    Path((task_id, _file_index)): Path<(usize, usize)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let (task, downloaded) = {
        let tasks = inner.tasks.read().unwrap();
        let states = inner.states.read().unwrap();

        let task = match tasks.get(&task_id) {
            Some(t) => t.clone(),
            None => return (StatusCode::NOT_FOUND, HeaderMap::new(), axum::body::Body::empty()).into_response(),
        };
        let downloaded = states.get(&task_id).map(|s| s.downloaded).unwrap_or(0);

        (task, downloaded)
    };

    let file_size = task.total_size;

    let mut start = 0u64;
    let mut end = file_size.saturating_sub(1);

    if let Some(range) = headers.get(header::RANGE).and_then(|r| r.to_str().ok()) {
        if let Some(range_val) = range.strip_prefix("bytes=") {
            let parts: Vec<&str> = range_val.split('-').collect();
            if parts.len() == 2 {
                start = parts[0].parse::<u64>().unwrap_or(0);
                end = parts[1].parse::<u64>().unwrap_or(file_size.saturating_sub(1));
                end = end.min(file_size.saturating_sub(1));
            }
        }
    }

    let content_length = end.saturating_sub(start) + 1;

    if start < downloaded {
        let local_end = end.min(downloaded.saturating_sub(1));
        let local_length = local_end.saturating_sub(start) + 1;

        let mut file = match File::open(&task.file_path).await {
            Ok(f) => f,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), axum::body::Body::empty()).into_response(),
        };

        if file.seek(SeekFrom::Start(start)).await.is_err() {
            return (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), axum::body::Body::empty()).into_response();
        }

        let stream = ReaderStream::new(file.take(local_length));
        let body = axum::body::Body::from_stream(stream);

        let mut response_headers = HeaderMap::new();
        response_headers.insert(header::CONTENT_TYPE, crate::streamer::get_mime_type_from_filename(&task.url));
        response_headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {}-{}/{}", start, local_end, file_size)).unwrap()
        );
        response_headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&local_length.to_string()).unwrap()
        );
        response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

        (StatusCode::PARTIAL_CONTENT, response_headers, body).into_response()
    } else {
        let range_str = format!("bytes={}-{}", start, end);

        let proxy_req = inner.client.get(&task.download_url)
            .header(header::RANGE, range_str)
            .send()
            .await;

        match proxy_req {
            Ok(resp) => {
                let mut response_headers = HeaderMap::new();
                response_headers.insert(header::CONTENT_TYPE, crate::streamer::get_mime_type_from_filename(&task.url));
                response_headers.insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, file_size)).unwrap()
                );
                response_headers.insert(
                    header::CONTENT_LENGTH,
                    HeaderValue::from_str(&content_length.to_string()).unwrap()
                );
                response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

                let stream = resp.bytes_stream();
                let body = axum::body::Body::from_stream(stream);
                (StatusCode::PARTIAL_CONTENT, response_headers, body).into_response()
            }
            Err(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, HeaderMap::new(), axum::body::Body::empty()).into_response()
            }
        }
    }
}
