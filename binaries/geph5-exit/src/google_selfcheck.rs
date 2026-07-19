//! Self-monitoring of how Google treats this exit's egress IP.
//!
//! A slow background loop (every ~10 min, jittered) probes Google Search and a
//! YouTube watch page, classifies the responses (ok / captcha / sign-in wall)
//! and records Google's perceived country. The existing 2s broker loop reads
//! the latest results and reports them as gauges, so a probe result is
//! re-sent every cycle — exactly what gauge semantics ("last value per
//! window") want.
//!
//! The probes are deliberately infrequent: probing Google often from an exit
//! IP could itself provoke the captchas we're measuring. Detection leans on
//! stable signals (redirects to /sorry/, HTTP 429, playability status) rather
//! than page markup wherever possible.

use std::{
    sync::Mutex,
    time::Duration,
};

use geph5_broker_protocol::StatEvent;

use crate::CONFIG_FILE;

const PROBE_INTERVAL: Duration = Duration::from_secs(600);
const PROBE_JITTER_SECS: u64 = 60;
/// Chrome-like headers. The TLS fingerprint is still reqwest's, so this
/// measures how Google treats automated-ish clients from this IP — a good
/// relative signal across exits and over time.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
const ACCEPT: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
/// Pre-accepted Google consent cookies, used to retry through the EU consent
/// wall (which is region-dependent, not a reputational block).
const CONSENT_COOKIES: &str = "CONSENT=YES+cb.20220301-11-p0.en+FX+700; SOCS=CAESEwgDEgk0ODE3Nzk3MjQaAmVuIAEaBgiA_LyaBg";

/// Benign queries, rotated so the probe doesn't send the exact same request
/// every 10 minutes forever.
const SEARCH_QUERIES: &[&str] = &[
    "weather tomorrow",
    "how to boil eggs",
    "current time",
    "population of france",
    "convert miles to km",
    "chocolate chip cookie recipe",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeStatus {
    Ok,
    Captcha,
    SigninWall,
}

/// Latest probe results. `None` = not yet known (never reported), so nothing
/// masquerades as "not blocked" before the first successful probe.
#[derive(Clone, Debug, Default)]
pub struct SelfCheckState {
    google_captcha: Option<bool>,
    youtube_captcha: Option<bool>,
    youtube_signin: Option<bool>,
    /// Google's perceived country (uppercase alpha2), from YouTube's ytcfg.
    google_country: Option<String>,
    /// Whether the last probe round failed to complete (network error etc).
    probe_error: Option<bool>,
}

static STATE: Mutex<SelfCheckState> = Mutex::new(SelfCheckState {
    google_captcha: None,
    youtube_captcha: None,
    youtube_signin: None,
    google_country: None,
    probe_error: None,
});

/// Gauges for the broker loop to append to its regular stats report.
pub fn google_selfcheck_stat_events(server_name: &str) -> Vec<StatEvent> {
    let state = STATE.lock().unwrap().clone();
    let exit_tag: &[(&str, &str)] = &[("exit", server_name)];
    let mut events = vec![];
    let as_gauge = |name: &str, val: Option<bool>| {
        val.map(|v| StatEvent::gauge(name, exit_tag, if v { 1.0 } else { 0.0 }))
    };
    events.extend(as_gauge("google_selfcheck_search_captcha", state.google_captcha));
    events.extend(as_gauge("google_selfcheck_youtube_captcha", state.youtube_captcha));
    events.extend(as_gauge("google_selfcheck_youtube_signin", state.youtube_signin));
    events.extend(as_gauge("google_selfcheck_error", state.probe_error));
    if let Some(country) = &state.google_country {
        let configured = CONFIG_FILE.wait().country.alpha2().to_uppercase();
        let mismatch = *country != configured;
        events.push(StatEvent::gauge(
            "google_selfcheck_geo_mismatch",
            &[("exit", server_name), ("google_country", country)],
            if mismatch { 1.0 } else { 0.0 },
        ));
    }
    events
}

pub async fn google_selfcheck_loop() -> anyhow::Result<()> {
    // Short initial delay so results appear soon after (re)deploys, without
    // racing startup.
    tokio::time::sleep(Duration::from_secs(fastrand::u64(15..60))).await;
    loop {
        match probe_round().await {
            Ok(round) => {
                tracing::info!(
                    google = debug(round.google),
                    youtube = debug(round.youtube),
                    google_country = debug(&round.google_country),
                    "selfcheck probe completed"
                );
                let mut state = STATE.lock().unwrap();
                state.google_captcha = Some(round.google == ProbeStatus::Captcha);
                state.youtube_captcha = Some(round.youtube == ProbeStatus::Captcha);
                state.youtube_signin = Some(round.youtube == ProbeStatus::SigninWall);
                if round.google_country.is_some() {
                    state.google_country = round.google_country;
                }
                state.probe_error = Some(false);
            }
            Err(err) => {
                tracing::warn!(err = debug(err), "selfcheck probe failed");
                // Only flag the error; block states keep their last known
                // value rather than being misreported.
                STATE.lock().unwrap().probe_error = Some(true);
            }
        }
        // PROBE_INTERVAL ± PROBE_JITTER_SECS
        let sleep_secs =
            PROBE_INTERVAL.as_secs() - PROBE_JITTER_SECS + fastrand::u64(0..PROBE_JITTER_SECS * 2);
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
    }
}

struct ProbeRound {
    google: ProbeStatus,
    youtube: ProbeStatus,
    google_country: Option<String>,
}

fn probe_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent(USER_AGENT)
        .build()?)
}

