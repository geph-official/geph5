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
pub fn google_selfcheck_stat_events(server_name: &str, level: &str) -> Vec<StatEvent> {
    let state = STATE.lock().unwrap().clone();
    let exit_tag: &[(&str, &str)] = &[("exit", server_name), ("level", level)];
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
            &[
                ("exit", server_name),
                ("level", level),
                ("google_country", country),
            ],
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
    let (google, search_country) = probe_google_search(&client).await?;
    let (youtube, yt_country) = probe_youtube(&client).await?;
    // Google Search and YouTube geolocate independently, and they can
    // disagree: an IP whose *users* behave like mainland-China users gets the
    // google.com.hk treatment from Search while YouTube's IP database still
    // says something else entirely. The search-side signal is the one that
    // reflects reputation, so it wins.
    Ok(ProbeRound {
        google,
        youtube,
        google_country: search_country.or(yt_country),
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

async fn probe_google_search(
    client: &reqwest::Client,
) -> anyhow::Result<(ProbeStatus, Option<String>)> {
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
            return Ok((ProbeStatus::Ok, None));
        }
    }
    let search_country = search_country_from_host(&page.final_url);
    if is_sorry_page(&page) {
        return Ok((ProbeStatus::Captcha, search_country));
    }
    if page.status.is_success() {
        return Ok((ProbeStatus::Ok, search_country));
    }
    anyhow::bail!(
        "unclassifiable Google search response: status {} at {}",
        page.status,
        page.final_url
    )
}

/// Google retired per-country ccTLD redirects in 2017; the one that remains
/// is mainland-China-located clients → google.com.hk (`pref=hkredirect`,
/// served with hl=zh-CN). Landing there means Google Search locates this IP
/// in mainland China — typically from user-behavior heuristics, and exactly
/// the misclassification worth catching on a VPN exit.
fn search_country_from_host(url: &reqwest::Url) -> Option<String> {
    let host = url.host_str()?;
    if host.ends_with("google.com.hk") || host.ends_with("google.cn") {
        Some("CN".to_string())
    } else {
        None
    }
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
    match yt_playability(&page.body) {
        Some(playability) => match playability {
            YtPlayability::Ok => Ok((ProbeStatus::Ok, country)),
            YtPlayability::BotCheck => Ok((ProbeStatus::SigninWall, country)),
            YtPlayability::Other(status, reason) => {
                // Age gates, removed videos, etc. are properties of the
                // probed video, not of this IP's reputation — surface as a
                // probe error so a bad video choice is visible, not
                // misreported as ok/blocked.
                anyhow::bail!(
                    "probe video has playability {status} ({reason}); reconfigure google_selfcheck_video"
                )
            }
        },
        None => anyhow::bail!(
            "no ytInitialPlayerResponse in YouTube response: status {} at {}",
            page.status,
            page.final_url
        ),
    }
}

enum YtPlayability {
    Ok,
    /// LOGIN_REQUIRED with a "confirm you're not a bot" reason — the
    /// IP-reputation sign-in wall (per yt-dlp/YouTube.js reverse engineering).
    BotCheck,
    Other(String, String),
}

/// Parse `playabilityStatus` out of the `ytInitialPlayerResponse` object
/// embedded in the watch page — the exact playability a real browser sees.
/// Note that very popular (heavily cached) videos can stay playable from
/// flagged IPs, so the probe video should be an unpopular one.
fn yt_playability(body: &str) -> Option<YtPlayability> {
    let json = extract_json_object(body, "ytInitialPlayerResponse")?;
    let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
    let playability = parsed.get("playabilityStatus")?;
    let status = playability.get("status")?.as_str()?;
    let reason = playability
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or_default();
    Some(match status {
        "OK" => YtPlayability::Ok,
        "LOGIN_REQUIRED" if reason.to_lowercase().contains("bot") => YtPlayability::BotCheck,
        _ => YtPlayability::Other(status.to_string(), reason.to_string()),
    })
}

/// Find `<var_name> = {...}` in an HTML page and return the balanced JSON
/// object text. Brace counting ignores braces inside JSON strings.
fn extract_json_object<'a>(body: &'a str, var_name: &str) -> Option<&'a str> {
    let pos = body.find(var_name)?;
    let after = &body[pos + var_name.len()..];
    let eq = after.find('=')?;
    let rest = after[eq + 1..].trim_start();
    if !rest.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Pull Google's perceived country out of YouTube's embedded ytcfg
/// (`"GL":"XX"`, with `"gl":"xx"` in INNERTUBE_CONTEXT as fallback).
/// Caveat: YouTube geolocates by IP database, which can disagree with what
/// Search's reputation heuristics decide (datacenter ranges have shown up as
/// US here while Search treats them as mainland China), so this is only the
/// fallback when the search probe yields no country signal.
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

    fn watch_page(playability: &str) -> String {
        format!(
            r#"<script>var ytInitialPlayerResponse = {{"responseContext":{{"key":"a {{ b }}"}},"playabilityStatus":{playability}}};var other = 1;</script>"#
        )
    }

    #[test]
    fn classifies_playability() {
        assert!(matches!(
            yt_playability(&watch_page(r#"{"status":"OK","playableInEmbed":true}"#)),
            Some(YtPlayability::Ok)
        ));
        assert!(matches!(
            yt_playability(&watch_page(
                r#"{"status":"LOGIN_REQUIRED","reason":"Sign in to confirm you’re not a bot"}"#
            )),
            Some(YtPlayability::BotCheck)
        ));
        // age gate is LOGIN_REQUIRED too, but must NOT count as the bot wall
        assert!(matches!(
            yt_playability(&watch_page(
                r#"{"status":"LOGIN_REQUIRED","reason":"Sign in to confirm your age"}"#
            )),
            Some(YtPlayability::Other(_, _))
        ));
        assert!(matches!(
            yt_playability(&watch_page(
                r#"{"status":"UNPLAYABLE","reason":"Video unavailable"}"#
            )),
            Some(YtPlayability::Other(_, _))
        ));
        assert!(yt_playability("<html>no player response</html>").is_none());
    }

    #[test]
    fn detects_hk_redirect_as_mainland_china() {
        let cn: reqwest::Url =
            "https://www.google.com.hk/search?q=test&hl=en".parse().unwrap();
        assert_eq!(search_country_from_host(&cn), Some("CN".to_string()));
        let cn2: reqwest::Url = "https://www.google.cn/".parse().unwrap();
        assert_eq!(search_country_from_host(&cn2), Some("CN".to_string()));
        let normal: reqwest::Url =
            "https://www.google.com/search?q=test".parse().unwrap();
        assert_eq!(search_country_from_host(&normal), None);
    }

    #[test]
    fn extracts_balanced_json_with_braces_in_strings() {
        let body = r#"x = 1; ytInitialPlayerResponse = {"a":"{ not a real brace }","b":{"c":1}}; y = 2;"#;
        assert_eq!(
            extract_json_object(body, "ytInitialPlayerResponse"),
            Some(r#"{"a":"{ not a real brace }","b":{"c":1}}"#)
        );
    }
}
