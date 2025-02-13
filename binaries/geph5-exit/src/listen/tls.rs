use async_native_tls::TlsAcceptor;
use rcgen::KeyPair;

pub fn dummy_tls_config() -> TlsAcceptor {
    // let subject_alt_names = (0..10)
    //     .map(|_| format!("{}.com", rand::random::<u16>()))
    //     .collect::<Vec<_>>();

    // let cert = generate_simple_self_signed(subject_alt_names).unwrap();
    // let cert_pem = cert.cert.pem();
    // let cert_key = cert.key_pair.serialize_pem();
    // let identity = Identity::from_pkcs8(cert_pem.as_bytes(), cert_key.as_bytes())
    //     .expect("Cannot decode identity");

    let subject_alt_names = (0..10)
        .map(|_| format!("{}.com", rand::random::<u16>()))
        .collect::<Vec<_>>();

    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "api.example.com");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "Example Inc.");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationalUnitName, "API Services");
    params
        .distinguished_name
        .push(rcgen::DnType::LocalityName, "San Francisco");
    params
        .distinguished_name
        .push(rcgen::DnType::StateOrProvinceName, "California");
    params
        .distinguished_name
        .push(rcgen::DnType::CountryName, "US");

    // Customize the issuer (for self-signed certificates, issuer is the same as subject)
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Example Inc. Root CA");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "Example Inc.");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationalUnitName, "Security");
    params
        .distinguished_name
        .push(rcgen::DnType::LocalityName, "San Francisco");
    params
        .distinguished_name
        .push(rcgen::DnType::StateOrProvinceName, "California");
    params
        .distinguished_name
        .push(rcgen::DnType::CountryName, "US");
    params.subject_alt_names = subject_alt_names
        .iter()
        .map(|san| rcgen::SanType::DnsName(san.clone().try_into().unwrap()))
        .collect();

    // Customize other fields as needed
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(365);
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
