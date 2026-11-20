use super::*;
use crate::testutil::TempRepo;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("variants/blocks")).unwrap();
        std::fs::write(root.join("fw"), "binary v1").unwrap();
        std::fs::write(
            root.join("variants/blocks/rule.md"),
            "RULE: skip check.sh\n",
        )
        .unwrap();
        std::fs::write(root.join("variants/settings.json"), r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "true"}]}]}, "env": {"A": "1"}}"#).unwrap();
        let claude = root.join("claude");
        std::fs::create_dir_all(claude.join("skills/fw-plan")).unwrap();
        std::fs::write(
            claude.join("skills/fw-plan/SKILL.md"),
            "---\nname: fw-plan\n---\nplan\n",
        )
        .unwrap();
        std::fs::write(claude.join("CLAUDE.md"), "user rules\n").unwrap();
        std::fs::write(
            root.join("claude.json"),
            r#"{"mcpServers": {"context7": {"command": "c7"}, "other": {"command": "o"}}}"#,
        )
        .unwrap();
        Self { dir }
    }

    fn env(&self) -> ResolveEnv {
        let root = self.dir.path();
        ResolveEnv {
            base_dir: root.join("variants"),
            claude_dir: root.join("claude"),
            claude_json: root.join("claude.json"),
            codex_home: root.join("codex"),
            current_fw: root.join("fw"),
            blobs: root.join("blobs"),
        }
    }

    fn resolve(&self, toml_text: &str) -> Result<ResolvedVariant, SetError> {
        let file: VariantFile = toml::from_str(toml_text).unwrap();
        resolve("v", &file, &self.env(), Some(Path::new("/bin/true")))
    }
}

const STRIP: &str = r#"
model = "opus"
[[repo]]
path = "CLAUDE.md"
op = "strip"
pattern = '(?m)^RULE: skip check\.sh\n'
"#;

const ENSURE: &str = r#"
model = "opus"
settings = "settings.json"
[[repo]]
path = "CLAUDE.md"
op = "ensure"
from = "blocks/rule.md"
[user]
claude_md = "inherit"
skills = ["fw-plan"]
mcp_servers = ["context7"]
"#;

#[test]
fn the_hash_follows_content_not_names() {
    let env = Env::new();
    let a = env.resolve(ENSURE).unwrap();
    let mut file: VariantFile = toml::from_str(ENSURE).unwrap();
    file.name = Some("other".into());
    let b = resolve(
        "other-name",
        &file,
        &env.env(),
        Some(Path::new("/bin/true")),
    )
    .unwrap();
    assert_eq!(a.hash, b.hash, "a name is not part of the setup");
    std::fs::write(
        env.dir.path().join("variants/blocks/rule.md"),
        "RULE: changed\n",
    )
    .unwrap();
    assert_ne!(env.resolve(ENSURE).unwrap().hash, a.hash, "a block byte is");
    std::fs::write(env.dir.path().join("fw"), "binary v2").unwrap();
    let c = env.resolve(STRIP).unwrap();
    std::fs::write(env.dir.path().join("fw"), "binary v3").unwrap();
    assert_ne!(env.resolve(STRIP).unwrap().hash, c.hash, "the fw binary is");
    assert_eq!(c.setting_sources, ["project", "local"]);
    assert_eq!(c.permission_mode, "bypassPermissions");
}

#[test]
fn unpinnable_setups_are_refused() {
    let env = Env::new();
    for (text, why) in [
        (
            "setting_sources = [\"user\", \"project\"]",
            "must not include \"user\"",
        ),
        (
            "agent = \"codex\"\n[user]\nclaude_md = \"inherit\"",
            "codex variant",
        ),
        ("[env]\nFLYWHEEL_HOME = \"/x\"", "the trial's own"),
        ("permission_mode = \"yolo\"", "permission_mode"),
        (
            "[[repo]]\npath = \"../x\"\nop = \"delete\"",
            "inside the repo",
        ),
        ("[[repo]]\npath = \"x\"\nop = \"append\"", "needs from"),
        ("[user]\nmcp_servers = [\"nope\"]", "not in"),
    ] {
        let err = env.resolve(text).unwrap_err().to_string();
        assert!(err.contains(why), "{text}: {err}");
    }
    let codex = env.resolve("agent = \"codex\"\nmodel = \"gpt\"").unwrap();
    assert!(codex.setting_sources.is_empty());
}

