use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{PathBuf, Path as StdPath};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;
use tracing::{error, info};

#[cfg(feature = "ffmpeg")]
use ffmpeg_the_third as ffmpeg;

use crate::streamer::{DownloadStats, FileInfo, MediaStreamer};

#[derive(Clone)]
struct LocalTask {
    file_path: PathBuf,
    file_name: String,
    total_size: u64,
    duration: f64,
}

struct LocalFileStreamerInner {
    tasks: RwLock<HashMap<usize, LocalTask>>,
    next_id: AtomicUsize,
}

pub struct LocalFileStreamer {
    inner: Arc<LocalFileStreamerInner>,
    server_port: tokio::sync::RwLock<u16>,
}

impl LocalFileStreamer {
    pub async fn new() -> Result<Self> {
        #[cfg(feature = "ffmpeg")]
        {
            ffmpeg::init().unwrap_or_else(|e| error!("Failed to init ffmpeg: {}", e));
        }

        let inner = Arc::new(LocalFileStreamerInner {
            tasks: RwLock::new(HashMap::new()),
            next_id: AtomicUsize::new(1),
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

        info!("LocalFileStreamer server started on port {}", actual_port);
        *self.server_port.write().await = actual_port;

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                error!("LocalFileStreamer server error: {}", e);
            }
        });

        Ok(())
    }
}

#[async_trait::async_trait]
impl MediaStreamer for LocalFileStreamer {
    fn supports(&self, url: &str) -> bool {
        url.starts_with("file://") || StdPath::new(url).exists()
    }

    async fn add_task(&self, url: &str) -> Result<usize> {
        let path_str = url.strip_prefix("file://").unwrap_or(url);
        let file_path = PathBuf::from(path_str);

        if !file_path.exists() {
            anyhow::bail!("Local file does not exist");
        }

        let metadata = tokio::fs::metadata(&file_path).await?;
        let total_size = metadata.len();
        let file_name = file_path.file_name().unwrap_or_default().to_string_lossy().to_string();

        let mut duration = 0.0;

        #[cfg(feature = "ffmpeg")]
        {
            if let Ok(ctx) = ffmpeg::format::input(&file_path) {
                let dur = ctx.duration() as f64 / ffmpeg::ffi::AV_TIME_BASE as f64;
                duration = dur.max(0.0);
                info!("Local file duration via ffmpeg-next: {}s", duration);
            }
        }

        let task_id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let task = LocalTask {
            file_path,
            file_name,
            total_size,
            duration,
        };

        self.inner.tasks.write().unwrap().insert(task_id, task);
        info!("LocalFileStreamer: 添加本地文件 {}", task_id);

        Ok(task_id)
    }

    fn get_stats(&self, task_id: usize) -> DownloadStats {
        let total_size = {
            let tasks = self.inner.tasks.read().unwrap();
            tasks.get(&task_id).map(|t| t.total_size).unwrap_or(0)
        };

        DownloadStats {
            progress_percent: 100.0,
            downloaded_bytes: total_size,
            total_bytes: total_size,
            download_speed: 0.0,
            upload_speed: 0.0,
            is_finished: true,
            error: None,
            peers_count: 0,
        }
    }

    fn get_files(&self, task_id: usize) -> Result<Vec<FileInfo>> {
        let (file_name, total_size) = {
            let tasks = self.inner.tasks.read().unwrap();
            let task = tasks.get(&task_id).context("Task not found")?;
            (task.file_name.clone(), task.total_size)
        };

        Ok(vec![FileInfo {
            index: 0,
            name: file_name,
            size: total_size,
            downloaded: total_size,
            is_video: true,
        }])
    }

    async fn get_stream_url(&self, task_id: usize, file_index: usize) -> Result<String> {
        let port = *self.server_port.read().await;
        Ok(format!("http://127.0.0.1:{}/stream/{}/{}", port, task_id, file_index))
    }

    async fn pause(&self, _task_id: usize) -> Result<()> { Ok(()) }
    async fn resume(&self, _task_id: usize) -> Result<()> { Ok(()) }
    async fn remove(&self, task_id: usize, _delete_files: bool) -> Result<()> {
        self.inner.tasks.write().unwrap().remove(&task_id);
        Ok(())
    }
}

async fn stream_file_handler(
    State(inner): State<Arc<LocalFileStreamerInner>>,
    Path((task_id, _file_index)): Path<(usize, usize)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let task = {
        let tasks = inner.tasks.read().unwrap();
        match tasks.get(&task_id) {
            Some(t) => t.clone(),
            None => return (StatusCode::NOT_FOUND, HeaderMap::new(), axum::body::Body::empty()).into_response(),
        }
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

    let local_length = end.saturating_sub(start) + 1;

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
    
    let content_type = crate::streamer::get_mime_type_from_filename(&task.file_name);
    
    response_headers.insert(header::CONTENT_TYPE, content_type);
    response_headers.insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, file_size)).unwrap()
    );
    response_headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&local_length.to_string()).unwrap()
    );
    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    (StatusCode::PARTIAL_CONTENT, response_headers, body).into_response()
}
