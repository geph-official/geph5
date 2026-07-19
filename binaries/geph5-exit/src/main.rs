mod bw_accounting;
mod dns;
mod ipv6;
mod session;
mod tasklimit;

use clap::Parser;
use ed25519_dalek::SigningKey;
use ipnet::Ipv6Net;

use isocountry::CountryCode;
use listen::listen_main;
use once_cell::sync::{Lazy, OnceCell};
use rand::Rng;
use serde::Deserialize;
use serde_with::{DisplayFromStr, serde_as};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process,
    str::FromStr,
    thread,
    time::Duration,
};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

mod allow;
mod auth;
mod broker;
mod listen;
mod proxy;
mod ratelimit;
mod schedlag;
mod selfcheck;

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use crate::ratelimit::update_load_loop;
use geph5_broker_protocol::{AccountLevel, ExitCategory, ExitMetadata};
use session::RATE_LIMITER_CACHE;

fn read_status_kb(field: &str) -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            return rest
                .split_whitespace()
                .next()
                .and_then(|val| val.parse::<u64>().ok());
        }
    }
    None
}

fn instrumentation_report_loop() {
    tracing::info!(
        pid = process::id(),
        picomux_stream_size = std::mem::size_of::<picomux::Stream>(),
        sosistab_over_stream_size =
            std::mem::size_of::<sillad_sosistab3::SosistabPipe<picomux::Stream>>(),
        "instrumentation type sizes"
    );
    loop {
        thread::sleep(Duration::from_secs(60));
        let picomux_stats = picomux::global_buffer_table_stats();
        let sosistab_stats = sillad_sosistab3::listener::global_listener_stats();
        tracing::info!(
            vmrss_kb = read_status_kb("VmRSS:"),
            vmswap_kb = read_status_kb("VmSwap:"),
            rate_limiter_entries = RATE_LIMITER_CACHE.entry_count(),
            picomux_tables = picomux_stats.live_tables,
            picomux_active_streams = picomux_stats.active_streams,
            picomux_active_stream_capacity = picomux_stats.active_stream_capacity,
            picomux_tombstones = picomux_stats.tombstones,
            picomux_tombstone_capacity = picomux_stats.tombstone_capacity,
            sosistab_listeners = sosistab_stats.live_listeners,
            sosistab_queue_slots = sosistab_stats.queue_slots,
            sosistab_queue_payload_bytes = sosistab_stats.queue_payload_bytes,
            "instrumentation snapshot"
        );
    }
}

/// The global config file.
static CONFIG_FILE: OnceCell<ConfigFile> = OnceCell::new();

/// This struct defines the structure of our configuration file
#[serde_as]
#[derive(Deserialize)]
struct ConfigFile {
    signing_secret: PathBuf,
    broker: Option<BrokerConfig>,

    c2e_listen: SocketAddr,
    b2e_listen: SocketAddr,
    ip_addr: Option<AdvertisedIpSource>,

    country: CountryCode,
    city: String,

    #[serde(default)]
    metadata: Option<ExitMetadata>,

    #[serde(default = "default_country_blacklist")]
    country_blacklist: Vec<String>,

    #[serde(default = "default_free_ratelimit")]
    free_ratelimit: u32,

    #[serde(default = "default_plus_ratelimit")]
    plus_ratelimit: u32,

    #[serde(default = "default_total_ratelimit")]
    total_ratelimit: u32,

    #[serde(default = "default_free_port_whitelist")]
    free_port_whitelist: Vec<u16>,

    #[serde(default = "default_task_limit")]
    task_limit: usize,

    #[serde_as(as = "DisplayFromStr")]
    #[serde(default)]
    ipv6_subnet: Ipv6Net,

    #[serde(default = "default_ipv6_pool_size")]
    ipv6_pool_size: usize,

    /// YouTube video ID probed by the Google-reachability selfcheck.
    #[serde(default = "default_selfcheck_video")]
    selfcheck_video: String,
}

fn default_selfcheck_video() -> String {
    // A well-known, stable video.
    "dQw4w9WgXcQ".into()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AdvertisedIpSource {
    Static(IpAddr),
    Hostname(String),
}

impl<'de> Deserialize<'de> for AdvertisedIpSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(serde::de::Error::custom("ip_addr must not be empty"));
        }
        Ok(IpAddr::from_str(trimmed)
            .map(AdvertisedIpSource::Static)
            .unwrap_or_else(|_| AdvertisedIpSource::Hostname(trimmed.to_string())))
    }
}

fn default_free_ratelimit() -> u32 {
    150
}

fn default_plus_ratelimit() -> u32 {
    50000
}

fn default_total_ratelimit() -> u32 {
    125000
}

fn default_task_limit() -> usize {
    1_000_000
}

