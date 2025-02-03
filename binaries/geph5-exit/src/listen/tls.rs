use async_native_tls::{Identity, TlsAcceptor};
use rcgen::generate_simple_self_signed;

pub fn dummy_tls_config() -> TlsAcceptor {
    let subject_alt_names = (0..10)
        .map(|_| format!("{}.com", rand::random::<u16>()))
        .collect::<Vec<_>>();

    let cert = generate_simple_self_signed(subject_alt_names).unwrap();
    let cert_pem = cert.cert.pem();
    let cert_key = cert.key_pair.serialize_pem();
    let identity = Identity::from_pkcs8(cert_pem.as_bytes(), cert_key.as_bytes())
        .expect("Cannot decode identity");

    let mut builder = native_tls::TlsAcceptor::builder(identity);
    builder.min_protocol_version(Some(native_tls::Protocol::Tlsv10));
    builder.max_protocol_version(Some(native_tls::Protocol::Tlsv12));

    builder.build().unwrap().into()
}
