use anyhow::Context;
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, KeyPair};
use tokio_native_tls::TlsAcceptor;

/// Generate the self-signed transport certificate for the broker's chosen SNI.
pub fn tls_config(sni_domain: &str) -> anyhow::Result<TlsAcceptor> {
    let (cert, keypair) = certificate(sni_domain)?;
    acceptor(&cert, &keypair)
}

fn certificate(sni_domain: &str) -> anyhow::Result<(Certificate, KeyPair)> {
    anyhow::ensure!(!sni_domain.is_empty(), "TLS server name is empty");
    let mut params = CertificateParams::new(vec![sni_domain.to_owned()])?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, sni_domain);
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(365);
    params.serial_number = Some(rand::random::<u64>().into());
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let keypair = KeyPair::generate()?;
    let cert = params.self_signed(&keypair)?;
    Ok((cert, keypair))
}

fn acceptor(cert: &Certificate, keypair: &KeyPair) -> anyhow::Result<TlsAcceptor> {
    let identity =
        native_tls::Identity::from_pkcs8(cert.pem().as_bytes(), keypair.serialize_pem().as_bytes())
            .context("cannot load TLS identity")?;
    let mut builder = native_tls::TlsAcceptor::builder(identity);
    builder.min_protocol_version(Some(native_tls::Protocol::Tlsv10));
    Ok(builder.build()?.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use geph5_misc_rpc::bridge::DEFAULT_TLS_SNI;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn connect_with_name(cert_name: &str, client_name: &str) -> bool {
        let (cert, keypair) = certificate(cert_name).unwrap();
        let acceptor = acceptor(&cert, &keypair).unwrap();
        // Trust only the generated certificate while retaining hostname checking.
        let mut builder = native_tls::TlsConnector::builder();
        builder.disable_built_in_roots(true);
        builder.add_root_certificate(native_tls::Certificate::from_der(cert.der()).unwrap());
        builder.max_protocol_version(Some(native_tls::Protocol::Tlsv12));
        let connector = tokio_native_tls::TlsConnector::from(builder.build().unwrap());
        let (client, server) = tokio::io::duplex(65536);
        let server = geph5_rt::spawn(async move {
            if let Ok(mut tls) = acceptor.accept(server).await {
                let _ = tls.write_all(b"ok").await;
            }
        });
        let result = if let Ok(mut tls) = connector.connect(client_name, client).await {
            let mut body = [0; 2];
            tls.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"ok");
            true
        } else {
            false
        };
        server.await;
        result
    }

    #[test]
    fn tls_certificate_matches_the_requested_name() {
        geph5_rt::block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                assert!(connect_with_name(DEFAULT_TLS_SNI, DEFAULT_TLS_SNI).await);
                assert!(connect_with_name("custom.example.org", "custom.example.org").await);
                assert!(!connect_with_name("custom.example.org", "different.example.org").await);
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn invalid_certificate_names_return_errors() {
        assert!(tls_config("").is_err());
        assert!(tls_config("bad\u{2603}.example").is_err());
    }
}
