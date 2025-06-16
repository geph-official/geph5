use geph5_misc_rpc::bridge::ObfsProtocol;
use anyhow::bail;

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

use sillad::{listener::{DynListener, Listener, ListenerExt}, dialer::{DynDialer, DialerExt}, tcp::TcpDialer};
use sillad_conntest::{ConnTestListener, ConnTestDialer};
use sillad_sosistab3::{listener::SosistabListener, dialer::SosistabDialer, Cookie};
use sillad_native_tls::{TlsListener, TlsDialer};
use sillad_hex::{HexDialer, HexListener};
use async_native_tls::{TlsConnector, TlsAcceptor};
use time::OffsetDateTime;
use time::Duration as TimeDuration;

/// Recursively wrap a listener according to the protocol stack.
pub fn listener_from_stack<L: Listener>(proto: ObfsProtocol, bottom: L, tls_acceptor: &TlsAcceptor) -> DynListener
where
    L::P: 'static,
{
    match proto {
        ObfsProtocol::None => bottom.dynamic(),
        ObfsProtocol::Sosistab3(cookie) => {
            SosistabListener::new(bottom, Cookie::new(&cookie)).dynamic()
        }
        ObfsProtocol::ConnTest(inner) => {
            let inner = listener_from_stack(*inner, bottom, tls_acceptor);
            ConnTestListener::new(inner).dynamic()
        }
        ObfsProtocol::PlainTls(inner) => {
            let inner = listener_from_stack(*inner, bottom, tls_acceptor);
            TlsListener::new(inner, tls_acceptor.clone()).dynamic()
        }
        ObfsProtocol::Hex(inner) => {
            let inner = listener_from_stack(*inner, bottom, tls_acceptor);
            HexListener { inner }.dynamic()
        }
        ObfsProtocol::Sosistab3New(cookie, inner) => {
            let inner = listener_from_stack(*inner, bottom, tls_acceptor);
            SosistabListener::new(inner, Cookie::new(&cookie)).dynamic()
        }
    }
}

/// Recursively wrap a dialer according to the protocol stack.
pub fn dialer_from_stack(proto: &ObfsProtocol, addr: std::net::SocketAddr) -> DynDialer {
    fn inner(proto: &ObfsProtocol, lower: DynDialer) -> DynDialer {
        match proto {
            ObfsProtocol::None => lower,
            ObfsProtocol::Sosistab3(cookie) => {
                SosistabDialer { inner: lower, cookie: Cookie::new(cookie) }.dynamic()
            }
            ObfsProtocol::ConnTest(sub) => {
                let lower = inner(&*sub, lower);
                ConnTestDialer { inner: lower, ping_count: 1 }.dynamic()
            }
            ObfsProtocol::PlainTls(sub) => {
                let lower = inner(&*sub, lower);
                let connector = TlsConnector::new()
                    .use_sni(false)
                    .danger_accept_invalid_certs(true)
                    .danger_accept_invalid_hostnames(true)
                    .min_protocol_version(None)
                    .max_protocol_version(None);
                TlsDialer::new(lower, connector, "example.com".into()).dynamic()
            }
            ObfsProtocol::Hex(sub) => {
                let lower = inner(&*sub, lower);
                HexDialer { inner: lower }.dynamic()
            }
            ObfsProtocol::Sosistab3New(cookie, sub) => {
                let lower = inner(&*sub, lower);
                SosistabDialer { inner: lower, cookie: Cookie::new(cookie) }.dynamic()
            }
        }
    }

    let bottom = TcpDialer { dest_addr: addr }.dynamic();
    inner(proto, bottom)
}

/// Generate a TLS acceptor using a random self-signed certificate.
pub fn dummy_tls_acceptor() -> TlsAcceptor {
    use rcgen::KeyPair;
    let subject_alt_names = (0..10)
        .map(|_| format!("{}.com", rand::random::<u16>()))
        .collect::<Vec<_>>();
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name.push(rcgen::DnType::CommonName, "api.example.com");
    params.subject_alt_names = subject_alt_names
        .iter()
        .map(|san| rcgen::SanType::DnsName(san.clone().try_into().unwrap()))
        .collect();
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + TimeDuration::days(365);
    params.serial_number = Some(rand::random::<u64>().into());
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let keypair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&keypair).unwrap();
    let cert_pem = cert.pem();
    let cert_key = keypair.serialize_pem();
    let identity = native_tls::Identity::from_pkcs8(cert_pem.as_bytes(), cert_key.as_bytes())
        .expect("Cannot decode identity");
    let mut builder = native_tls::TlsAcceptor::builder(identity);
    builder.min_protocol_version(Some(native_tls::Protocol::Tlsv10));
    builder.max_protocol_version(Some(native_tls::Protocol::Tlsv12));
    builder.build().unwrap().into()
}
