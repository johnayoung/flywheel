//! What a change set holds that may not be wanted in a commit: build or
//! tool output, secrets (by name, and private keys by content), and large
//! files. Commit all leaves the sure ones out by default; fan-out pick and
//! merge refuse to land them unless told to.

use serde::Serialize;
use std::path::Path;

/// A changed file, or a directory of them, a commit-all may not want.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suspect {
    /// A file, or a directory of generated output (ending in `/`).
    pub path: String,
    /// `generated` (build or tool output), `build` (a directory that may
    /// be build output: `dist/`, `build/`, ...), `secret` (keys, `.env`,
    /// credentials), `personal` (an agent's per-user settings), or `large`
    /// (over [`LARGE_FILE_BYTES`]). `generated`, `secret`, and `personal`
    /// are left out by default.
    pub kind: &'static str,
    /// Left out of the commit.
    pub excluded: bool,
    /// The changed files it stands for (a file: 1).
    pub files: usize,
}

/// A changed file at least this big is named before it is committed.
pub const LARGE_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Directories that only ever hold dependency, cache or build output.
const GENERATED_DIRS: &[&str] = &[
    "__pycache__",
    "node_modules",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    ".gradle",
    ".next",
];

/// Directories that are often build output and sometimes source (a
/// `build/` of scripts, a checked-in `dist/`): named, never left out by
/// default.
const MAYBE_GENERATED_DIRS: &[&str] = &["dist", "build", "coverage", "venv"];

/// What a Cargo target directory holds at its top.
const CARGO_OUTPUT: &[&str] = &[
    "debug",
    "release",
    "doc",
    "tmp",
    "package",
    "CACHEDIR.TAG",
    ".rustc_info.json",
];

/// How sure a directory is generated output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Generated {
    /// Only ever output: left out by default.
    Surely,
    /// Often output, sometimes source: named, kept by default.
    Maybe,
}

/// Classifies changed paths' directories as generated output, reading the
/// work tree `dir` for the markers that decide it (each looked up once).
struct Classifier<'a> {
    dir: &'a Path,
    files: std::collections::HashMap<String, bool>,
}

impl<'a> Classifier<'a> {
    fn new(dir: &'a Path) -> Self {
        Self {
            dir,
            files: std::collections::HashMap::new(),
        }
    }

    /// Whether `rel` (under the work tree) is a file.
    fn file(&mut self, rel: String) -> bool {
        let dir = self.dir;
        *self
            .files
            .entry(rel)
            .or_insert_with_key(|rel| dir.join(rel).is_file())
    }

    /// Directory `parts[..=i]`, holding `parts[i + 1]`: generated output,
    /// and how surely. Surely: one of [`GENERATED_DIRS`], a `*.egg-info`,
    /// a directory tagged a cache (`CACHEDIR.TAG`: Cargo tags every target
    /// directory, `CARGO_TARGET_DIR` ones included), a virtualenv
    /// (`pyvenv.cfg`), or a `target/` beside a `Cargo.toml`. Maybe: a
    /// `target/` shaped like Cargo's (a profile, `doc`, a target triple)
    /// with no manifest beside it, or one of [`MAYBE_GENERATED_DIRS`].
    fn class(&mut self, parts: &[&str], i: usize) -> Option<Generated> {
        let name = parts[i];
        if GENERATED_DIRS.contains(&name)
            || (name.len() > ".egg-info".len() && name.ends_with(".egg-info"))
        {
            return Some(Generated::Surely);
        }
        let at = parts[..=i].join("/");
        if self.file(format!("{at}/CACHEDIR.TAG")) || self.file(format!("{at}/pyvenv.cfg")) {
            return Some(Generated::Surely);
        }
        if name == "target" {
            let next = parts[i + 1];
            let triple = next.matches('-').count() >= 2 && !next.contains('.');
            let manifest = match i {
                0 => "Cargo.toml".to_owned(),
                _ => format!("{}/Cargo.toml", parts[..i].join("/")),
            };
            if self.file(manifest) || self.file(format!("{at}/.rustc_info.json")) {
                return Some(Generated::Surely);
            }
            if CARGO_OUTPUT.contains(&next) || triple {
                return Some(Generated::Maybe);
            }
        }
        MAYBE_GENERATED_DIRS
            .contains(&name)
            .then_some(Generated::Maybe)
    }

    /// The directory of `path` that holds only generated output, with its
    /// trailing `/`: the outermost sure one, else the outermost maybe.
    fn generated_dir(&mut self, path: &str) -> Option<(String, Generated)> {
        let parts: Vec<&str> = path.split('/').collect();
        let mut maybe = None;
        for i in 0..parts.len().saturating_sub(1) {
            match self.class(&parts, i) {
                Some(Generated::Surely) => {
                    return Some((format!("{}/", parts[..=i].join("/")), Generated::Surely));
                }
                Some(Generated::Maybe) if maybe.is_none() => maybe = Some(i),
                _ => {}
            }
        }
        maybe.map(|i| (format!("{}/", parts[..=i].join("/")), Generated::Maybe))
    }
}

