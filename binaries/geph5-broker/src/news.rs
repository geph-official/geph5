use std::time::{Duration, SystemTime};

use anyhow::Context;
use geph5_broker_protocol::NewsItem;
use serde::{Deserialize, Serialize};
use serde_json::json;
use smol::lock::Mutex;
use sqlx::types::chrono::NaiveDate;

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

    // First, try to read the cached data regardless of age
    let cached_news = match smol::fs::read_to_string(&cache_path).await {
        Ok(cached_data) => match serde_json::from_str::<Vec<NewsItem>>(&cached_data) {
            Ok(news) => Some(news),
            Err(_) => None,
        },
        Err(_) => None,
    };

    // Check if we need to refresh the data
    let needs_refresh = match smol::fs::metadata(&cache_path).await {
        Ok(metadata) => match metadata.modified() {
            Ok(modified) => match SystemTime::now().duration_since(modified) {
                Ok(elapsed) => elapsed >= Duration::from_secs(3600),
                Err(_) => true,
            },
            Err(_) => true,
        },
        Err(_) => true,
    };

    // If we need to refresh and have cached data, spawn a task to update the cache
    // and return the cached data immediately
    if needs_refresh {
        if let Some(news) = cached_news.clone() {
            // We have stale data, so let's refresh in the background
            let cache_path_clone = cache_path.clone();
            let lang_code_clone = lang_code.to_string();

            // Use a static mutex to prevent multiple simultaneous refreshes for the same language
            static REFRESH_MUTEX: Mutex<()> = Mutex::new(());

            smolscale::spawn(async move {
                // Try to acquire the lock. If we can't, it means another refresh is in progress
                if let Some(guard) = REFRESH_MUTEX.try_lock() {
                    if let Err(e) = refresh_news_cache(&lang_code_clone, &cache_path_clone).await {
                        // Log the error but don't propagate it
                        tracing::warn!("Background news refresh failed: {}", e);
                    }
                    drop(guard);
                }
            })
            .detach();

            return Ok(news);
        }
    } else if let Some(news) = cached_news {
        // Cache is fresh, return it directly
        return Ok(news);
    }

    // If we get here, either:
    // 1. We need to refresh and don't have cached data
    // 2. Cache is stale and we have no data
    // So we need to fetch and block
    refresh_news_cache(lang_code, &cache_path).await
}

// Separate function to refresh the news cache
async fn refresh_news_cache(lang_code: &str, cache_path: &str) -> anyhow::Result<Vec<NewsItem>> {
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
    let content_str = response_json["choices"][0]["message"]["content"]
        .as_str()
        .context("no response string")?;
    let parsed = serde_json::from_str::<serde_json::Value>(content_str)?;

    // Convert to Vec<NewsItem>
    let news_items: Vec<PreNewsItem> = serde_json::from_value(parsed["news"].clone())?;
    let news_items = news_items
        .into_iter()
        .map(|item| NewsItem {
            title: item.title,
            date_unix: date_to_unix_timestamp(&item.date) as _,
            contents: item.contents,
        })
        .collect();

    // Write to cache (overwrite if it exists or create if it doesn't)
    let serialized = serde_json::to_string(&news_items)?;
    smol::fs::write(cache_path, serialized).await?;

    Ok(news_items)
}

fn date_to_unix_timestamp(date_str: &str) -> i64 {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").expect("Failed to parse date");
    date.and_hms_opt(0, 0, 0)
        .expect("Invalid time")
        .and_utc()
        .timestamp()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct PreNewsItem {
    pub title: String,
    pub date: String,
    pub contents: String,
}