fn default_free_port_whitelist() -> Vec<u16> {
    vec![
        20, 21, 43, 53, 79, 80, 81, 88, 110, 143, 220, 389, 443, 464, 531, 543, 544, 554, 636, 706,
        749, 873, 902, 903, 904, 981, 989, 990, 991, 992, 993, 995, 1194, 1220, 1293, 1500, 1533,
        1677, 1723, 1755, 1863, 2083, 2086, 2087, 2095, 2096, 2102, 2103, 2104, 3690, 4321, 4643,
        5050, 5190, 5222, 5223, 5228, 8008, 8074, 8082, 8087, 8088, 8332, 8333, 8443, 8888, 9418,
        10000, 11371, 19294, 19638, 50002, 64738,
        // --- VoIP / WebRTC calling (WhatsApp, Signal, FaceTime, Teams, Zoom, Meet) ---
        // STUN/TURN signaling + relay. This is the single most important group: when
        // direct peer-to-peer media is blocked, virtually every app falls back to
        // relaying media through a TURN server on these ports, so opening them makes
        // calls work without exposing the wide ephemeral media ranges. Not amplifiers,
        // and UDP here is a *connected* socket on the exit (see proxy.rs), so it cannot
        // be abused as a spoofed-source reflector.
        3478, 3479, 3480, 3481, // STUN/TURN/ICE (WhatsApp, Teams, Zoom, Meet, FaceTime)
        5349, // STUN/TURN over TLS/DTLS
        // WhatsApp-specific signaling/media ports documented by Meta.
        4244, 5242, 45395, 50318,
        // Direct-media ports for the major apps (bounded ranges, not the full
        // 49152-65535 ephemeral range, which would defeat the whitelist).
        19302, 19303, 19304, 19305, 19306, 19307, 19308, 19309, // Google Meet/Duo
        8801, 8802, 8803, 8804, 8805, 8806, 8807, 8808, 8809, 8810, // Zoom media
        16384, 16385, 16386, 16387, // Apple FaceTime RTP
        16393, 16394, 16395, 16396, 16397, 16398, 16399, 16400, 16401, 16402, // FaceTime RTP
    ]
}

fn default_country_blacklist() -> Vec<String> {
    vec![]
}

fn default_ipv6_pool_size() -> usize {
    1000
}

#[cfg(target_os = "linux")]
fn disable_process_thp() -> std::io::Result<()> {
    let rc = unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn disable_process_thp() -> std::io::Result<()> {
    Ok(())
}

fn default_exit_metadata() -> ExitMetadata {
    let cfg = CONFIG_FILE.wait();
    let mut allowed_levels = vec![AccountLevel::Plus];
    if cfg.free_ratelimit > 0
        && matches!(
            cfg.country,
            CountryCode::CAN
                | CountryCode::NLD
                | CountryCode::FRA
                | CountryCode::POL
                | CountryCode::DEU
        )
    {
        allowed_levels.push(AccountLevel::Free);
    }
    ExitMetadata {
        allowed_levels,
        category: ExitCategory::Core,
    }
}

#[derive(Deserialize)]
struct BrokerConfig {
    url: String,
    auth_token: String,
}

static SIGNING_SECRET: Lazy<SigningKey> = Lazy::new(|| {
    let config_file = CONFIG_FILE.get().expect("Config file must be initialized.");
    let path = &config_file.signing_secret;
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() == 32 => {
            let bytes: [u8; 32] = bytes.as_slice().try_into().unwrap();
            SigningKey::from_bytes(&bytes)
        }
        _ => {
            // Generate a new SigningKey if there's an error or the length is not 32 bytes.
            let new_key = SigningKey::from_bytes(&rand::thread_rng().r#gen());
            let key_bytes = new_key.to_bytes();
            std::fs::write(&config_file.signing_secret, key_bytes).unwrap();
            new_key
        }
    }
});

fn exit_metadata() -> ExitMetadata {
    let cfg = CONFIG_FILE.wait();
    let mut metadata = cfg.metadata.clone().unwrap_or_else(default_exit_metadata);
    if cfg.free_ratelimit == 0 {
        metadata
            .allowed_levels
            .retain(|level| *level != AccountLevel::Free);
    }
    metadata
}

/// Run the Geph5 broker.
#[derive(Parser)]
struct CliArgs {
    /// path to a YAML-based config file
    #[arg(short, long)]
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    if let Err(err) = disable_process_thp() {
        eprintln!("warning: failed to disable THP for geph5-exit: {err}");
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive("geph5_exit=debug".parse()?)
                .from_env_lossy(),
        )
        .init();
    tracing::info!("process requested THP disable");

    std::thread::spawn(update_load_loop);
    tracing::info!("**** START GEPH EXIT ****");
    let args = CliArgs::parse();
    let config: ConfigFile = serde_yaml::from_slice(&std::fs::read(args.config)?)?;

    CONFIG_FILE.set(config).ok().unwrap();

    std::thread::spawn(instrumentation_report_loop);
    geph5_rt::block_on(listen_main())
}

#[cfg(test)]
mod tests {
    use super::{AdvertisedIpSource, ConfigFile};

    fn parse_ip_addr(value: &str) -> Option<AdvertisedIpSource> {
        let yaml = format!(
            r#"
signing_secret: /tmp/geph5-exit-test-secret
c2e_listen: 0.0.0.0:9002
b2e_listen: 0.0.0.0:9003
ip_addr: {value}
country: US
city: NYC
"#
        );
        serde_yaml::from_str::<ConfigFile>(&yaml).unwrap().ip_addr
    }

    #[test]
    fn static_ipv4_ip_addr_still_parses() {
        assert_eq!(
            parse_ip_addr("203.0.113.5"),
            Some(AdvertisedIpSource::Static("203.0.113.5".parse().unwrap()))
        );
    }

    #[test]
    fn static_ipv6_ip_addr_still_parses() {
        assert_eq!(
            parse_ip_addr("\"2001:db8::5\""),
            Some(AdvertisedIpSource::Static("2001:db8::5".parse().unwrap()))
        );
    }

    #[test]
    fn hostname_ip_addr_parses_as_dynamic_source() {
        assert_eq!(
            parse_ip_addr("exit.example.org"),
            Some(AdvertisedIpSource::Hostname("exit.example.org".into()))
        );
    }
}
