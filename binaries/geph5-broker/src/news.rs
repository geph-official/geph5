use std::time::Duration;

use anyhow::Context;
use geph5_broker_protocol::NewsItem;
use serde_json::json;

use crate::CONFIG_FILE;

pub async fn fetch_news(lang_code: &str) -> anyhow::Result<Vec<NewsItem>> {
    if lang_code != "en"
        && lang_code != "zh-CN"
        && lang_code != "zh-TW"
        && lang_code != "fa"
        && lang_code != "ru"
    {
        anyhow::bail!("unsupported language {}", lang_code)
    }
    // Replace with your actual OpenAI API key
    let api_key = CONFIG_FILE.wait().openai_key.clone();

    // Create a reqwest Client
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let announcements = client.get(format!("https://gist.githubusercontent.com/nullchinchilla/b99b14bbc6090a423c62c6b29b9d06ca/raw/612fcb51be5700807b035056886124f7279713cb/geph-announcements.md?bust={}", rand::random::<u128>())).send().await?.text().await?;
    // Create the JSON request body
    let request_body = json!({
        "model": "o3-mini",
        "reasoning_effort": "low",
        "messages": [
            {
                "role": "system",
                "content": include_str!("news_prompt.txt").replace("$LANGUAGE", "uk-UA")
            },
            {
                "role": "user",
                "content": announcements
            }
        ],

        "response_format": { "type": "json_object" }

    });

    // Make the POST request
    let response = client
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&request_body)
        .send()
        .await?;

    // Parse the response text (or handle JSON as needed)
    let response: serde_json::Value = response.json().await?;
    dbg!(&response);
    let response = response["choices"][0]["message"]["content"]
        .as_str()
        .context("no response string")?;
    let response: serde_json::Value = serde_json::from_str(response)?;

    Ok(serde_json::from_value(response["news"].clone())?)
}

// #[cfg(test)]
// mod tests {
//     use super::fetch_news;

//     #[test]
//     fn test_news() {
//         smolscale::block_on(async move {
//             let res = fetch_news().await.unwrap();
//             dbg!(res);
//         })
//     }
// }
