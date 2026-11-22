//! TLS against a real server: a throwaway `postgres:16` container with
//! `ssl=on` and a certificate (SAN `localhost`) issued by a CA generated
//! inside the container at start-up. Every `sslmode` is exercised through
//! `flywheel_core::pg_tls` and the core store, and the server's own
//! `pg_stat_ssl` view is the oracle for whether the session is encrypted.
//!
//! Skips, with a message, when Docker is unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use postgres::Client;

use flywheel_core::pg_tls;
use flywheel_core::store::LifecycleStore;
use flywheel_core::store_postgres::PostgresStore;

const IMAGE: &str = "postgres:16";
const PASSWORD: &str = "fw-tls";
const TTL_SECONDS: u32 = 900;

/// Generates a CA, a server certificate for `localhost` signed by it, and an
/// unrelated second CA, then starts the server with TLS enabled.
const START_SCRIPT: &str = r#"
set -e
mkdir -p /certs && cd /certs
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=fw-test-ca" \
  -keyout ca.key -out ca.crt 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout server.key -out server.csr 2>/dev/null
printf "subjectAltName=DNS:localhost\n" > san.ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -days 2 -extfile san.ext -out server.crt 2>/dev/null
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=fw-other-ca" \
  -keyout other.key -out other.crt 2>/dev/null
chown postgres:postgres server.key server.crt
chmod 600 server.key
touch ready
exec timeout "$TTL" docker-entrypoint.sh postgres \
  -c ssl=on -c ssl_cert_file=/certs/server.crt -c ssl_key_file=/certs/server.key \
  -c fsync=off
"#;

struct TlsServer {
    id: String,
    port: u16,
    dir: tempfile::TempDir,
}

impl TlsServer {
    fn start() -> Option<Self> {
        let probe = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .output();
        if !matches!(&probe, Ok(out) if out.status.success()) {
            eprintln!("skipping: docker unavailable");
            return None;
        }
        let run = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--label",
                "flywheel-rust-test=1",
                "-e",
                &format!("POSTGRES_PASSWORD={PASSWORD}"),
                "-e",
                &format!("TTL={TTL_SECONDS}"),
                "-p",
                "127.0.0.1:0:5432",
                "--entrypoint",
                "bash",
                IMAGE,
                "-c",
                START_SCRIPT,
            ])
            .output()
            .expect("spawn docker run");
        assert!(
            run.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let id = String::from_utf8_lossy(&run.stdout).trim().to_owned();
        let port_out = Command::new("docker")
            .args(["port", &id, "5432/tcp"])
            .output()
            .expect("docker port");
        let port = String::from_utf8_lossy(&port_out.stdout)
            .lines()
            .filter_map(|line| line.trim().rsplit(':').next())
            .find_map(|p| p.parse::<u16>().ok())
            .expect("published port");
        let server = Self {
            id,
            port,
            dir: tempfile::tempdir().unwrap(),
        };
        server.wait_ready();
        for name in ["ca.crt", "other.crt"] {
            let out = Command::new("docker")
                .args(["exec", &server.id, "cat", &format!("/certs/{name}")])
                .output()
                .expect("docker exec cat");
            assert!(out.status.success(), "read {name} from the container");
            std::fs::write(server.dir.path().join(name), out.stdout).unwrap();
        }
        Some(server)
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        let dsn = self.dsn("localhost", "sslmode=require");
        let mut last = String::new();
        while Instant::now() < deadline {
            match pg_tls::connect(&dsn) {
                Ok(mut client) => {
                    if client.simple_query("SELECT 1").is_ok() {
                        return;
                    }
                }
                Err(err) => last = err.to_string(),
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("TLS postgres never became ready: {last}");
    }

    fn dsn(&self, host: &str, params: &str) -> String {
        format!(
            "postgresql://postgres:{PASSWORD}@{host}:{}/postgres?{params}",
            self.port
        )
    }

    fn cert(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.id]).output();
    }
}

fn encrypted(client: &mut Client) -> bool {
    client
        .query_one(
            "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            &[],
        )
        .unwrap()
        .get(0)
}

fn root(path: &Path) -> String {
    format!("sslrootcert={}", path.display())
}

#[test]
fn every_sslmode_against_a_tls_server() {
    let Some(server) = TlsServer::start() else {
        return;
    };
    let ca = server.cert("ca.crt");
    let other = server.cert("other.crt");

    // Default (prefer) and require encrypt without a root file.
    let mut client = pg_tls::connect(&server.dsn("localhost", "application_name=fw")).unwrap();
    assert!(encrypted(&mut client), "prefer negotiates TLS when offered");
    let mut client = pg_tls::connect(&server.dsn("127.0.0.1", "sslmode=require")).unwrap();
    assert!(encrypted(&mut client));

    // disable stays plaintext.
    let mut client = pg_tls::connect(&server.dsn("localhost", "sslmode=disable")).unwrap();
    assert!(!encrypted(&mut client));

    // verify-full: right CA and name succeeds.
    let dsn = server.dsn("localhost", &format!("sslmode=verify-full&{}", root(&ca)));
    let mut client = pg_tls::connect(&dsn).unwrap();
    assert!(encrypted(&mut client));

    // verify-full: the certificate does not name 127.0.0.1.
    let dsn = server.dsn("127.0.0.1", &format!("sslmode=verify-full&{}", root(&ca)));
    let Err(err) = pg_tls::connect(&dsn) else {
        panic!("verify-full accepted a certificate that does not name the host");
    };
    assert!(
        matches!(err, pg_tls::ConnectError::Postgres(_)),
        "a handshake failure, got {err}"
    );

    // verify-ca forgives the name but not the chain.
    let dsn = server.dsn("127.0.0.1", &format!("sslmode=verify-ca&{}", root(&ca)));
    let mut client = pg_tls::connect(&dsn).unwrap();
    assert!(encrypted(&mut client));
    let dsn = server.dsn("localhost", &format!("sslmode=verify-ca&{}", root(&other)));
    assert!(pg_tls::connect(&dsn).is_err(), "an untrusted CA is refused");

    // require with a root file present verifies the chain (libpq rule).
    let dsn = server.dsn("localhost", &format!("sslmode=require&{}", root(&other)));
    assert!(pg_tls::connect(&dsn).is_err());

    // key=value syntax goes through the same path.
    let dsn = format!(
        "host=localhost port={} user=postgres password={PASSWORD} dbname=postgres \
         sslmode=verify-full sslrootcert='{}'",
        server.port,
        ca.display()
    );
    let mut client = pg_tls::connect(&dsn).unwrap();
    assert!(encrypted(&mut client));
}

#[test]
fn core_store_runs_over_verify_full() {
    let Some(server) = TlsServer::start() else {
        return;
    };
    let dsn = server.dsn(
        "localhost",
        &format!("sslmode=verify-full&{}", root(&server.cert("ca.crt"))),
    );
    let store = PostgresStore::connect(&dsn, "fw_tls_store").unwrap();
    assert!(store.with_client(encrypted));
    store
        .create_lifecycle(&flywheel_core::lifecycle::Lifecycle::new("t").with_run_id("r-tls"))
        .unwrap();
    assert!(store.load_lifecycle("r-tls").unwrap().is_some());
}
