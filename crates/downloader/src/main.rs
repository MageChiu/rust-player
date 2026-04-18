#![allow(non_snake_case)]

use dioxus::desktop::{Config, WindowBuilder};
use dioxus::prelude::*;
use futures_util::StreamExt;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session};

#[derive(Clone, PartialEq)]
enum DownloadState {
    Idle,
    Downloading {
        progress: f64,
        downloaded: u64,
        total: u64,
    },
    Finished,
    Error(String),
}

fn main() {
    let window = WindowBuilder::new().with_title("磁力链接下载器");
    let config = Config::new().with_window(window);
    LaunchBuilder::desktop().with_cfg(config).launch(App);
}

#[component]
fn App() -> Element {
    let mut magnet_link = use_signal(|| String::new());
    let default_dir = dirs::download_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| String::from("."));
    let mut save_path = use_signal(|| default_dir);
    let mut download_state = use_signal(|| DownloadState::Idle);

    let downloader = use_coroutine(
        move |mut rx: UnboundedReceiver<(String, String)>| async move {
            while let Some((magnet, path)) = rx.next().await {
                download_state.set(DownloadState::Downloading {
                    progress: 0.0,
                    downloaded: 0,
                    total: 0,
                });

                let result = async {
                let session = Session::new(path.into()).await.map_err(|e| e.to_string())?;
                let add_torrent = AddTorrent::from_url(&magnet);
                let mut opts = AddTorrentOptions::default();
                opts.overwrite = true;
                
                let managed_torrent = match session.add_torrent(add_torrent, Some(opts)).await.map_err(|e| e.to_string())? {
                    AddTorrentResponse::Added(_, handle) => handle,
                    AddTorrentResponse::AlreadyManaged(_, handle) => handle,
                    AddTorrentResponse::ListOnly(_) => return Err("不支持 List only 模式".to_string()),
                };

                let mut completion = Box::pin(managed_torrent.wait_until_completed());

                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                            let stats = managed_torrent.stats();
                            let total = stats.total_bytes;
                            let downloaded = stats.progress_bytes;
                            let progress = if total > 0 {
                                (downloaded as f64 / total as f64) * 100.0
                            } else {
                                0.0
                            };
                            download_state.set(DownloadState::Downloading { progress, downloaded, total });
                        }
                        _ = &mut completion => {
                            // Finished
                            download_state.set(DownloadState::Finished);
                            break;
                        }
                    }
                }
                Ok::<(), String>(())
            }.await;

                if let Err(e) = result {
                    download_state.set(DownloadState::Error(e));
                }
            }
        },
    );

    rsx! {
        div {
            style: "padding: 20px; font-family: sans-serif; display: flex; flex-direction: column; gap: 15px; height: 100vh; background-color: #f5f5f5;",
            h1 { style: "text-align: center; color: #333;", "磁力链接下载器" }

            div {
                style: "display: flex; flex-direction: column; gap: 5px;",
                label { style: "font-weight: bold; color: #555;", "保存路径:" }
                input {
                    style: "padding: 8px; border: 1px solid #ccc; border-radius: 4px;",
                    value: "{save_path}",
                    oninput: move |evt| save_path.set(evt.value().clone()),
                }
            }

            div {
                style: "display: flex; flex-direction: column; gap: 5px;",
                label { style: "font-weight: bold; color: #555;", "磁力链接:" }
                input {
                    style: "padding: 8px; border: 1px solid #ccc; border-radius: 4px;",
                    value: "{magnet_link}",
                    oninput: move |evt| magnet_link.set(evt.value().clone()),
                    placeholder: "magnet:?xt=urn:btih:..."
                }
            }

            button {
                style: "padding: 12px; background: #007bff; color: white; border: none; border-radius: 5px; cursor: pointer; font-size: 16px; font-weight: bold;",
                onclick: move |_| {
                    if !magnet_link.read().is_empty() && !save_path.read().is_empty() {
                        downloader.send((magnet_link.read().clone(), save_path.read().clone()));
                    }
                },
                "开始下载"
            }

            div {
                style: "margin-top: 20px; padding: 20px; background: white; border: 1px solid #ddd; border-radius: 8px; box-shadow: 0 2px 4px rgba(0,0,0,0.1);",
                match &*download_state.read() {
                    DownloadState::Idle => rsx! { span { style: "color: #666;", "准备就绪，输入链接开始下载。" } },
                    DownloadState::Downloading { progress, downloaded, total } => rsx! {
                        div {
                            style: "display: flex; flex-direction: column; gap: 10px;",
                            div {
                                style: "display: flex; justify-content: space-between; font-weight: bold; color: #333;",
                                span { "下载进度: {progress:.2}%" }
                                span { "{downloaded} / {total} bytes" }
                            }
                            progress {
                                value: "{progress}",
                                max: "100",
                                style: "width: 100%; height: 20px;"
                            }
                        }
                    },
                    DownloadState::Finished => rsx! { span { style: "color: #28a745; font-weight: bold;", "下载完成！" } },
                    DownloadState::Error(err) => rsx! { span { style: "color: #dc3545; font-weight: bold;", "错误: {err}" } },
                }
            }
        }
    }
}