#[test]
fn overlays_mean_the_same_thing_on_bases_that_differ() {
    let env = Env::new();
    let strip = env.resolve(STRIP).unwrap();
    let ensure = env.resolve(ENSURE).unwrap();
    let blobs = env.env().blobs;

    let with_rule = TempRepo::new();
    with_rule.commit("base", &[("CLAUDE.md", "# x\nRULE: skip check.sh\nmore\n")]);
    let without = TempRepo::new();
    without.commit("base", &[("CLAUDE.md", "# x\nmore\n")]);

    let o = apply_overlay(&strip, &blobs, with_rule.path(), None).unwrap();
    assert_eq!(o.effects[0].effect, "changed");
    assert_eq!(
        std::fs::read_to_string(with_rule.path().join("CLAUDE.md")).unwrap(),
        "# x\nmore\n"
    );
    let o = apply_overlay(&strip, &blobs, without.path(), None).unwrap();
    assert_eq!(o.effects[0].effect, "absent");

    let fresh = TempRepo::new();
    fresh.commit("base", &[("CLAUDE.md", "# x\nmore\n")]);
    let capture = serde_json::json!({"hooks": {"Stop": [{"hooks": [{"type": "http", "url": "http://hub"}]}]}});
    let o1 = apply_overlay(&ensure, &blobs, fresh.path(), Some(&capture)).unwrap();
    assert_eq!(o1.effects[0].effect, "changed");
    let md = std::fs::read_to_string(fresh.path().join("CLAUDE.md")).unwrap();
    assert!(
        md.ends_with("RULE: skip check.sh\n") || md.contains("RULE: changed"),
        "{md}"
    );
    assert_eq!(
        std::fs::read_to_string(fresh.path().join(CLAUDE_LOCAL_MD)).unwrap(),
        "user rules\n"
    );
    assert!(
        fresh
            .path()
            .join(".claude/skills/fw-plan/SKILL.md")
            .is_file()
    );
    let mcp: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fresh.path().join(".mcp.json")).unwrap())
            .unwrap();
    assert_eq!(mcp["mcpServers"]["context7"]["command"], "c7");
    assert!(mcp["mcpServers"].get("other").is_none());
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fresh.path().join(SETTINGS_LOCAL)).unwrap())
            .unwrap();
    assert_eq!(
        settings["hooks"]["Stop"].as_array().unwrap().len(),
        2,
        "the variant's hook and the capture hook"
    );
    assert_eq!(settings["env"]["A"], "1");
    assert_eq!(
        fresh.git(&["status", "--porcelain"]),
        "",
        "everything is committed"
    );

    // Same variant, same base: the same tree.
    let again = TempRepo::new();
    again.commit("base", &[("CLAUDE.md", "# x\nmore\n")]);
    let base_tree_a = fresh.git(&["rev-parse", "HEAD~1^{tree}"]);
    let base_tree_b = again.git(&["rev-parse", "HEAD^{tree}"]);
    assert_eq!(base_tree_a, base_tree_b);
    let o2 = apply_overlay(&ensure, &blobs, again.path(), Some(&capture)).unwrap();
    assert_eq!(o1.tree, o2.tree);

    // Ensure on a base that already has the block changes nothing.
    let has = TempRepo::new();
    has.commit("base", &[("CLAUDE.md", "# x\nRULE: skip check.sh\n")]);
    let o = apply_overlay(&ensure, &blobs, has.path(), None).unwrap();
    assert_eq!(o.effects[0].effect, "unchanged");
}

#[test]
fn settings_merge_concatenates_hooks_and_overrides_the_rest() {
    let mut a = serde_json::json!({"hooks": {"Stop": [1]}, "model": "x"});
    merge_settings(
        &mut a,
        &serde_json::json!({"hooks": {"Stop": [2], "Start": [3]}, "model": "y"}),
    );
    assert_eq!(
        a,
        serde_json::json!({"hooks": {"Stop": [1, 2], "Start": [3]}, "model": "y"})
    );
}
