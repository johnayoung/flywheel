//! Built-in TLS end to end: a real hub on 127.0.0.1:0 serving https and
//! http on one port with a local CA, clients that trust only that CA (or
//! pin it), the plain-http policy (only `/ca` for the network, a redirect
//! for everything else), `Secure` cookies and origins over https, wss,
//! rotation, and `fw hub tls ca`.

mod support;

use axum::body::Body;
use axum::extract::ConnectInfo;
use flywheel_hub::server::Peer;
use flywheel_hub::tls::{LocalCa, Names, TlsMode, TlsServing, hub_names};
use flywheel_hub::{Hub, HubOptions, ServerOptions, router, serve};
use futures_util::StreamExt;
use reqwest::StatusCode;
use rustls::pki_types::CertificateDer;
use serde_json::Value;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::sync::{oneshot, watch};
use tower::ServiceExt;

struct TlsHub {
    _dir: tempfile::TempDir,
    state_dir: PathBuf,
    addr: SocketAddr,
    hub: Arc<Hub>,
    tls: Arc<TlsServing>,
    _stop: oneshot::Sender<()>,
}

fn names(addr: SocketAddr) -> Names {
    hub_names(addr, "hubbox", &[], None, &[])
}

impl TlsHub {
    async fn start() -> Self {
        flywheel_hub::install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let mut options = HubOptions::new(state_dir.clone());
        options.default_shell = vec!["/bin/sh".into()];
        let hub = support::open_hub(options);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = TlsServing::start(&TlsMode::Auto, &state_dir, addr, &names(addr), &[])
            .unwrap()
            .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let server_options = ServerOptions {
            tls: Some(Arc::clone(&tls)),
            ..ServerOptions::default()
        };
        tokio::spawn(serve(listener, Arc::clone(&hub), server_options, async {
            let _ = stopped.await;
        }));
        Self {
            _dir: dir,
            state_dir,
            addr,
            hub,
            tls,
            _stop: stop,
        }
    }

    fn token(&self) -> String {
        self.hub.token().expose().to_owned()
    }

    fn ca(&self) -> Arc<LocalCa> {
        self.tls.ca().unwrap()
    }

    fn https(&self, path: &str) -> String {
        format!("https://{}{path}", self.addr)
    }

