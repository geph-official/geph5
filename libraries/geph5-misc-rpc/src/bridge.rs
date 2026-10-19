use std::{net::SocketAddr, time::SystemTime};

use async_trait::async_trait;
use nanorpc::nanorpc_derive;
use serde::{Deserialize, Serialize};
use sillad_meeklike::MeeklikeConfig;

/// Hostname used by legacy TLS metadata, which does not carry its own SNI.
pub const DEFAULT_TLS_SNI: &str = "lusheeta-toel.yandex.ru";

/// The metadata object passed to the exit on every b2e link.
#[derive(Serialize, Deserialize, Clone, Debug, Eq, Hash, PartialEq)]
pub struct B2eMetadata {
    pub protocol: ObfsProtocol,
    pub expiry: SystemTime,
}

/// Initialization information for an obfuscation session.
#[derive(Serialize, Deserialize, Clone, Debug, Eq, Hash, PartialEq)]
pub enum ObfsProtocol {
    Sosistab3(String),
    None,
    ConnTest(Box<Self>),
    PlainTls(Box<Self>),
    Sosistab3New(String, Box<Self>),
    Hex(Box<Self>),
    Meeklike(String, MeeklikeConfig, Box<Self>),
    // Append new variants: bridge-to-exit stdcode encodes the variant index.
    /// TLS with a certificate for the hostname advertised to clients as SNI.
    PlainTlsWithSni {
        sni_domain: String,
        lower: Box<Self>,
    },
}

/// The RPC protocol that bridges expose, called by the broker.
#[nanorpc_derive]
#[async_trait]
pub trait BridgeControlProtocol {
    async fn tcp_forward(&self, b2e_dest: SocketAddr, metadata: B2eMetadata) -> SocketAddr;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_tls_wire_format_is_preserved() {
        let protocol = ObfsProtocol::PlainTls(Box::new(ObfsProtocol::None));
        // Existing bridge/exit binaries encode PlainTls(None) as variants 3, 1.
        assert_eq!(stdcode::serialize(&protocol).unwrap(), [3, 1]);
        assert_eq!(
            stdcode::deserialize::<ObfsProtocol>(&[3, 1]).unwrap(),
            protocol
        );
        assert_eq!(
            serde_json::to_string(&protocol).unwrap(),
            r#"{"PlainTls":"None"}"#
        );
    }

    #[test]
    fn tls_name_survives_broker_and_bridge_serialization() {
        let metadata = B2eMetadata {
            protocol: ObfsProtocol::ConnTest(Box::new(ObfsProtocol::Sosistab3New(
                "test-cookie".into(),
                Box::new(ObfsProtocol::PlainTlsWithSni {
                    sni_domain: "bridge.example.org".into(),
                    lower: Box::new(ObfsProtocol::None),
                }),
            ))),
            expiry: SystemTime::UNIX_EPOCH,
        };
        let bridge: B2eMetadata =
            serde_json::from_slice(&serde_json::to_vec(&metadata).unwrap()).unwrap();
        let exit: B2eMetadata =
            stdcode::deserialize(&stdcode::serialize(&bridge).unwrap()).unwrap();
        assert_eq!(metadata, exit);
    }
}
