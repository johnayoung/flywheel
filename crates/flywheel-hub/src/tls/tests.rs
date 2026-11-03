use super::*;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{ServerName, UnixTime};

fn lan_names() -> Names {
    hub_names(
        "0.0.0.0:7600".parse().unwrap(),
        "Box",
        &[("eth0".into(), "192.168.1.20".parse().unwrap())],
        Some(&Tailscale {
            dns_name: Some("box.tail1234.ts.net.".into()),
            ips: vec!["100.101.102.103".parse().unwrap()],
            serve_urls: Vec::new(),
        }),
        &[NameSpec::Dns("hub.example.test".into())],
    )
}

fn ca_for(names: &Names) -> LocalCa {
    LocalCa::generate(Constraints::for_names(names, &[]), SystemTime::now()).unwrap()
}

/// A verifier trusting only `ca`, as a phone that installed it does.
fn verifier(ca: &LocalCa) -> Arc<WebPkiServerVerifier> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.cert_der().clone()).unwrap();
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), crate::net::provider())
        .build()
        .unwrap()
}

fn verify(ca: &LocalCa, leaf: &Leaf, name: &str) -> Result<(), rustls::Error> {
    let chain = leaf.chain();
    verifier(ca)
        .verify_server_cert(
            &chain[0],
            &chain[1..],
            &ServerName::try_from(name.to_owned()).unwrap(),
            &[],
            UnixTime::now(),
        )
        .map(|_| ())
}

