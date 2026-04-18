use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, REFERER, USER_AGENT};
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<()> {
    let bvid = "BV1uMQwB7EAi";
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".parse().unwrap());
    headers.insert(REFERER, "https://www.bilibili.com".parse().unwrap());
    
    let client = reqwest::Client::builder().default_headers(headers).build()?;
    
    let view_url = format!("https://api.bilibili.com/x/web-interface/view?bvid={}", bvid);
    let view_resp: Value = client.get(&view_url).send().await?.json().await?;
    
    if view_resp["code"].as_i64().unwrap_or(-1) != 0 {
        println!("View API Error: {}", view_resp);
        return Ok(());
    }
    
    let cid = view_resp["data"]["cid"].as_i64().context("no cid")?;
    println!("CID: {}", cid);
    
    // fnval=1 is for mp4, fnval=16 is for dash
    let play_url = format!("https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&qn=80&fnval=1", bvid, cid);
    let play_resp: Value = client.get(&play_url).send().await?.json().await?;
    println!("Play Resp Code: {}", play_resp["code"]);
    
    if let Some(durl) = play_resp["data"]["durl"].as_array() {
        let download_url = durl[0]["url"].as_str().unwrap();
        println!("URL: {}", download_url);
        
        let head_resp = client.head(download_url).send().await?;
        println!("HEAD status: {}", head_resp.status());
    } else {
        println!("No durl found! Data: {}", play_resp["data"]);
    }
    
    Ok(())
}
