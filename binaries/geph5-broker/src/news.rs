use std::time::Duration;

use anyhow::Context;
use serde_json::json;

async fn fetch_news() -> anyhow::Result<()> {
    // Replace with your actual OpenAI API key
    let api_key = "YOUR_API_KEY_HERE";

    // Create a reqwest Client
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let announcements = client.get(format!("https://gist.githubusercontent.com/nullchinchilla/b99b14bbc6090a423c62c6b29b9d06ca/raw/612fcb51be5700807b035056886124f7279713cb/geph-announcements.md?bust={}", rand::random::<u128>())).send().await?.text().await?;
    // Create the JSON request body
    let request_body = json!({
        "model": "gpt-4o",

        "messages": [
            {
                "role": "system",
                "content": include_str!("news_prompt.txt")
            },
            {
                "role": "user",
                "content": announcements
            }
        ],

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
    let response = response["choices"][0]["message"]["content"]
        .as_str()
        .context("no response string")?;

    Ok(())
}