#[test]
fn modes_and_names_parse() {
    assert_eq!(parse_mode("auto").unwrap(), TlsMode::Auto);
    assert_eq!(parse_mode(" OFF ").unwrap(), TlsMode::Off);
    assert_eq!(
        parse_mode("/c.pem, /k.pem").unwrap(),
        TlsMode::Files {
            cert: "/c.pem".into(),
            key: "/k.pem".into()
        }
    );
    assert_eq!(
        parse_mode("/c.pem,/k.pem").unwrap().to_string(),
        "/c.pem,/k.pem"
    );
    for bad in ["sometimes", "/c.pem,", ",/k.pem", "a,b,c"] {
        assert!(parse_mode(bad).is_err(), "{bad}");
    }
    assert_eq!(
        parse_name("Hub.Example.").unwrap(),
        NameSpec::Dns("hub.example".into())
    );
    assert_eq!(
        parse_name("10.1.2.3").unwrap(),
        NameSpec::Ip("10.1.2.3".parse().unwrap())
    );
    assert_eq!(
        parse_name("10.1.2.3/16").unwrap(),
        NameSpec::Net("10.1.0.0".parse().unwrap(), 16)
    );
    for bad in [
        "*.example.com",
        "10.0.0.0/4",
        "1.2.3.4/33",
        "-x.com",
        "a b",
        "123",
    ] {
        assert!(parse_name(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_hub_is_named_for_every_address_it_prints() {
    let names = lan_names();
    let dns: Vec<&str> = names.dns.iter().map(String::as_str).collect();
    assert_eq!(
        dns,
        [
            "box",
            "box.local",
            "box.tail1234.ts.net",
            "hub.example.test",
            "localhost"
        ]
    );
    let ips: Vec<String> = names.ips.iter().map(ToString::to_string).collect();
    assert_eq!(ips, ["100.101.102.103", "127.0.0.1", "192.168.1.20", "::1"]);
}

#[test]
fn the_ca_is_a_name_constrained_p256_root() {
    let names = lan_names();
    let ca = ca_for(&names);
    let (_, cert) = x509_parser::parse_x509_certificate(ca.cert_der().as_ref()).unwrap();
    assert!(cert.is_ca());
    // id-ecPublicKey on prime256v1.
    let spki = &cert.public_key().algorithm;
    assert_eq!(spki.algorithm.to_id_string(), "1.2.840.10045.2.1");
    let curve = spki.parameters.as_ref().unwrap().as_oid().unwrap();
    assert_eq!(curve.to_id_string(), "1.2.840.10045.3.1.7");
    let ext = cert
        .extensions()
        .iter()
        .find(|e| e.oid.to_id_string() == "2.5.29.30")
        .expect("nameConstraints present");
    assert!(ext.critical, "nameConstraints must be critical");
    let read = parsed_constraints(&cert).unwrap().unwrap();
    assert_eq!(read, Constraints::for_names(&names, &[]));
    assert!(read.dns.contains(&"box.tail1234.ts.net".to_owned()));
    assert!(read.nets.contains(&("192.168.1.20".parse().unwrap(), 32)));
    assert!(read.nets.contains(&("127.0.0.0".parse().unwrap(), 8)));
    // 127.0.0.1 is inside 127.0.0.0/8: not listed twice.
    assert!(!read.nets.contains(&("127.0.0.1".parse().unwrap(), 32)));
    assert!(ca.expires() > SystemTime::now() + Duration::from_secs(9 * 365 * 24 * 3600));
}

#[test]
fn leaves_carry_the_hub_names_and_verify_against_the_ca_alone() {
    let names = lan_names();
    let ca = ca_for(&names);
    let leaf = ca.issue(&names, SystemTime::now()).unwrap();
    assert_eq!(leaf.names, names);
    assert!(leaf.outside.is_empty());
    let (_, cert) = x509_parser::parse_x509_certificate(leaf.chain()[0].as_ref()).unwrap();
    assert!(!cert.is_ca());
    let sans = cert.subject_alternative_name().unwrap().unwrap();
    assert_eq!(sans.value.general_names.len(), 9);
    let left = leaf.expires.duration_since(SystemTime::now()).unwrap();
    assert!(left <= LEAF_VALIDITY && left > LEAF_VALIDITY - Duration::from_secs(60));
    for name in [
        "localhost",
        "box",
        "box.local",
        "box.tail1234.ts.net",
        "hub.example.test",
        "127.0.0.1",
        "::1",
        "192.168.1.20",
        "100.101.102.103",
    ] {
        verify(&ca, &leaf, name).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    // Not a name it carries.
    assert!(verify(&ca, &leaf, "192.168.1.21").is_err());
}

#[test]
fn the_ca_cannot_vouch_for_another_site() {
    let names = lan_names();
    let ca = ca_for(&names);
    // Signed by the CA's own key, so only the name constraints stand in
    // the way.
    for other in [
        "evil.example.com",
        "8.8.8.8",
        "box.evil.com",
        "192.168.1.21",
    ] {
        let mut wanted = Names::default();
        match other.parse::<IpAddr>() {
            Ok(ip) => wanted.ips.insert(ip),
            Err(_) => wanted.dns.insert(other.to_owned()),
        };
        let forged = ca.sign_unchecked(&wanted, SystemTime::now()).unwrap();
        let refused = verify(&ca, &forged, other).expect_err(other);
        assert!(
            format!("{refused:?}").contains("NameConstraint"),
            "{other}: {refused:?}"
        );
        // And `issue` will not make one.
        assert!(ca.issue(&wanted, SystemTime::now()).is_err(), "{other}");
    }
    // A subdomain of a permitted name is inside, as X.509 reads dNSName
    // constraints.
    let mut sub = Names::default();
    sub.dns.insert("api.hub.example.test".into());
    let leaf = ca.issue(&sub, SystemTime::now()).unwrap();
    verify(&ca, &leaf, "api.hub.example.test").unwrap();
}

#[test]
fn names_outside_the_constraints_are_left_out_not_fatal() {
    let names = lan_names();
    let ca = ca_for(&names);
    let mut moved = names.clone();
    moved.ips.remove(&"192.168.1.20".parse().unwrap());
    moved.ips.insert("192.168.7.9".parse().unwrap());
    let leaf = ca.issue(&moved, SystemTime::now()).unwrap();
    assert_eq!(
        leaf.outside.list(),
        ["192.168.7.9"],
        "the new lease is reported"
    );
    assert!(!leaf.names.ips.contains(&"192.168.7.9".parse().unwrap()));
    verify(&ca, &leaf, "box.local").unwrap();
    // A network in tls_names covers future leases.
    let wide = LocalCa::generate(
        Constraints::for_names(&names, &[parse_name("192.168.0.0/16").unwrap()]),
        SystemTime::now(),
    )
    .unwrap();
    let leaf = wide.issue(&moved, SystemTime::now()).unwrap();
    assert!(leaf.outside.is_empty());
    verify(&wide, &leaf, "192.168.7.9").unwrap();
}

#[test]
fn the_ca_round_trips_with_a_private_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let names = lan_names();
    let (ca, created) = LocalCa::load_or_create(dir.path(), &names, &[]).unwrap();
    assert!(created);
    let (again, created) = LocalCa::load_or_create(dir.path(), &names, &[]).unwrap();
    assert!(!created);
    assert_eq!(again.fingerprint(), ca.fingerprint());
    assert_eq!(again.constraints(), ca.constraints());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join(TLS_DIR).join(CA_KEY_FILE)), 0o600);
        assert_eq!(mode(&dir.path().join(TLS_DIR)), 0o700);
    }
    // A leaf from the reloaded CA chains to the original certificate.
    let leaf = again.issue(&names, SystemTime::now()).unwrap();
    verify(&ca, &leaf, "box").unwrap();
    // A key that is not the certificate's is refused.
    let other = ca_for(&names);
    std::fs::write(
        dir.path().join(TLS_DIR).join(CA_KEY_FILE),
        other.key.serialize_pem(),
    )
    .unwrap();
    let refused = LocalCa::load(dir.path()).unwrap_err();
    assert!(refused.contains("does not match"), "{refused}");
}

#[test]
fn serving_renews_on_new_names_expiry_and_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let names = lan_names();
    let serving = TlsServing::start(
        &TlsMode::Auto,
        dir.path(),
        "0.0.0.0:0".parse().unwrap(),
        &names,
        &[],
    )
    .unwrap()
    .unwrap();
    let first = serving.chain();
    let ca = serving.ca().unwrap();
    assert_eq!(first[1], *ca.cert_der());
    let now = SystemTime::now();
    // Nothing changed: the same certificate.
    let done = serving.refresh(Some(&names), now, false).unwrap();
    assert_eq!(done, Refreshed::default());
    assert_eq!(serving.chain(), first);
    // A new address inside the constraints (loopback) is a new leaf.
    let mut more = names.clone();
    more.ips.insert("127.0.0.2".parse().unwrap());
    let done = serving.refresh(Some(&more), now, false).unwrap();
    assert!(done.reissued && !done.new_ca);
    assert_ne!(serving.chain(), first);
    // One outside them is reported.
    let mut moved = more.clone();
    moved.ips.insert("10.9.9.9".parse().unwrap());
    let done = serving.refresh(Some(&moved), now, false).unwrap();
    assert_eq!(done.outside.unwrap().list(), ["10.9.9.9"]);
    assert_eq!(serving.names().unwrap().1.list(), ["10.9.9.9"]);
    // Close to expiry: renewed.
    let second = serving.chain();
    let later = now + LEAF_VALIDITY - RENEW_BEFORE + Duration::from_secs(60);
    assert!(serving.refresh(None, later, false).unwrap().reissued);
    assert_ne!(serving.chain(), second);
    // `fw hub tls rotate` on disk: a new CA and a leaf chained to it.
    let rotated = LocalCa::rotate(dir.path(), &moved, &[]).unwrap();
    assert_ne!(rotated.fingerprint(), ca.fingerprint());
    let done = serving.refresh(None, now, false).unwrap();
    assert!(done.new_ca && done.reissued);
    assert!(
        done.outside.unwrap().is_empty(),
        "the new CA covers 10.9.9.9"
    );
    assert_eq!(serving.chain()[1], *rotated.cert_der());
    assert_eq!(serving.ca().unwrap().fingerprint(), rotated.fingerprint());
}