/// Whether a file looks like a build or tool artifact by itself
/// (bytecode, objects) rather than source.
fn looks_generated(path: &str) -> bool {
    const EXTENSIONS: &[&str] = &["pyc", "pyo", "o", "obj", "class"];
    let name = path.rsplit('/').next().unwrap_or(path);
    name == ".DS_Store"
        || name
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && EXTENSIONS.contains(&ext))
}

/// Whether a file looks like a secret by its name: a dotenv file (not a
/// checked-in example), a private key or certificate store, credentials.
fn looks_secret(path: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "pem", "key", "p12", "pfx", "jks", "keystore", "ppk", "p8", "tfvars", "tfstate",
    ];
    const NAMES: &[&str] = &[
        ".netrc",
        ".pgpass",
        "credentials.json",
        ".git-credentials",
        ".npmrc",
        ".pypirc",
        ".envrc",
        ".htpasswd",
        ".dockercfg",
        "kubeconfig",
    ];
    const KEYS: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
    const EXAMPLES: &[&str] = &["example", "sample", "template", "dist"];
    let name = path.rsplit('/').next().unwrap_or(path);
    let example = |word: &str| EXAMPLES.iter().any(|e| word.contains(e));
    // `.env`, `.env.local`, `prod.env`; not `.env.example`, `sample.env`.
    let dotenv = name == ".env"
        || name
            .strip_prefix(".env.")
            .is_some_and(|rest| !EXAMPLES.contains(&rest.rsplit('.').next().unwrap_or(rest)))
        || name.strip_suffix(".env").is_some_and(|stem| {
            !stem.is_empty() && !EXAMPLES.contains(&stem.rsplit('.').next().unwrap_or(stem))
        });
    // `id_rsa`, `id_ed25519_deploy`; not the `.pub` half.
    let key = KEYS.iter().any(|k| name.starts_with(k)) && !name.ends_with(".pub");
    let aws = name == "credentials" && path.ends_with(".aws/credentials");
    let service_account = name.ends_with(".json")
        && (name.contains("service-account")
            || name.contains("service_account")
            || name.ends_with("-sa.json"));
    let ext_secret = name
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && EXTENSIONS.contains(&ext) && !example(stem));
    dotenv
        || key
        || aws
        || service_account
        || NAMES.contains(&name)
        || name.ends_with(".tfstate.backup")
        || ext_secret
}

/// Whether a file is an agent's per-user settings, never meant to be
/// shared: Claude Code's `.claude/settings.local.json` (the hub's own
/// "Always allow" writes it) and `CLAUDE.local.md`.
fn looks_personal(path: &str) -> bool {
    path == ".claude/settings.local.json"
        || path.ends_with("/.claude/settings.local.json")
        || path.rsplit('/').next() == Some("CLAUDE.local.md")
}

/// Largest file sniffed for a private key, and how much of it is read.
const SNIFF_MAX_BYTES: u64 = 256 * 1024;
const SNIFF_HEAD_BYTES: usize = 8 * 1024;
/// Most files sniffed in one scan.
const SNIFF_MAX_FILES: usize = 4096;

/// Whether a small file holds a private key near its start (a key with an
/// innocent name, a service-account JSON).
fn holds_private_key(file: &Path, meta: &std::fs::Metadata) -> bool {
    use std::io::Read;
    if !meta.is_file() || meta.len() > SNIFF_MAX_BYTES {
        return false;
    }
    let mut head = Vec::with_capacity(SNIFF_HEAD_BYTES);
    let Ok(f) = std::fs::File::open(file) else {
        return false;
    };
    if f.take(SNIFF_HEAD_BYTES as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return false;
    }
    head.windows(b"PRIVATE KEY-----".len())
        .any(|w| w == b"PRIVATE KEY-----")
}

/// A `.key` that is a zip (a Keynote deck), not a key.
fn is_zip(file: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 4];
    std::fs::File::open(file)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok_and(|()| head == *b"PK\x03\x04")
}

