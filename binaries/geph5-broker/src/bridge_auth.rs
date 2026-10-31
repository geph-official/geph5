//! Bridge credential overlap during rotation. No token values enter logs or metrics.
use geph5_broker_protocol::{Mac, MacError};
use geph5_stats::{StatEvent, StatsdUdpSink};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ConfigFile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Primary,
    Legacy,
    Exit,
}

impl TokenKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Legacy => "legacy",
            Self::Exit => "exit",
        }
    }
}

pub(crate) fn verify_bridge<T: Serialize>(
    mac: Mac<T>,
    config: &ConfigFile,
) -> Result<(T, TokenKind), MacError> {
    verify(mac, config, false)
}

pub(crate) fn verify_stats<T: Serialize>(
    mac: Mac<T>,
    config: &ConfigFile,
) -> Result<(T, TokenKind), MacError> {
    verify(mac, config, true)
}

fn verify<T: Serialize>(
    mac: Mac<T>,
    config: &ConfigFile,
    allow_exit: bool,
) -> Result<(T, TokenKind), MacError> {
    // Borrow the payload so trying another key does not clone a stats batch.
    let matches = |token: &str| {
        Mac {
            inner: &mac.inner,
            khash: mac.khash,
        }
        .verify(blake3::hash(token.as_bytes()).as_bytes())
        .is_ok()
    };
    let kind = if matches(&config.bridge_token) {
        TokenKind::Primary
    } else if config
        .legacy_bridge_tokens
        .iter()
        .any(|token| matches(token))
    {
        TokenKind::Legacy
    } else if allow_exit && matches(&config.exit_token) {
        TokenKind::Exit
    } else {
        return Err(MacError::InvalidMac);
    };
    Ok((mac.inner, kind))
}

pub(crate) fn record_accepted(method: &str, token: TokenKind, sink: Option<&StatsdUdpSink>) {
    static PRIMARY: AtomicU64 = AtomicU64::new(0);
    static LEGACY: AtomicU64 = AtomicU64::new(0);
    static EXIT: AtomicU64 = AtomicU64::new(0);
    let counter = match token {
        TokenKind::Primary => &PRIMARY,
        TokenKind::Legacy => &LEGACY,
        TokenKind::Exit => &EXIT,
    };
    let accepted_total = counter.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::info!(
        method,
        token = token.label(),
        accepted_total,
        "node MAC accepted"
    );
    if let Some(sink) = sink {
        sink.send_one(&StatEvent::counter(
            "broker_node_auth_accepted",
            &[("method", method), ("token", token.label())],
            1.0,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ConfigFile {
        serde_yaml::from_str(
            "listen: 127.0.0.1:8080\ntcp_listen: 127.0.0.1:8081\nmaster_secret: master\nmizaru_keys: keys\npostgres_url: unused\nbridge_token: new\nexit_token: exit\nopenai_key: unused\n",
        ).unwrap()
    }

    fn signed(token: &str) -> Mac<Vec<u64>> {
        Mac::new(vec![1, 2, 3], blake3::hash(token.as_bytes()).as_bytes())
    }

    #[test]
    fn existing_config_accepts_primary_and_defaults_to_no_legacy() {
        let config = config();
        assert!(config.legacy_bridge_tokens.is_empty());
        assert_eq!(
            verify_bridge(signed("new"), &config).unwrap(),
            (vec![1, 2, 3], TokenKind::Primary)
        );
        assert!(verify_bridge(signed("old"), &config).is_err());
        assert!(verify_stats(signed("old"), &config).is_err());
    }

    #[test]
    fn overlap_and_retirement_apply_to_registration_and_statistics() {
        let mut config = config();
        config.legacy_bridge_tokens = vec!["old".into(), "older".into()];
        for token in ["old", "older"] {
            assert_eq!(
                verify_bridge(signed(token), &config).unwrap().1,
                TokenKind::Legacy
            );
            assert_eq!(
                verify_stats(signed(token), &config).unwrap().1,
                TokenKind::Legacy
            );
        }
        assert_eq!(
            verify_stats(signed("new"), &config).unwrap().1,
            TokenKind::Primary
        );
        config.legacy_bridge_tokens.clear();
        assert!(verify_bridge(signed("old"), &config).is_err());
        assert!(verify_stats(signed("old"), &config).is_err());
        assert_eq!(
            verify_stats(signed("new"), &config).unwrap().1,
            TokenKind::Primary
        );
    }

    #[test]
    fn exit_token_only_authenticates_statistics() {
        let config = config();
        assert_eq!(
            verify_stats(signed("exit"), &config).unwrap().1,
            TokenKind::Exit
        );
        assert!(verify_bridge(signed("exit"), &config).is_err());
        assert!(verify_bridge(signed("unknown"), &config).is_err());
        assert!(verify_stats(signed("unknown"), &config).is_err());
    }

    #[test]
    fn modified_payload_is_rejected_by_all_keys() {
        let mut config = config();
        config.legacy_bridge_tokens.push("old".into());
        for token in ["new", "old", "exit"] {
            let mut mac = signed(token);
            mac.inner.push(4);
            assert!(verify_bridge(mac.clone(), &config).is_err());
            assert!(verify_stats(mac, &config).is_err());
        }
    }

    #[test]
    fn primary_has_priority_if_also_listed_as_legacy() {
        let mut config = config();
        config.legacy_bridge_tokens.push("new".into());
        assert_eq!(
            verify_bridge(signed("new"), &config).unwrap().1,
            TokenKind::Primary
        );
    }
}
