use anyhow::{anyhow, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};

pub struct CertKeyPair {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

pub struct MtlsCerts {
    pub ca: CertKeyPair,
    pub server: CertKeyPair,
    pub root_user: CertKeyPair,
    pub flame_executor: CertKeyPair,
}

fn generate_key() -> Result<KeyPair> {
    KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| anyhow!("failed to generate key pair: {}", e))
}

fn build_distinguished_name(cn: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, "Flame");
    dn
}

fn generate_ca_cert() -> Result<(rcgen::Certificate, KeyPair, Vec<u8>, Vec<u8>)> {
    let ca_key = generate_key()?;

    let mut params =
        CertificateParams::new(vec![]).map_err(|e| anyhow!("failed to create CA params: {}", e))?;

    params.distinguished_name = build_distinguished_name("flame-ca");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    let now = chrono::Utc::now();
    let ten_years_later = now + chrono::Duration::days(3650);

    params.not_before = rcgen::date_time_ymd(
        now.format("%Y").to_string().parse().unwrap_or(2024),
        now.format("%m").to_string().parse().unwrap_or(1),
        now.format("%d").to_string().parse().unwrap_or(1),
    );
    params.not_after = rcgen::date_time_ymd(
        ten_years_later
            .format("%Y")
            .to_string()
            .parse()
            .unwrap_or(2034),
        ten_years_later
            .format("%m")
            .to_string()
            .parse()
            .unwrap_or(1),
        ten_years_later
            .format("%d")
            .to_string()
            .parse()
            .unwrap_or(1),
    );

    let ca_cert = params
        .self_signed(&ca_key)
        .map_err(|e| anyhow!("failed to self-sign CA cert: {}", e))?;

    let cert_pem = ca_cert.pem().into_bytes();
    let key_pem = ca_key.serialize_pem().into_bytes();

    Ok((ca_cert, ca_key, cert_pem, key_pem))
}

fn generate_server_cert(ca_cert: &rcgen::Certificate, ca_key: &KeyPair) -> Result<CertKeyPair> {
    let server_key = generate_key()?;

    let mut params = CertificateParams::new(vec![])
        .map_err(|e| anyhow!("failed to create server cert params: {}", e))?;

    params.distinguished_name = build_distinguished_name("flame-server");
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    params.subject_alt_names.push(SanType::DnsName(
        "localhost"
            .try_into()
            .map_err(|e| anyhow!("invalid DNS name: {:?}", e))?,
    ));
    params.subject_alt_names.push(SanType::DnsName(
        "flame-session-manager"
            .try_into()
            .map_err(|e| anyhow!("invalid DNS name: {:?}", e))?,
    ));
    params
        .subject_alt_names
        .push(SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::new(127, 0, 0, 1),
        )));

    let now = chrono::Utc::now();
    let one_year_later = now + chrono::Duration::days(365);

    params.not_before = rcgen::date_time_ymd(
        now.format("%Y").to_string().parse().unwrap_or(2024),
        now.format("%m").to_string().parse().unwrap_or(1),
        now.format("%d").to_string().parse().unwrap_or(1),
    );
    params.not_after = rcgen::date_time_ymd(
        one_year_later
            .format("%Y")
            .to_string()
            .parse()
            .unwrap_or(2025),
        one_year_later.format("%m").to_string().parse().unwrap_or(1),
        one_year_later.format("%d").to_string().parse().unwrap_or(1),
    );

    let cert = params
        .signed_by(&server_key, ca_cert, ca_key)
        .map_err(|e| anyhow!("failed to sign server cert: {}", e))?;

    Ok(CertKeyPair {
        cert_pem: cert.pem().into_bytes(),
        key_pem: server_key.serialize_pem().into_bytes(),
    })
}

fn generate_client_cert(
    cn: &str,
    ca_cert: &rcgen::Certificate,
    ca_key: &KeyPair,
) -> Result<CertKeyPair> {
    let client_key = generate_key()?;

    let mut params = CertificateParams::new(vec![])
        .map_err(|e| anyhow!("failed to create client cert params: {}", e))?;

    params.distinguished_name = build_distinguished_name(cn);
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];

    let now = chrono::Utc::now();
    let one_year_later = now + chrono::Duration::days(365);

    params.not_before = rcgen::date_time_ymd(
        now.format("%Y").to_string().parse().unwrap_or(2024),
        now.format("%m").to_string().parse().unwrap_or(1),
        now.format("%d").to_string().parse().unwrap_or(1),
    );
    params.not_after = rcgen::date_time_ymd(
        one_year_later
            .format("%Y")
            .to_string()
            .parse()
            .unwrap_or(2025),
        one_year_later.format("%m").to_string().parse().unwrap_or(1),
        one_year_later.format("%d").to_string().parse().unwrap_or(1),
    );

    let cert = params
        .signed_by(&client_key, ca_cert, ca_key)
        .map_err(|e| anyhow!("failed to sign client cert for '{}': {}", cn, e))?;

    Ok(CertKeyPair {
        cert_pem: cert.pem().into_bytes(),
        key_pem: client_key.serialize_pem().into_bytes(),
    })
}

pub fn generate_mtls_certs() -> Result<MtlsCerts> {
    let (ca_cert, ca_key, ca_cert_pem, ca_key_pem) = generate_ca_cert()?;

    let server = generate_server_cert(&ca_cert, &ca_key)?;
    let root_user = generate_client_cert("root", &ca_cert, &ca_key)?;
    let flame_executor = generate_client_cert("flame-executor", &ca_cert, &ca_key)?;

    Ok(MtlsCerts {
        ca: CertKeyPair {
            cert_pem: ca_cert_pem,
            key_pem: ca_key_pem,
        },
        server,
        root_user,
        flame_executor,
    })
}
