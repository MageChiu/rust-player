use anyhow::{Context, Result};
use std::collections::HashMap;
use axum::http::HeaderValue;
use std::sync::Arc;

pub fn get_mime_type_from_filename(filename: &str) -> HeaderValue {
    let mime = mime_guess::from_path(filename).first_or_octet_stream();
    HeaderValue::from_str(mime.as_ref()).unwrap_or(HeaderValue::from_static("video/mp4"))
}
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct DownloadStats {
    pub progress_percent: f32,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub download_speed: f64,
    pub upload_speed: f64,
    pub is_finished: bool,
    pub error: Option<String>,
    pub peers_count: usize,
}

impl Default for DownloadStats {
    fn default() -> Self {
        Self {
            progress_percent: 0.0,
            downloaded_bytes: 0,
            total_bytes: 0,
            download_speed: 0.0,
            upload_speed: 0.0,
            is_finished: false,
            error: None,
            peers_count: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub index: usize,
    pub name: String,
    pub size: u64,
    pub downloaded: u64,
    pub is_video: bool,
}

#[async_trait::async_trait]
pub trait MediaStreamer: Send + Sync {
    fn supports(&self, url: &str) -> bool;

    async fn add_task(&self, url: &str) -> Result<usize>;

    fn get_stats(&self, task_id: usize) -> DownloadStats;

    fn get_files(&self, task_id: usize) -> Result<Vec<FileInfo>>;

    async fn get_stream_url(&self, task_id: usize, file_index: usize) -> Result<String>;

    async fn pause(&self, task_id: usize) -> Result<()>;

    async fn resume(&self, task_id: usize) -> Result<()>;

    async fn remove(&self, task_id: usize, delete_files: bool) -> Result<()>;

    fn get_main_video_file(&self, task_id: usize) -> Result<FileInfo> {
        let video_files: Vec<_> = self
            .get_files(task_id)?
            .into_iter()
            .filter(|f| f.is_video)
            .collect();
        
        video_files
            .into_iter()
            .max_by_key(|f| f.size)
            .context("No video file found")
    }
}

pub struct StreamerManager {
    backends: Vec<Box<dyn MediaStreamer>>,
    task_map: RwLock<HashMap<usize, (usize, usize)>>,
    next_task_id: std::sync::atomic::AtomicUsize,
}

impl StreamerManager {
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
            task_map: RwLock::new(HashMap::new()),
            next_task_id: std::sync::atomic::AtomicUsize::new(1),
        }
    }

    pub fn register_backend(&mut self, backend: impl MediaStreamer + 'static) {
        self.backends.push(Box::new(backend));
    }

    pub async fn add_task(&self, url: &str) -> Result<usize> {
        for (i, backend) in self.backends.iter().enumerate() {
            if backend.supports(url) {
                let backend_task_id = backend.add_task(url).await?;
                let global_id = self
                    .next_task_id
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                
                self.task_map
                    .write()
                    .await
                    .insert(global_id, (i, backend_task_id));
                return Ok(global_id);
            }
        }
        anyhow::bail!("不支持的链接格式或协议: {}", url);
    }

    async fn get_backend(&self, global_id: usize) -> Result<(&Box<dyn MediaStreamer>, usize)> {
        let map = self.task_map.read().await;
        if let Some(&(backend_idx, backend_task_id)) = map.get(&global_id) {
            if let Some(backend) = self.backends.get(backend_idx) {
                return Ok((backend, backend_task_id));
            }
        }
        anyhow::bail!("无效的任务 ID: {}", global_id);
    }

    pub async fn get_stats(&self, global_id: usize) -> DownloadStats {
        if let Ok((backend, backend_task_id)) = self.get_backend(global_id).await {
            backend.get_stats(backend_task_id)
        } else {
            DownloadStats {
                error: Some("任务未找到".to_string()),
                ..Default::default()
            }
        }
    }

    pub async fn get_files(&self, global_id: usize) -> Result<Vec<FileInfo>> {
        let (backend, backend_task_id) = self.get_backend(global_id).await?;
        backend.get_files(backend_task_id)
    }

    pub async fn get_stream_url(&self, global_id: usize, file_index: usize) -> Result<String> {
        let (backend, backend_task_id) = self.get_backend(global_id).await?;
        backend.get_stream_url(backend_task_id, file_index).await
    }

    pub async fn pause(&self, global_id: usize) -> Result<()> {
        let (backend, backend_task_id) = self.get_backend(global_id).await?;
        backend.pause(backend_task_id).await
    }

    pub async fn resume(&self, global_id: usize) -> Result<()> {
        let (backend, backend_task_id) = self.get_backend(global_id).await?;
        backend.resume(backend_task_id).await
    }

    pub async fn remove(&self, global_id: usize, delete_files: bool) -> Result<()> {
        let (backend, backend_task_id) = self.get_backend(global_id).await?;
        let res = backend.remove(backend_task_id, delete_files).await;
        if res.is_ok() {
            self.task_map.write().await.remove(&global_id);
        }
        res
    }

    pub async fn get_main_video_file(&self, global_id: usize) -> Result<FileInfo> {
        let (backend, backend_task_id) = self.get_backend(global_id).await?;
        backend.get_main_video_file(backend_task_id)
    }
}
