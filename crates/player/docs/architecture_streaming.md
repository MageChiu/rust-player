# 多协议流媒体边下边播架构设计

为了让 Rust Player 支持除了 BitTorrent (磁力链接) 之外的更多流媒体和下载协议（如 HTTP、HTTPS 直链、HLS/M3U8、IPFS 等），我们需要将现有的下载与流媒体服务解耦，引入一个基于 Trait 的插件化架构。

## 1. 核心问题抽象

无论是什么协议，"边下边播" 服务的核心生命周期是相同的：
1. **任务添加 (Add Task)**：接收一个 URL，解析并开始处理。
2. **状态轮询 (Stats Polling)**：获取下载进度、速度等数据。
3. **媒体发现 (File Discovery)**：获取任务包含的文件列表并找出主媒体文件。
4. **流媒体供给 (Streaming)**：为本地播放器提供可以连续读取（或支持 Range Seek）的 HTTP 接口。

## 2. 架构设计：`MediaStreamer` Trait

为了支持多协议，我们将 `TorrentStreamer` 重构并抽象出一个核心 trait `MediaStreamer`：

```rust
#[async_trait::async_trait]
pub trait MediaStreamer: Send + Sync {
    /// 识别此处理器是否支持给定的 URL（例如 `magnet:?`, `http://`, `ipfs://`）
    fn supports(&self, url: &str) -> bool;

    /// 添加任务并返回全局唯一的任务 ID
    async fn add_task(&self, url: &str) -> Result<usize>;

    /// 获取任务的状态
    fn get_stats(&self, task_id: usize) -> Result<DownloadStats>;

    /// 获取任务中的文件列表
    fn get_files(&self, task_id: usize) -> Result<Vec<FileInfo>>;

    /// 获取文件的本地播放 URL (通常是类似 http://127.0.0.1:xxx/stream/...)
    async fn get_stream_url(&self, task_id: usize, file_index: usize) -> Result<String>;
    
    /// 控制指令
    async fn pause(&self, task_id: usize) -> Result<()>;
    async fn resume(&self, task_id: usize) -> Result<()>;
    async fn remove(&self, task_id: usize, delete_files: bool) -> Result<()>;
}
```

## 3. 支持的扩展协议规划

有了 `MediaStreamer` Trait，我们可以依次实现以下后端插件：

### 3.1 HTTP / HTTPS 直链流媒体后端 (`HttpStreamer`)
- **适用场景**：用户输入 `http://example.com/movie.mp4`
- **实现原理**：
  - 基于 `reqwest`，发起带 `Range` header 的请求。
  - **按需下载**：本地 HTTP Server 接收到播放器的 Range 请求后，实时代理请求源站，并将获取的数据写入本地文件缓存。
  - **块管理**：在本地维护一个以稀疏文件（Sparse File）或块映射（Block Map）形式存在的缓存。

### 3.2 M3U8 / HLS 后端 (`HlsStreamer`)
- **适用场景**：用户输入 `http://example.com/index.m3u8`
- **实现原理**：
  - 解析 M3U8 播放列表。
  - 后台开启一个任务，按顺序预加载 `.ts` 切片文件。
  - 本地 HTTP Server 直接代理 `.m3u8` 请求，并在播放器请求 `.ts` 时提供预加载或实时下载的切片内容。

### 3.3 IPFS 后端 (`IpfsStreamer`)
- **适用场景**：用户输入 `ipfs://Qm...`
- **实现原理**：
  - 可以内嵌一个轻量级 IPFS 节点（如 `rust-ipfs` 库），或者通过后台启动一个 IPFS daemon 服务。
  - 将通过 CID 请求的文件封装成类似于 BitTorrent 的流。

### 3.4 视频网站解析后端 (`YtDlpStreamer`)
- **适用场景**：用户输入 YouTube、Bilibili 等网站链接。
- **实现原理**：
  - 在后台调用 `yt-dlp` 获取实际的 `m3u8` 或 `dash` 音视频分离直链。
  - 将获取到的真实直链再交由 `HttpStreamer` 或 `HlsStreamer` 处理。

## 4. UI 层适配

UI 层无需知道后端具体使用的什么协议。我们只需要：
1. 实现一个管理器 `StreamerManager` 统一管理所有的实现：
```rust
pub struct StreamerManager {
    backends: Vec<Box<dyn MediaStreamer>>,
}

impl StreamerManager {
    pub async fn add_task(&self, url: &str) -> Result<usize> {
        for backend in &self.backends {
            if backend.supports(url) {
                return backend.add_task(url).await;
            }
        }
        anyhow::bail!("No supported streamer backend for this URL");
    }
}
```
2. 输入框放宽限制：不再硬编码判断 `link.starts_with("magnet:?")`，而是直接交给 `StreamerManager` 处理。

## 5. 改造步骤建议

1. **第一阶段 (基础抽象)**：
   - 提取 `DownloadStats` 和 `FileInfo` 结构。
   - 创建 `MediaStreamer` Trait，并让现有的 `TorrentStreamer` 实现该 Trait。
   - 在 UI 层注入 `StreamerManager`。

2. **第二阶段 (HTTP 边下边播实现)**：
   - 引入 `reqwest` 和分块下载逻辑。
   - 实现 `HttpStreamer`。
   - 解除 UI 的 `magnet:?` 强制前缀校验，支持 `http(s)://` 直链。

3. **第三阶段 (HLS / m3u8)**：
   - 引入 m3u8 解析器（如 `m3u8-rs`）。
   - 实现切片下载和流式代理。