async fn probe_round() -> anyhow::Result<ProbeRound> {
    let client = probe_client()?;
    let google = probe_google_search(&client).await?;
    let (youtube, google_country) = probe_youtube(&client).await?;
    Ok(ProbeRound {
        google,
        youtube,
        google_country,
    })
}

struct FetchedPage {
    status: reqwest::StatusCode,
    final_url: reqwest::Url,
    body: String,
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    with_consent_cookie: bool,
) -> anyhow::Result<FetchedPage> {
    let mut req = client
        .get(url)
        .header("accept", ACCEPT)
        .header("accept-language", "en-US,en;q=0.9");
    if with_consent_cookie {
        req = req.header("cookie", CONSENT_COOKIES);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let final_url = resp.url().clone();
    let body = resp.text().await?;
    Ok(FetchedPage {
        status,
        final_url,
        body,
    })
}

fn is_sorry_page(page: &FetchedPage) -> bool {
    page.final_url.path().starts_with("/sorry")
        || page.status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || page.body.contains("detected unusual traffic")
}

fn is_consent_page(page: &FetchedPage) -> bool {
    page.final_url
        .host_str()
        .is_some_and(|h| h.contains("consent.google.com") || h.contains("consent.youtube.com"))
}

async fn probe_google_search(client: &reqwest::Client) -> anyhow::Result<ProbeStatus> {
    let query = SEARCH_QUERIES[fastrand::usize(0..SEARCH_QUERIES.len())];
    let url = format!(
        "https://www.google.com/search?q={}&hl=en",
        query.replace(' ', "+")
    );
    let mut page = fetch(client, &url, false).await?;
    if is_consent_page(&page) {
        page = fetch(client, &url, true).await?;
        if is_consent_page(&page) {
            // A consent wall is regional, not reputational; users click
            // through it. Not a block.
            tracing::debug!("selfcheck stuck on Google consent page even with cookie");
            return Ok(ProbeStatus::Ok);
        }
    }
    if is_sorry_page(&page) {
        return Ok(ProbeStatus::Captcha);
    }
    if page.status.is_success() {
        return Ok(ProbeStatus::Ok);
    }
    anyhow::bail!(
        "unclassifiable Google search response: status {} at {}",
        page.status,
        page.final_url
    )
}

async fn probe_youtube(
    client: &reqwest::Client,
) -> anyhow::Result<(ProbeStatus, Option<String>)> {
    let url = format!(
        "https://www.youtube.com/watch?v={}&hl=en",
        CONFIG_FILE.wait().google_selfcheck_video
    );
    let mut page = fetch(client, &url, false).await?;
    if is_consent_page(&page) {
        page = fetch(client, &url, true).await?;
    }
    let country = extract_yt_country(&page.body);
    if is_sorry_page(&page) {
        return Ok((ProbeStatus::Captcha, country));
    }
    if is_yt_signin_wall(&page.body) {
        return Ok((ProbeStatus::SigninWall, country));
    }
    if page.status.is_success() {
        return Ok((ProbeStatus::Ok, country));
    }
    anyhow::bail!(
        "unclassifiable YouTube response: status {} at {}",
        page.status,
        page.final_url
    )
}

fn is_yt_signin_wall(body: &str) -> bool {
    // The watch-page player response carries LOGIN_REQUIRED when YouTube
    // demands sign-in ("Sign in to confirm you're not a bot").
    body.contains("\"status\":\"LOGIN_REQUIRED\"")
        || body.contains("Sign in to confirm you\u{2019}re not a bot")
}

/// Pull Google's perceived country out of YouTube's embedded ytcfg
/// (`"GL":"XX"`, with `"gl":"xx"` in INNERTUBE_CONTEXT as fallback).
fn extract_yt_country(body: &str) -> Option<String> {
    for needle in ["\"GL\":\"", "\"gl\":\""] {
        if let Some(pos) = body.find(needle) {
            let cc = body.get(pos + needle.len()..pos + needle.len() + 2)?;
            if cc.len() == 2 && cc.chars().all(|c| c.is_ascii_alphabetic()) {
                return Some(cc.to_uppercase());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_country_from_ytcfg() {
        assert_eq!(
            extract_yt_country(r#"ytcfg.set({"GL":"NL","HL":"en"})"#),
            Some("NL".to_string())
        );
        assert_eq!(
            extract_yt_country(r#""INNERTUBE_CONTEXT":{"client":{"gl":"de"}}"#),
            Some("DE".to_string())
        );
        assert_eq!(extract_yt_country("no country here"), None);
    }

    #[test]
    fn detects_signin_wall() {
        assert!(is_yt_signin_wall(
            r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED"}}"#
        ));
        assert!(!is_yt_signin_wall(
            r#"{"playabilityStatus":{"status":"OK"}}"#
        ));
    }
}
