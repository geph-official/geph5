use anyhow::bail;
use geph5_misc_rpc::bridge::{DEFAULT_TLS_SNI, ObfsProtocol};

/// Parse a comma separated protocol stack.
/// Items are applied from bottom to top. For example:
/// "tls,sosistab3=COOKIE,conntest" means TCP -> TLS -> Sosistab3 -> ConnTest.
pub fn parse_stack(spec: &str) -> anyhow::Result<ObfsProtocol> {
    let mut proto = ObfsProtocol::None;
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(rest) = part.strip_prefix("sosistab3=") {
            proto = ObfsProtocol::Sosistab3New(rest.to_string(), Box::new(proto));
        } else if let Some(rest) = part.strip_prefix("meeklike=") {
            proto = ObfsProtocol::Meeklike(
                rest.to_string(),
                MeeklikeConfig::default(),
                Box::new(proto),
            );
        } else if let Some(sni_domain) = part.strip_prefix("tls=") {
            proto = ObfsProtocol::PlainTlsWithSni {
                sni_domain: sni_domain.into(),
                lower: Box::new(proto),
            };
        } else if part.eq_ignore_ascii_case("tls") {
            proto = ObfsProtocol::PlainTls(Box::new(proto));
        } else if part.eq_ignore_ascii_case("conntest") {
            proto = ObfsProtocol::ConnTest(Box::new(proto));
        } else if part.eq_ignore_ascii_case("hex") {
            proto = ObfsProtocol::Hex(Box::new(proto));
        } else if part.eq_ignore_ascii_case("none") {
            // explicitly no-op
        } else {
            bail!("unknown protocol segment: {part}");
        }
    }
    Ok(proto)
}

use sillad::{
    dialer::{DialerExt, DynDialer},
    listener::{DynListener, Listener, ListenerExt},
    tcp::TcpDialer,
};
use sillad_conntest::{ConnTestDialer, ConnTestListener};
use sillad_hex::{HexDialer, HexListener};
use sillad_meeklike::{MeeklikeConfig, MeeklikeDialer, MeeklikeListener};
use sillad_native_tls::{TlsDialer, TlsListener};
use sillad_sosistab3::{Cookie, dialer::SosistabDialer, listener::SosistabListener};
use time::Duration as TimeDuration;
use time::OffsetDateTime;
use tokio_native_tls::{TlsAcceptor, TlsConnector};

/// Recursively wrap a listener according to the protocol stack.
pub fn listener_from_stack<L: Listener>(
    proto: ObfsProtocol,
    bottom: L,
    tls_acceptor: &TlsAcceptor,
) -> anyhow::Result<DynListener>
where
    L::P: 'static,
{
    Ok(match proto {
        ObfsProtocol::None => bottom.dynamic(),
        ObfsProtocol::Sosistab3(cookie) => {
            SosistabListener::new(bottom, Cookie::new(&cookie)).dynamic()
        }
        ObfsProtocol::ConnTest(inner) => {
            let inner = listener_from_stack(*inner, bottom, tls_acceptor)?;
            ConnTestListener::new(inner).dynamic()
        }
        ObfsProtocol::PlainTls(inner) => {
            let inner = listener_from_stack(*inner, bottom, tls_acceptor)?;
            TlsListener::new(inner, tls_acceptor.clone()).dynamic()
        }
        ObfsProtocol::PlainTlsWithSni { sni_domain, lower } => {
            let inner = listener_from_stack(*lower, bottom, tls_acceptor)?;
            TlsListener::new(inner, tls_acceptor_for_sni(&sni_domain)?).dynamic()
        }
        ObfsProtocol::Hex(inner) => {
            let inner = listener_from_stack(*inner, bottom, tls_acceptor)?;
            HexListener { inner }.dynamic()
        }
        ObfsProtocol::Sosistab3New(cookie, inner) => {
            let inner = listener_from_stack(*inner, bottom, tls_acceptor)?;
            SosistabListener::new(inner, Cookie::new(&cookie)).dynamic()
        }
        ObfsProtocol::Meeklike(key, cfg, inner) => {
            let inner = listener_from_stack(*inner, bottom, tls_acceptor)?;
            MeeklikeListener::new(inner, *blake3::hash(key.as_bytes()).as_bytes(), cfg).dynamic()
        }
    })
}