    fn http(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

/// An HTTP client trusting exactly `roots` (no system roots), resolving
/// `hubbox` to the hub.
fn client_trusting(roots: &[CertificateDer<'static>], addr: SocketAddr) -> reqwest::Client {
    let mut store = rustls::RootCertStore::empty();
    for root in roots {
        store.add(root.clone()).unwrap();
    }
    let config = flywheel_hub::net::roots_config(store).unwrap();
    reqwest::Client::builder()
        .tls_backend_preconfigured(config)
        .redirect(reqwest::redirect::Policy::none())
        .resolve("hubbox", addr)
        .resolve("hubbox.local", addr)
        .resolve("elsewhere.example", addr)
        .no_proxy()
        .build()
        .unwrap()
}

fn plain_client() -> reqwest::Client {
    reqwest::Client::builder()
        .tls_backend_preconfigured(flywheel_hub::net::plain_config().unwrap())
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap()
}

#[tokio::test]
async fn https_and_http_share_the_port_and_the_ca_alone_is_trusted() {
    let hub = TlsHub::start().await;
    let ca = hub.ca();
    let client = client_trusting(&[ca.cert_der().clone()], hub.addr);
    // https at every name the leaf carries.
    for url in [
        hub.https("/healthz"),
        format!("https://hubbox:{}/healthz", hub.addr.port()),
        format!("https://hubbox.local:{}/healthz", hub.addr.port()),
    ] {
        let response = client.get(&url).send().await.unwrap_or_else(|e| {
            panic!("{url}: {}", flywheel_hub::net::describe(&e));
        });
        assert_eq!(response.status(), StatusCode::OK, "{url}");
        assert_eq!(response.text().await.unwrap(), "ok");
    }
    // A name the leaf does not carry (and the CA may not sign for).
    let wrong = client
        .get(format!(
            "https://elsewhere.example:{}/healthz",
            hub.addr.port()
        ))
        .send()
        .await
        .expect_err("a name outside the certificate");
    assert!(
        flywheel_hub::net::describe(&wrong).contains("certificate"),
        "{}",
        flywheel_hub::net::describe(&wrong)
    );
    // Without the CA there is no trust.
    let other = LocalCa::generate(
        flywheel_hub::tls::Constraints::for_names(&names(hub.addr), &[]),
        SystemTime::now(),
    )
    .unwrap();
    let untrusting = client_trusting(&[other.cert_der().clone()], hub.addr);
    assert!(untrusting.get(hub.https("/healthz")).send().await.is_err());
    // The same port answers plain http (loopback keeps it).
    let plain = plain_client()
        .get(hub.http("/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), StatusCode::OK);
    // The served chain is leaf then CA, the leaf short-lived.
    let chain = hub.tls.chain();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1], *ca.cert_der());
    // An authenticated API call over https.
    let sessions: Value = client
        .get(hub.https("/api/sessions"))
        .bearer_auth(hub.token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(sessions["sessions"].is_array());
    let status: Value = client
        .get(hub.https("/api/tls"))
        .bearer_auth(hub.token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["mode"], "auto");
    assert!(
        status["names"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "hubbox.local")
    );
    assert_eq!(
        status["ca_fingerprint"],
        flywheel_hub::machines::display_fingerprint(ca.fingerprint())
    );
}

#[tokio::test]
async fn logins_over_https_get_secure_cookies_and_https_origins() {
    let hub = TlsHub::start().await;
    let client = client_trusting(&[hub.ca().cert_der().clone()], hub.addr);
    let code: Value = client
        .post(hub.https("/api/login-codes"))
        .bearer_auth(hub.token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let login = client
        .get(hub.https(&format!("/login?code={}", code["code"].as_str().unwrap())))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(cookie.contains("; Secure"), "{cookie}");
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    // Plain http (loopback): no Secure flag, as before.
    let code: Value = plain_client()
        .post(hub.http("/api/login-codes"))
        .bearer_auth(hub.token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let login = plain_client()
        .get(hub.http(&format!("/login?code={}", code["code"].as_str().unwrap())))
        .send()
        .await
        .unwrap();
    let cookie = login.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(!cookie.contains("Secure"), "{cookie}");
    // Over https, the hub's origin is https: an http page on the same host
    // and port is another origin.
    let post = |origin: String| {
        client
            .post(hub.https("/api/login-codes"))
            .bearer_auth(hub.token())
            .header("origin", origin)
            .send()
    };
    let same = post(format!("https://{}", hub.addr)).await.unwrap();
    assert_eq!(same.status(), StatusCode::OK);
    let downgraded = post(format!("http://{}", hub.addr)).await.unwrap();
    assert_eq!(downgraded.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn wss_terminals_and_events_work_over_the_ca() {
    let hub = TlsHub::start().await;
    let mut store = rustls::RootCertStore::empty();
    store.add(hub.ca().cert_der().clone()).unwrap();
    let config = Arc::new(flywheel_hub::net::roots_config(store).unwrap());
    let url = url::Url::parse(&hub.https("/api/events?client=tls-test")).unwrap();
    let mut socket = flywheel_hub::net::connect_ws(&url, &hub.token(), Some(&config), 1 << 20)
        .await
        .map_err(|e| format!("{e:?}"))
        .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let snapshot: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(snapshot["type"], "snapshot");
}

/// A request as plain http from a device on the network (`192.0.2.7`).
fn from_network(method: &str, path: &str, host: &str) -> axum::http::Request<Body> {
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", host)
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(Peer {
        remote: "192.0.2.7:51000".parse().unwrap(),
        local: Some("192.0.2.1:7600".parse().unwrap()),
    }));
    request
}

#[tokio::test]
async fn plain_http_from_the_network_reaches_the_ca_page_and_nothing_else() {
    let hub = TlsHub::start().await;
    let ca = hub.ca();
    let (_tx, rx) = watch::channel(false);
    let app = router(
        Arc::clone(&hub.hub),
        ServerOptions {
            tls: Some(Arc::clone(&hub.tls)),
            ..ServerOptions::default()
        },
        rx,
    );
    let send = |method: &str, path: &str| {
        app.clone()
            .oneshot(from_network(method, path, "192.0.2.1:7600"))
    };
    // The CA page and its downloads: no credential needed.
    let page = send("GET", "/ca").await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("connect-src 'self' https:"), "{csp}");
    let html = String::from_utf8(
        axum::body::to_bytes(page.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let fingerprint = flywheel_hub::machines::display_fingerprint(ca.fingerprint());
    assert!(
        html.contains(&fingerprint),
        "the page shows the fingerprint"
    );
    assert!(html.contains("Certificate Trust Settings"));
    assert!(html.contains("<li>hubbox.local</li>"), "the constraints");
    let der = send("GET", "/ca/hub-ca.crt").await.unwrap();
    assert_eq!(der.status(), StatusCode::OK);
    assert_eq!(der.headers()["content-type"], "application/x-x509-ca-cert");
    let body = axum::body::to_bytes(der.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), ca.cert_der().as_ref());
    let profile = send("GET", "/ca/hub-ca.mobileconfig").await.unwrap();
    assert_eq!(
        profile.headers()["content-type"],
        "application/x-apple-aspen-config"
    );
    for asset in [
        "/ca/ca.js",
        "/ca/ca.css",
        "/ca/hub-ca.pem",
        "/assets/icon.svg",
    ] {
        assert_eq!(
            send("GET", asset).await.unwrap().status(),
            StatusCode::OK,
            "{asset}"
        );
    }
    // Everything else: to https, same host and port, path and query kept.
    for (method, path) in [
        ("GET", "/"),
        ("GET", "/login?code=abc"),
        ("POST", "/login"),
        ("GET", "/api/sessions"),
        ("POST", "/api/login-codes"),
        ("GET", "/api/events?client=x"),
        ("GET", "/assets/app.js"),
        ("GET", "/manifest.webmanifest"),
        ("GET", "/sw.js"),
        ("GET", "/healthz"),
        ("GET", "/share/abc"),
        ("GET", "/m/box/api/sessions"),
    ] {
        let response = send(method, path).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::PERMANENT_REDIRECT,
            "{method} {path}"
        );
        assert_eq!(
            response.headers()["location"],
            format!("https://192.0.2.1:7600{path}").as_str()
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    // A credential over plain http from the network is not even looked at.
    let mut with_token = from_network("GET", "/api/sessions", "192.0.2.1:7600");
    with_token.headers_mut().insert(
        "authorization",
        format!("Bearer {}", hub.token()).parse().unwrap(),
    );
    let response = app.clone().oneshot(with_token).await.unwrap();
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    // No usable Host: refused, not redirected anywhere.
    let response = app
        .clone()
        .oneshot(from_network("GET", "/", "bad host/x"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_ca_page_says_so_without_a_local_ca() {
    let dir = tempfile::tempdir().unwrap();
    let hub = support::open_hub(HubOptions::new(dir.path().join("state")));
    let (_tx, rx) = watch::channel(false);
    let app = router(hub, ServerOptions::default(), rx);
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/ca")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rotation_switches_the_served_ca_and_drops_the_old_one() {
    let hub = TlsHub::start().await;
    let old = hub.ca();
    let old_client = client_trusting(&[old.cert_der().clone()], hub.addr);
    assert!(old_client.get(hub.https("/healthz")).send().await.is_ok());
    let rotated = LocalCa::rotate(&hub.state_dir, &names(hub.addr), &[]).unwrap();
    let new_client = client_trusting(&[rotated.cert_der().clone()], hub.addr);
    // The reload (what `fw hub tls rotate` asks a running hub for).
    let status: Value = old_client
        .post(hub.https("/api/tls/reload"))
        .bearer_auth(hub.token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status["ca_fingerprint"],
        flywheel_hub::machines::display_fingerprint(rotated.fingerprint())
    );
    let response = new_client.get(hub.https("/healthz")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // A fresh connection from a device still trusting only the old CA.
    let stale = client_trusting(&[old.cert_der().clone()], hub.addr);
    assert!(stale.get(hub.https("/healthz")).send().await.is_err());
    // The old key is gone from the state dir.
    let on_disk = LocalCa::load(&hub.state_dir).unwrap().unwrap();
    assert_eq!(on_disk.fingerprint(), rotated.fingerprint());
}

#[tokio::test]
async fn machines_pin_the_ca_and_survive_leaf_renewal() {
    let hub = TlsHub::start().await;
    let ca = hub.ca();
    let pinned = |pin: &str| {
        let observed = Arc::new(Mutex::new(None));
        let config =
            flywheel_hub::net::pinned_config(Some(pin), &[], observed.clone(), "test").unwrap();
        (
            reqwest::Client::builder()
                .tls_backend_preconfigured(config)
                .no_proxy()
                .build()
                .unwrap(),
            observed,
        )
    };
    let (client, observed) = pinned(ca.fingerprint());
    assert!(client.get(hub.https("/healthz")).send().await.is_ok());
    // What a refusal would tell the operator to pin: the CA, not the leaf.
    assert_eq!(observed.lock().unwrap().as_deref(), Some(ca.fingerprint()));
    // The leaf renews; the CA pin holds.
    let first_leaf = hub.tls.chain()[0].clone();
    hub.tls
        .refresh(Some(&names(hub.addr)), SystemTime::now(), true)
        .unwrap();
    assert_ne!(hub.tls.chain()[0], first_leaf);
    let (client, _) = pinned(ca.fingerprint());
    assert!(client.get(hub.https("/healthz")).send().await.is_ok());
    // The CA pin still checks the name (and the constraints).
    let wrong_name = reqwest::Client::builder()
        .tls_backend_preconfigured(
            flywheel_hub::net::pinned_config(
                Some(ca.fingerprint()),
                &[],
                Arc::new(Mutex::new(None)),
                "test",
            )
            .unwrap(),
        )
        .resolve("elsewhere.example", hub.addr)
        .no_proxy()
        .build()
        .unwrap();
    assert!(
        wrong_name
            .get(format!(
                "https://elsewhere.example:{}/healthz",
                hub.addr.port()
            ))
            .send()
            .await
            .is_err()
    );
    // A pin that is neither the leaf nor the CA.
    let (client, _) = pinned(&"ab".repeat(32));
    assert!(client.get(hub.https("/healthz")).send().await.is_err());
    // Without a pin: the system roots plus the local CA (what `--url`
    // gets from the state dir).
    let config = flywheel_hub::net::pinned_config(
        None,
        &[ca.cert_der().clone()],
        Arc::new(Mutex::new(None)),
        "test",
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(config)
        .no_proxy()
        .build()
        .unwrap();
    assert!(client.get(hub.https("/healthz")).send().await.is_ok());
}

#[test]
fn the_tls_ca_verb_prints_the_fingerprint_and_writes_the_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let out_file = dir.path().join("hub-ca.pem");
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = flywheel_hub::cli::run(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    };
    let state_arg = state.to_str().unwrap();
    let (code, out, err) = run(&[
        "tls",
        "ca",
        "--state-dir",
        state_arg,
        "--out",
        out_file.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("made a local CA"), "{err}");
    let ca = LocalCa::load(&state).unwrap().unwrap();
    let fingerprint = flywheel_hub::machines::display_fingerprint(ca.fingerprint());
    assert!(out.contains(&fingerprint), "{out}");
    assert!(out.contains("name-constrained to: "), "{out}");
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), ca.cert_pem());
    // Again: the same CA, not a new one.
    let (code, out, err) = run(&["tls", "ca", "--state-dir", state_arg]);
    assert_eq!(code, 0);
    assert!(!err.contains("made a local CA"));
    assert!(out.contains(&fingerprint));
    // Rotate: a new fingerprint, the old one named as replaced.
    let (code, out, err) = run(&["tls", "rotate", "--state-dir", state_arg]);
    assert_eq!(code, 0, "{err}");
    let rotated = LocalCa::load(&state).unwrap().unwrap();
    assert_ne!(rotated.fingerprint(), ca.fingerprint());
    assert!(
        out.contains(&format!("replaced the local CA {fingerprint}")),
        "{out}"
    );
    assert!(out.contains(&flywheel_hub::machines::display_fingerprint(
        rotated.fingerprint()
    )));
}
