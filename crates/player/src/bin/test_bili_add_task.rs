use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, REFERER, USER_AGENT};
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = "https://www.bilibili.com/video/BV1uMQwB7EAi/?spm_id_from=333.337.search-card.all.click&vd_source=2c6659819a74487da2c4783813500a8b";
    
    let bvid = url
        .split(&['/', '?'][..])
        .find(|s| s.starts_with("BV"))
        .context("Failed to extract BVID from URL")?;
        
    println!("Extracted bvid: {}", bvid);
    
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".parse().unwrap());
    headers.insert(REFERER, "https://www.bilibili.com".parse().unwrap());

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
        
    let view_url = format!("https://api.bilibili.com/x/web-interface/view?bvid={}", bvid);
    let view_resp: Value = client.get(&view_url).send().await?.json().await?;
    
    let code = view_resp["code"].as_i64().unwrap_or(-1);
    if code != 0 {
        println!("Bilibili API error: code {}", code);
        return Ok(());
    }

    let cid = view_resp["data"]["cid"].as_i64().context("Failed to get cid")?;
    let title = view_resp["data"]["title"]
        .as_str()
        .unwrap_or("bilibili_video")
        .replace(&['/', '\\', ':', '*', '?', '"', '<', '>', '|'][..], "_");

    println!("Got CID: {}, Title: {}", cid, title);

    let play_url = format!(
        "https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&qn=80&fnval=1",
        bvid, cid
    );
    let play_resp: Value = client.get(&play_url).send().await?.json().await?;

    let play_code = play_resp["code"].as_i64().unwrap_or(-1);
    if play_code != 0 {
        println!("Bilibili playurl API error: code {}", play_code);
        return Ok(());
    }

    let download_url = play_resp["data"]["durl"][0]["url"]
        .as_str()
        .context("Failed to get download url");
        
    println!("Download URL Result: {:?}", download_url.is_ok());

    Ok(())
}
