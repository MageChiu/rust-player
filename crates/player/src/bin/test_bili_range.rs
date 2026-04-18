use reqwest::header::{HeaderMap, REFERER, USER_AGENT, RANGE};
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bvid = "BV1uMQwB7EAi";
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36".parse().unwrap());
    headers.insert(REFERER, "https://www.bilibili.com".parse().unwrap());
    
    let client = reqwest::Client::builder().default_headers(headers).build()?;
    
    let view_url = format!("https://api.bilibili.com/x/web-interface/view?bvid={}", bvid);
    let view_resp: Value = client.get(&view_url).send().await?.json().await?;
    let cid = view_resp["data"]["cid"].as_i64().unwrap();
    
    let play_url = format!("https://api.bilibili.com/x/player/playurl?bvid={}&cid={}&qn=80&fnval=1", bvid, cid);
    let play_resp: Value = client.get(&play_url).send().await?.json().await?;
    let download_url = play_resp["data"]["durl"][0]["url"].as_str().unwrap();
    
    let resp = client.get(download_url).header(RANGE, "bytes=0-1000").send().await?;
    println!("Range response status: {}", resp.status());
    let bytes = resp.bytes().await?;
    println!("Range response length: {}", bytes.len());
    
    Ok(())
}
