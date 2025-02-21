use std::time::{Duration, SystemTime};

use anyhow::Context;
use geph5_broker_protocol::NewsItem;

use serde_json::json;

use crate::CONFIG_FILE;

pub async fn fetch_news(lang_code: &str) -> anyhow::Result<Vec<NewsItem>> {
    // Validate language code
    if lang_code != "en"
        && lang_code != "zh-CN"
        && lang_code != "zh-TW"
        && lang_code != "fa"
        && lang_code != "ru"
    {
        anyhow::bail!("unsupported language {}", lang_code)
    }

    // Path to cache file
    let cache_path = format!("/tmp/geph5_news_{lang_code}.json");

    // If the file exists and is not older than 1 hour, use the cached value
    if let Ok(metadata) = smol::fs::metadata(&cache_path).await {
        if let Ok(modified) = metadata.modified() {
            if let Ok(elapsed) = SystemTime::now().duration_since(modified) {
                if elapsed < Duration::from_secs(3600) {
                    // Read from cache
                    let cached_data = smol::fs::read_to_string(&cache_path).await?;
                    let cached_news: Vec<NewsItem> = serde_json::from_str(&cached_data)?;
                    return Ok(cached_news);
                }
            }
        }
    }

    // Otherwise, we need to fetch fresh data. Use a mutex to ensure no duplicate work.
    static MUTEX: smol::lock::Mutex<()> = smol::lock::Mutex::new(());
    let _guard = MUTEX.lock().await;

    // Replace with your actual OpenAI API key
    let api_key = CONFIG_FILE.wait().openai_key.clone();

    // Create a reqwest Client
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    // Fetch announcements
    let announcements = client
        .get(format!(
            "https://gist.githubusercontent.com/nullchinchilla/b99b14bbc6090a423c62c6b29b9d06ca/raw/612fcb51be5700807b035056886124f7279713cb/geph-announcements.md?bust={}",
            rand::random::<u128>()
        ))
        .send().await?
        .text().await?;

    // Create the JSON request body
    let request_body = json!({
        "model": "o3-mini",
        "reasoning_effort": "low",
        "messages": [
            {
                "role": "system",

                "content": include_str!("news_prompt.txt").replace("$LANGUAGE", lang_code)
            },
            {
                "role": "user",
                "content": announcements
            }
        ],
        "response_format": { "type": "json_object" }
    });

    // Make the POST request to OpenAI
    let response = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&request_body)
        .send()
        .await?;

    // Parse the response as JSON
    let response_json: serde_json::Value = response.json().await?;
    dbg!(&response_json);
    let content_str = response_json["choices"][0]["message"]["content"]
        .as_str()
        .context("no response string")?;
    let parsed = serde_json::from_str::<serde_json::Value>(content_str)?;

    // Convert to Vec<NewsItem>
    let news_items: Vec<NewsItem> = serde_json::from_value(parsed["news"].clone())?;

    // Write to cache (overwrite if it exists or create if it doesn't)
    let serialized = serde_json::to_string(&news_items)?;
    smol::fs::write(cache_path, serialized).await?;

    Ok(news_items)
}
