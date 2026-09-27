//! Shared HTTP egress for broker transports, including VPN bootstrap.

use std::{future::Future, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::Context;
use reqwest::{ClientBuilder, Url, dns::Resolve};

pub(super) fn client_builder() -> ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
}

pub(super) fn with_dns(builder: ClientBuilder, addrs: Vec<SocketAddr>) -> ClientBuilder {
    builder.dns_resolver(Arc::new(OverrideDnsResolve(addrs)))
}

/// Select the TCP destination without changing the URL, TLS identity, or Host.
/// In VPN mode both DNS and upstream TCP must use the physical interface; the
/// system resolver would otherwise enter the tunnel during bootstrap/reconnect.
pub(super) async fn egress_addrs(
    url: &Url,
    fixed: Option<&[SocketAddr]>,
) -> anyhow::Result<Option<Vec<SocketAddr>>> {
    egress_addrs_with(
        url,
        fixed,
        crate::bound_dialer::binding_active(),
        |host, port| async move { crate::china::resolve_a_physical(&host, port).await },
    )
    .await
}

async fn egress_addrs_with<F, Fut>(
    url: &Url,
    fixed: Option<&[SocketAddr]>,
    binding: bool,
    resolve: F,
) -> anyhow::Result<Option<Vec<SocketAddr>>>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: Future<Output = anyhow::Result<Vec<SocketAddr>>>,
{
    if !binding {
        return Ok(fixed.map(<[SocketAddr]>::to_vec));
    }
    let dests = match fixed {
        Some(addrs) => addrs.to_vec(),
        None => {
            let host = url.host_str().context("broker URL has no host")?;
            resolve(host.to_owned(), url.port_or_known_default().unwrap_or(443))
                .await
                .context("could not resolve broker over the physical NIC")?
        }
    };
    if url.port().is_some() {
        tracing::warn!(%url, "broker URL has an explicit port; the loopback egress forwarder may be bypassed");
    }
    let loopback = super::bind_forward::forward_addrs(dests)
        .await
        .context("could not set up broker egress forwarder")?;
    Ok(Some(vec![loopback]))
}

/// reqwest honors these ports when the URL has no explicit port. This allows
/// dialing a loopback forwarder while keeping the remote hostname for TLS.
pub(crate) struct OverrideDnsResolve(pub(crate) Vec<SocketAddr>);

impl Resolve for OverrideDnsResolve {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let addrs = self.0.clone();
        Box::pin(async move {
            let iter: Box<dyn Iterator<Item = SocketAddr> + Send> = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn ordinary_and_fixed_dns_do_not_call_physical_resolver() {
        let url = Url::parse("https://front.example/").unwrap();
        let fixed = vec!["192.0.2.1:443".parse().unwrap()];
        for addrs in [None, Some(fixed.as_slice())] {
            let result = egress_addrs_with(&url, addrs, false, |_, _| async {
                panic!("physical DNS must not be used without binding")
            })
            .await
            .unwrap();
            assert_eq!(result.as_deref(), addrs);
        }
    }

    #[tokio::test]
    async fn physical_dns_failure_does_not_fall_back() {
        let url = Url::parse("https://front.example/").unwrap();
        let error = egress_addrs_with(&url, None, true, |host, port| async move {
            assert_eq!(host, "front.example");
            assert_eq!(port, 443);
            anyhow::bail!("no physical DNS servers available")
        })
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("no physical DNS servers available"));
        assert!(
            egress_addrs_with(&url, None, true, |_, _| async { Ok(vec![]) })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn forwarded_https_preserves_tls_identity_and_host_override() {
        // Exercise both fixed addresses and addresses returned by physical DNS.
        // The local server proves that reqwest uses the forwarder's ephemeral
        // port while verifying/SNI-addressing the original remote hostname.
        for fixed in [false, true] {
            let rcgen::CertifiedKey { cert, key_pair } =
                rcgen::generate_simple_self_signed(vec!["front.example".into()]).unwrap();
            let tls_config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
                tokio_rustls::rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der())
                    .into(),
            )
            .unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                // An untrusted certificate must fail, even through the forwarder.
                let (stream, _) = listener.accept().await.unwrap();
                assert!(acceptor.accept(stream).await.is_err());
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(stream).await.unwrap();
                assert_eq!(stream.get_ref().1.server_name(), Some("front.example"));
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with("GET /probe HTTP/1.1\r\n"));
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("\r\nhost: broker.example\r\n")
                );
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
            });
            let url = Url::parse("https://front.example/probe").unwrap();
            let fixed_addrs = [upstream];
            let addrs = egress_addrs_with(
                &url,
                fixed.then_some(fixed_addrs.as_slice()),
                true,
                |host, port| async move {
                    assert!(!fixed, "fixed addresses must bypass physical DNS");
                    assert_eq!(host, "front.example");
                    assert_eq!(port, 443);
                    Ok(vec![upstream])
                },
            )
            .await
            .unwrap()
            .unwrap();
            assert!(addrs[0].ip().is_loopback());
            assert_ne!(addrs[0], upstream);
            let untrusted = with_dns(client_builder(), addrs.clone()).build().unwrap();
            assert!(untrusted.get(url.clone()).send().await.is_err());
            let trusted = with_dns(client_builder(), addrs)
                .add_root_certificate(reqwest::Certificate::from_der(cert.der()).unwrap())
                .build()
                .unwrap();
            let response = trusted
                .get(url)
                .header("Host", "broker.example")
                .send()
                .await
                .unwrap();
            assert_eq!(response.text().await.unwrap(), "ok");
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
        }
    }
}