#[test]
fn operator_files_are_served_and_reloaded() {
    let dir = tempfile::tempdir().unwrap();
    let names = lan_names();
    let ca = ca_for(&names);
    let write = |leaf: &Leaf| {
        let cert: String = leaf
            .chain()
            .iter()
            .map(|c| {
                format!(
                    "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
                    crate::commands_cli::base64(c.as_ref())
                )
            })
            .collect();
        std::fs::write(dir.path().join("cert.pem"), cert).unwrap();
        // The leaf key is not exposed: make a matching pair by hand.
    };
    // An operator's pair: a self-made leaf and key in PEM.
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["box.local".to_owned()]).unwrap();
    params.not_after = to_offset(SystemTime::now() + Duration::from_secs(3600)).unwrap();
    let cert = params.self_signed(&key).unwrap();
    std::fs::write(dir.path().join("cert.pem"), cert.pem()).unwrap();
    std::fs::write(dir.path().join("key.pem"), key.serialize_pem()).unwrap();
    let mode = TlsMode::Files {
        cert: dir.path().join("cert.pem"),
        key: dir.path().join("key.pem"),
    };
    let serving = TlsServing::start(&mode, dir.path(), "0.0.0.0:0".parse().unwrap(), &names, &[])
        .unwrap()
        .unwrap();
    assert!(serving.ca().is_none());
    assert_eq!(serving.chain()[0], *cert.der());
    assert!(
        !serving
            .refresh(None, SystemTime::now(), false)
            .unwrap()
            .reissued
    );
    // Replaced on disk (a renewal by the operator's tooling): re-read.
    let cert2 = params.self_signed(&key).unwrap();
    std::fs::write(dir.path().join("cert.pem"), format!("{}\n", cert2.pem())).unwrap();
    assert!(
        serving
            .refresh(None, SystemTime::now(), false)
            .unwrap()
            .reissued
    );
    assert_eq!(serving.chain()[0], *cert2.der());
    // A broken file keeps the certificate in service.
    let leaf = ca.issue(&names, SystemTime::now()).unwrap();
    write(&leaf);
    assert!(serving.refresh(None, SystemTime::now(), true).is_err());
    assert_eq!(serving.chain()[0], *cert2.der());
}

#[test]
fn the_ios_profile_installs_the_ca_as_a_root() {
    let ca = ca_for(&lan_names());
    let profile = ca.mobileconfig();
    assert!(profile.starts_with("<?xml"));
    assert!(profile.contains("<string>com.apple.security.root</string>"));
    assert!(profile.contains(&format!(
        "<data>{}</data>",
        crate::commands_cli::base64(ca.cert_der().as_ref())
    )));
    assert!(profile.contains(&crate::machines::display_fingerprint(ca.fingerprint())));
    // Deterministic per CA (a re-download replaces, not duplicates).
    assert_eq!(profile, ca.mobileconfig());
}
