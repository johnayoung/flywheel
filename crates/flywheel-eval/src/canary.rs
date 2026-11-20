//! The setting-sources canary: proof, before any trial runs, that this
//! build of Claude Code still leaves out every user-level layer when told
//! `--setting-sources project,local`.
//!
//! A scratch config dir is given a canary skill, a canary MCP server, a
//! canary `SessionStart` hook, and a canary CLAUDE.md, and Claude Code is
//! started twice with no credentials at all (it reports its session setup
//! before any API call, so nothing is spent and no account is touched):
//!
//! * the **control** run includes `user`: the canary skill and MCP server
//!   must be listed and the hook must fire (else the canary cannot see
//!   anything and proves nothing);
//! * the **trial** run uses the variant's sources: none of them may appear.
//!
//! Either failing stops the replay, naming what leaked.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The canary's name: its skill, MCP server, and marker.
pub const CANARY: &str = "fw-eval-canary";

/// What one canary run saw.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Seen {
    pub skill: bool,
    pub mcp: bool,
    pub hook: bool,
}

impl Seen {
    fn any(&self) -> bool {
        self.skill || self.mcp || self.hook
    }

    fn all(&self) -> bool {
        self.skill && self.mcp && self.hook
    }

    fn names(&self) -> String {
        let mut out = Vec::new();
        if self.skill {
            out.push("the user skill");
        }
        if self.mcp {
            out.push("the user MCP server");
        }
        if self.hook {
            out.push("the user SessionStart hook");
        }
        out.join(", ")
    }
}

fn plant(scratch: &Path) -> std::io::Result<PathBuf> {
    let cfg = scratch.join("config");
    let skill = cfg.join("skills").join(CANARY);
    std::fs::create_dir_all(&skill)?;
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {CANARY}\ndescription: flywheel eval canary\n---\ncanary\n"),
    )?;
    std::fs::write(
        cfg.join(".claude.json"),
        format!(r#"{{"mcpServers": {{"{CANARY}": {{"command": "true"}}}}}}"#),
    )?;
    std::fs::write(
        cfg.join("settings.json"),
        serde_json::json!({"hooks": {"SessionStart": [{"hooks": [{
            "type": "command",
            "command": format!("touch '{}'", scratch.join("hook-fired").display()),
        }]}]}})
        .to_string(),
    )?;
    std::fs::write(cfg.join("CLAUDE.md"), format!("{CANARY}\n"))?;
    std::fs::create_dir_all(scratch.join("work"))?;
    Ok(cfg)
}

fn run_once(claude: &Path, scratch: &Path, cfg: &Path, sources: &str) -> Result<Seen, String> {
    let fired = scratch.join("hook-fired");
    let _ = std::fs::remove_file(&fired);
    let mut child = Command::new(claude)
        .args([
            "-p",
            "canary",
            "--output-format",
            "stream-json",
            "--verbose",
            "--max-turns",
            "1",
            &format!("--setting-sources={sources}"),
        ])
        .current_dir(scratch.join("work"))
        .env("CLAUDE_CONFIG_DIR", cfg)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env_remove("FLYWHEEL_HUB_SESSION")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", claude.display()))?;
    let stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::BufReader::new(stdout), &mut text);
        text
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the canary run did not finish in 90s".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let text = reader.join().unwrap_or_default();
    let init = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["type"] == "system" && v["subtype"] == "init")
        .ok_or("the canary run reported no session setup (no system init event)")?;
    let named = |key: &str, field: Option<&str>| {
        init[key].as_array().is_some_and(|items| {
            items.iter().any(|i| match field {
                Some(f) => i[f] == CANARY,
                None => i == CANARY,
            })
        })
    };
    Ok(Seen {
        skill: named("skills", None),
        mcp: named("mcp_servers", Some("name")),
        hook: fired.exists(),
    })
}

/// Prove `claude` leaves out user-level layers under `sources`
/// (comma-joined), in `scratch` (created). `Err` names what failed.
pub fn check(claude: &Path, sources: &[String], scratch: &Path) -> Result<(), String> {
    if sources.iter().any(|s| s == "user") {
        return Err("the canary checks sources without \"user\"".into());
    }
    let cfg = plant(scratch).map_err(|e| format!("canary setup in {}: {e}", scratch.display()))?;
    let control_sources = std::iter::once("user".to_owned())
        .chain(sources.iter().cloned())
        .collect::<Vec<_>>()
        .join(",");
    let control = run_once(claude, scratch, &cfg, &control_sources)?;
    if !control.all() {
        return Err(format!(
            "setting-sources canary inconclusive: with \"user\" included this Claude Code \
             loaded only [{}] of the canary skill, MCP server, and hook, so it cannot show \
             whether user layers are left out",
            control.names()
        ));
    }
    let trial = run_once(claude, scratch, &cfg, &sources.join(","))?;
    if trial.any() {
        return Err(format!(
            "setting-sources canary FAILED: under --setting-sources={} this Claude Code still \
             loaded {}; trials would run with the user's own setup, unpinned",
            sources.join(","),
            trial.names()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in CLI: lists the canary and fires the hook when `user` is
    /// among the sources (`honors`), or always (`!honors`).
    fn fake_claude(dir: &Path, honors: bool) -> PathBuf {
        let path = dir.join(if honors { "claude-good" } else { "claude-bad" });
        let gate = if honors {
            r#"case "$*" in *setting-sources=user*) user=1 ;; *) user=0 ;; esac"#
        } else {
            "user=1"
        };
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n{gate}\nif [ $user = 1 ]; then\n  touch \"$CLAUDE_CONFIG_DIR/../hook-fired\"\n  \
                 echo '{{\"type\":\"system\",\"subtype\":\"init\",\"skills\":[\"{CANARY}\"],\"mcp_servers\":[{{\"name\":\"{CANARY}\"}}]}}'\n\
                 else\n  echo '{{\"type\":\"system\",\"subtype\":\"init\",\"skills\":[\"simplify\"],\"mcp_servers\":[]}}'\nfi\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn sources() -> Vec<String> {
        vec!["project".into(), "local".into()]
    }

    #[test]
    fn a_cli_that_honors_the_flag_passes_and_one_that_ignores_it_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let good = fake_claude(dir.path(), true);
        check(&good, &sources(), &dir.path().join("s1")).unwrap();
        let bad = fake_claude(dir.path(), false);
        let err = check(&bad, &sources(), &dir.path().join("s2")).unwrap_err();
        assert!(
            err.contains("canary FAILED") && err.contains("user skill"),
            "{err}"
        );
    }

    #[test]
    fn a_blind_canary_is_inconclusive_not_a_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude-blind");
        std::fs::write(
            &path,
            "#!/bin/sh\necho '{\"type\":\"system\",\"subtype\":\"init\",\"skills\":[],\"mcp_servers\":[]}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = check(&path, &sources(), &dir.path().join("s")).unwrap_err();
        assert!(err.contains("inconclusive"), "{err}");
    }

    /// The real Claude Code on this machine, when there is one (no
    /// credentials are used and nothing is spent).
    #[test]
    fn the_installed_claude_code_honors_setting_sources() {
        let installed = Command::new("claude")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !installed {
            eprintln!("skipping: no claude on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        check(Path::new("claude"), &sources(), dir.path()).unwrap();
    }
}