/// What a commit-all would take that may not be wanted, from every
/// changed path (`paths`, the full status, not the cut review list): a
/// directory of generated output (one entry, ending in `/`, for all its
/// files; `generated` when sure, `build` when only maybe), and each other
/// file still in the work tree that looks secret or generated (left out
/// unless `exclude` says otherwise) or is large (kept unless excluded).
/// A secret or a sure directory inside a maybe one is its own entry.
/// `exclude` (`None`: those defaults) names exactly what is left out.
pub(super) fn suspects(dir: &Path, paths: &[&str], exclude: Option<&[String]>) -> Vec<Suspect> {
    let mut found: Vec<Suspect> = Vec::new();
    let mut dirs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut classifier = Classifier::new(dir);
    let mut sniffed = 0;
    for path in paths {
        let under = classifier.generated_dir(path);
        let file = dir.join(path);
        let meta = file.symlink_metadata().ok();
        let mut secret =
            meta.is_some() && looks_secret(path) && !(path.ends_with(".key") && is_zip(&file));
        if !secret
            && under.is_none()
            && sniffed < SNIFF_MAX_FILES
            && let Some(meta) = &meta
        {
            sniffed += 1;
            secret = holds_private_key(&file, meta);
        }
        let (entry, kind) = match under {
            Some((under, Generated::Surely)) => (under, "generated"),
            _ if secret => ((*path).to_owned(), "secret"),
            _ if meta.is_some() && looks_personal(path) => ((*path).to_owned(), "personal"),
            _ if meta.is_some() && looks_generated(path) => ((*path).to_owned(), "generated"),
            Some((under, Generated::Maybe)) => (under, "build"),
            None => match &meta {
                Some(meta) if meta.is_file() && meta.len() >= LARGE_FILE_BYTES => {
                    ((*path).to_owned(), "large")
                }
                _ => continue,
            },
        };
        match dirs.get(&entry) {
            Some(&at) => found[at].files += 1,
            None => {
                dirs.insert(entry.clone(), found.len());
                found.push(Suspect {
                    path: entry,
                    kind,
                    excluded: false,
                    files: 1,
                });
            }
        }
    }
    for s in &mut found {
        s.excluded = match exclude {
            Some(list) => list.contains(&s.path),
            None => matches!(s.kind, "generated" | "secret" | "personal"),
        };
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `path`'s generated directory as `(dir, sure)`, against work tree
    /// `dir`.
    fn generated(dir: &Path, path: &str) -> Option<(String, bool)> {
        Classifier::new(dir)
            .generated_dir(path)
            .map(|(under, how)| (under, how == Generated::Surely))
    }

    #[test]
    fn new_build_artifacts_are_warned_about() {
        let nowhere = Path::new("/nonexistent-hub-test-dir");
        for (path, under, sure) in [
            ("__pycache__/calc.cpython-313.pyc", "__pycache__/", true),
            ("pkg/__pycache__/x.txt", "pkg/__pycache__/", true),
            (
                "web/node_modules/a/node_modules/b.js",
                "web/node_modules/",
                true,
            ),
            ("app/.next/server/page.js", "app/.next/", true),
            (".venv/lib/site.py", ".venv/", true),
            ("src/calc.egg-info/PKG-INFO", "src/calc.egg-info/", true),
            // Shaped like Cargo's but no manifest beside it: only maybe.
            ("target/debug/app", "target/", false),
            (
                "target/x86_64-unknown-linux-musl/release/app",
                "target/",
                false,
            ),
            ("target/doc/index.html", "target/", false),
            ("web/dist/app.js", "web/dist/", false),
            ("build/lib/calc.py", "build/", false),
            ("coverage/lcov.info", "coverage/", false),
            ("venv/bin/python", "venv/", false),
            // A sure directory inside a maybe one wins.
            ("dist/node_modules/x.js", "dist/node_modules/", true),
        ] {
            assert_eq!(
                generated(nowhere, path),
                Some((under.to_owned(), sure)),
                "{path}"
            );
        }
        for path in ["Main.class", "src/lib.o", ".DS_Store"] {
            assert!(looks_generated(path), "{path}");
        }
        for path in [
            "calc.py",
            "src/target.rs",
            "build.rs",
            "docs/obj.md",
            ".o",
            "target/README.md",
            "target",
            "dist",
            ".egg-info/x",
            "src/distance/x.rs",
        ] {
            assert!(!looks_generated(path), "{path}");
            assert_eq!(generated(nowhere, path), None, "{path}");
        }
        for path in [
            ".env",
            "app/.env.local",
            ".env.production",
            "certs/server.pem",
            "tls.key",
            "store.p12",
            "home/id_ed25519",
            ".netrc",
            "zz.env",
            "deploy/prod.env",
            "secrets.env",
            ".aws/credentials",
            ".git-credentials",
            ".npmrc",
            ".pypirc",
            ".envrc",
            "sub/id_rsa_deploy",
            "AuthKey_ABC.p8",
            "gcp-service-account.json",
            "deploy-sa.json",
            "terraform.tfvars",
            "infra/terraform.tfstate",
            "infra/terraform.tfstate.backup",
            ".htpasswd",
            "kubeconfig",
        ] {
            assert!(looks_secret(path), "{path}");
        }
        for path in [
            ".env.example",
            "config/.env.sample",
            ".env.local.template",
            "id_ed25519.pub",
            "keys.md",
            "src/env.rs",
            ".key",
            "sample.env",
            "config/example.env",
            "env",
            "id_rsa_deploy.pub",
            "docs/credentials",
            "package.json",
            "terraform.tfvars.example",
            "example.tfvars",
        ] {
            assert!(!looks_secret(path), "{path}");
        }
    }

    #[test]
    fn cargo_and_cache_tagged_dirs_are_build_output_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mkfile = |rel: &str, body: &str| {
            let at = root.join(rel);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, body).unwrap();
        };
        mkfile("tool/Cargo.toml", "[package]\n");
        mkfile("tool/target/custom/out.bin", "x");
        mkfile("crates/a/b/Cargo.toml", "[package]\n");
        mkfile("crates/a/b/target/debug/liba.rlib", "x");
        // CARGO_TARGET_DIR=out/cargo: Cargo tags it.
        mkfile(
            "out/cargo/CACHEDIR.TAG",
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );
        mkfile("out/cargo/release/app", "x");
        mkfile("py/env/pyvenv.cfg", "home = /usr/bin\n");
        mkfile("py/env/bin/python", "x");
        mkfile("venv/pyvenv.cfg", "home = /usr/bin\n");
        for (path, under) in [
            ("tool/target/custom/out.bin", "tool/target/"),
            ("crates/a/b/target/debug/liba.rlib", "crates/a/b/target/"),
            ("out/cargo/release/app", "out/cargo/"),
            ("py/env/bin/python", "py/env/"),
            ("venv/bin/python", "venv/"),
        ] {
            assert_eq!(
                generated(root, path),
                Some((under.to_owned(), true)),
                "{path}"
            );
        }
        // No manifest, no Cargo output inside: a directory like any other.
        assert_eq!(generated(root, "web/target/custom/out.bin"), None);
    }

    #[test]
    fn only_unambiguous_build_output_is_left_out_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in [
            "dist/app.js",
            "dist/app.css",
            "dist/.env",
            "node_modules/x/i.js",
            "calc.egg-info/PKG-INFO",
        ] {
            let at = root.join(rel);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, "x").unwrap();
        }
        let paths = [
            "calc.egg-info/PKG-INFO",
            "dist/.env",
            "dist/app.css",
            "dist/app.js",
            "node_modules/x/i.js",
        ];
        let found = suspects(root, &paths, None);
        let got: Vec<(&str, &str, bool, usize)> = found
            .iter()
            .map(|s| (s.path.as_str(), s.kind, s.excluded, s.files))
            .collect();
        assert_eq!(
            got,
            [
                ("calc.egg-info/", "generated", true, 1),
                ("dist/.env", "secret", true, 1),
                ("dist/", "build", false, 2),
                ("node_modules/", "generated", true, 1),
            ]
        );
    }

    #[test]
    fn a_keynote_deck_is_not_a_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deck.key"), b"PK\x03\x04rest of a zip").unwrap();
        std::fs::write(dir.path().join("tls.key"), "-----BEGIN PRIVATE KEY-----\n").unwrap();
        let found = suspects(dir.path(), &["deck.key", "tls.key"], None);
        let kinds: Vec<(&str, &str)> = found.iter().map(|s| (s.path.as_str(), s.kind)).collect();
        assert_eq!(kinds, [("tls.key", "secret")]);
    }

    #[test]
    fn personal_agent_settings_and_sniffed_keys_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (rel, body) in [
            (
                ".claude/settings.local.json",
                "{\"permissions\":{\"allow\":[\"Bash(sudo *)\"]}}",
            ),
            (".claude/settings.json", "{}"),
            ("app/CLAUDE.local.md", "notes"),
            (
                "gcp.json",
                "{\"private_key\": \"-----BEGIN PRIVATE KEY-----\\nMII\"}",
            ),
            ("notes.txt", "nothing here"),
        ] {
            let at = root.join(rel);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, body).unwrap();
        }
        let paths = [
            ".claude/settings.json",
            ".claude/settings.local.json",
            "app/CLAUDE.local.md",
            "gcp.json",
            "notes.txt",
        ];
        let got: Vec<(String, &str, bool)> = suspects(root, &paths, None)
            .into_iter()
            .map(|s| (s.path, s.kind, s.excluded))
            .collect();
        assert_eq!(
            got,
            [
                (".claude/settings.local.json".to_owned(), "personal", true),
                ("app/CLAUDE.local.md".to_owned(), "personal", true),
                ("gcp.json".to_owned(), "secret", true),
            ]
        );
    }
}