/// Recursively wrap a dialer according to the protocol stack.
pub fn dialer_from_stack(proto: &ObfsProtocol, addr: std::net::SocketAddr) -> DynDialer {
    fn inner(proto: &ObfsProtocol, lower: DynDialer) -> DynDialer {
        match proto {
            ObfsProtocol::None => lower,
            ObfsProtocol::Sosistab3(cookie) => SosistabDialer {
                inner: lower,
                cookie: Cookie::new(cookie),
            }
            .dynamic(),
            ObfsProtocol::ConnTest(sub) => {
                let lower = inner(sub, lower);
                ConnTestDialer {
                    inner: lower,
                    ping_count: 1,
                }
                .dynamic()
            }
            ObfsProtocol::PlainTls(sub) => {
                let lower = inner(sub, lower);
                tls_dialer(lower, DEFAULT_TLS_SNI)
            }
            ObfsProtocol::PlainTlsWithSni {
                sni_domain,
                lower: sub,
            } => {
                let lower = inner(sub, lower);
                tls_dialer(lower, sni_domain)
            }
            ObfsProtocol::Hex(sub) => {
                let lower = inner(sub, lower);
                HexDialer { inner: lower }.dynamic()
            }
            ObfsProtocol::Sosistab3New(cookie, sub) => {
                let lower = inner(sub, lower);
                SosistabDialer {
                    inner: lower,
                    cookie: Cookie::new(cookie),
                }
                .dynamic()
            }
            ObfsProtocol::Meeklike(key, cfg, sub) => {
                let lower = inner(sub, lower);
                MeeklikeDialer {
                    inner: lower.into(),
                    key: *blake3::hash(key.as_bytes()).as_bytes(),
                    cfg: *cfg,
                }
                .dynamic()
            }
        }
    }

    let bottom = TcpDialer { dest_addr: addr }.dynamic();
    inner(proto, bottom)
}

fn tls_dialer(lower: DynDialer, sni_domain: &str) -> DynDialer {
    let mut builder = native_tls::TlsConnector::builder();
    builder
        .use_sni(true)
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .min_protocol_version(None)
        .max_protocol_version(Some(native_tls::Protocol::Tlsv12));
    let connector = TlsConnector::from(builder.build().unwrap());
    TlsDialer::new(lower, connector, sni_domain.into()).dynamic()
}

/// Generate a TLS acceptor for the legacy stack's default SNI.
pub fn dummy_tls_acceptor() -> anyhow::Result<TlsAcceptor> {
    tls_acceptor_for_sni(DEFAULT_TLS_SNI)
}

fn tls_acceptor_for_sni(sni_domain: &str) -> anyhow::Result<TlsAcceptor> {
    use rcgen::KeyPair;
    anyhow::ensure!(!sni_domain.is_empty(), "TLS server name is empty");
    let mut params = rcgen::CertificateParams::new(vec![sni_domain.to_owned()])?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, sni_domain);
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + TimeDuration::days(365);
    params.serial_number = Some(rand::random::<u64>().into());
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let keypair = KeyPair::generate()?;
    let cert = params.self_signed(&keypair)?;
    let cert_pem = cert.pem();
    let cert_key = keypair.serialize_pem();
    let identity = native_tls::Identity::from_pkcs8(cert_pem.as_bytes(), cert_key.as_bytes())?;
    let mut builder = native_tls::TlsAcceptor::builder(identity);
    builder.min_protocol_version(Some(native_tls::Protocol::Tlsv10));
    builder.max_protocol_version(Some(native_tls::Protocol::Tlsv12));
    Ok(builder.build()?.into())
}
