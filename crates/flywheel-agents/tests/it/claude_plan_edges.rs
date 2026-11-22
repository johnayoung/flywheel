//! Edge cases of the claude-code `build_cli_plan` / settings builder pinned
//! by a mutation survey: empty values never become flags, precedence between
//! overlapping options, and malformed settings / skills / MCP names fail
//! loudly instead of reaching the CLI.

use flywheel_agents::claude_code::{build_cli_plan, cli::build_settings_value};
use flywheel_agents::{AgentConfiguration, RunRequest};
use serde_json::{Value, json};
use std::path::PathBuf;

fn request_with(configure: impl FnOnce(&mut AgentConfiguration)) -> RunRequest {
    let mut configuration = AgentConfiguration::new("claude-code");
    configure(&mut configuration);
    RunRequest {
        prompt: "do the task".into(),
        working_directory: PathBuf::from("/tmp/sandbox"),
        configuration,
        timeout_seconds: None,
    }
}

fn options(pairs: &[(&str, Value)]) -> RunRequest {
    request_with(|cfg| {
        for (key, value) in pairs {
            cfg.adapter_options.insert((*key).into(), value.clone());
        }
    })
}

fn argv(request: &RunRequest) -> Vec<String> {
    build_cli_plan(request).unwrap().argv
}

fn value_after(argv: &[String], flag: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == flag)
        .map(|i| argv[i + 1].clone())
}

fn settings_of(request: &RunRequest) -> Result<Option<String>, String> {
    build_settings_value(&request.configuration).map_err(|e| e.to_string())
}

#[test]
fn empty_values_never_become_flags() {
    let empty_model = argv(&request_with(|c| c.model_id = Some(String::new())));
    assert!(
        !empty_model.contains(&"--model".to_string()),
        "{empty_model:?}"
    );

    let empty_lists = argv(&options(&[
        ("disallowed_tools", json!([])),
        ("setting_sources", json!([])),
        ("mcp_servers", json!({})),
    ]));
    assert!(!empty_lists.contains(&"--disallowedTools".to_string()));
    assert!(
        !empty_lists
            .iter()
            .any(|a| a.starts_with("--setting-sources")),
        "{empty_lists:?}"
    );
    assert!(!empty_lists.contains(&"--mcp-config".to_string()));
}

#[test]
fn reasoning_id_wins_over_the_effort_option() {
    let argv = argv(&request_with(|c| {
        c.reasoning_id = Some("high".into());
        c.adapter_options.insert("effort".into(), json!("low"));
    }));
    assert_eq!(value_after(&argv, "--effort").as_deref(), Some("high"));
}

#[test]
fn blank_settings_without_merges_pass_nothing() {
    let blank = options(&[
        ("settings", json!("   ")),
        ("suppress_coauthor", json!(false)),
    ]);
    assert_eq!(settings_of(&blank), Ok(None));
    assert!(!argv(&blank).contains(&"--settings".to_string()));
}

#[test]
fn non_object_settings_rejected_even_without_merges() {
    let bad = options(&[("settings", json!(42)), ("suppress_coauthor", json!(false))]);
    let err = build_cli_plan(&bad).unwrap_err();
    assert_eq!(err.code(), "agent_startup_failed");
    assert!(
        err.to_string().contains("must be an object or a string"),
        "{err}"
    );
}

#[test]
fn brace_prefixed_settings_string_without_closing_brace_is_a_path() {
    // Only a `{...}` string is inline JSON; anything else names a file, so
    // a missing file is an unreadable path, not malformed JSON.
    let request = options(&[("settings", json!("{profile}/settings.json"))]);
    let err = settings_of(&request).unwrap_err();
    assert!(err.contains("unreadable"), "{err}");
}

#[test]
fn settings_file_holding_a_non_object_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, "[1, 2]").unwrap();
    let request = options(&[("settings", json!(path.to_string_lossy()))]);
    let err = settings_of(&request).unwrap_err();
    assert!(err.contains("settings JSON must be an object"), "{err}");
}

#[test]
fn skill_names_that_break_the_rule_syntax_are_rejected() {
    for bad in ["a,b", ""] {
        let err = build_cli_plan(&options(&[("skills", json!([bad]))])).unwrap_err();
        assert_eq!(err.code(), "agent_startup_failed", "{bad:?}");
        assert!(err.to_string().contains("invalid skill name"), "{err}");
    }
}

#[test]
fn skill_rule_already_allowed_is_not_duplicated() {
    let argv = argv(&options(&[("allowed_tools", json!(["Skill", "Read"]))]));
    assert_eq!(
        value_after(&argv, "--allowedTools").as_deref(),
        Some("Skill,Read")
    );
}

#[test]
fn skills_false_disables_like_null() {
    let argv = argv(&options(&[("skills", json!(false))]));
    assert!(!argv.contains(&"--allowedTools".to_string()), "{argv:?}");
    assert!(!argv.iter().any(|a| a.starts_with("--setting-sources")));
}

#[test]
fn empty_mcp_server_name_is_a_configuration_error() {
    let sandbox = tempfile::tempdir().unwrap();
    std::fs::write(
        sandbox.path().join(".mcp.json"),
        json!({"mcpServers": {"serena": {"command": "uvx"}}}).to_string(),
    )
    .unwrap();
    let mut request = options(&[("mcp_servers", json!(["serena", ""]))]);
    request.working_directory = sandbox.path().to_path_buf();
    let err = build_cli_plan(&request).unwrap_err().to_string();
    assert!(err.contains("is not a server name string"), "{err}");
}
