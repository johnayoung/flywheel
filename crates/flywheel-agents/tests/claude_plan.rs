//! `build_cli_plan` flag mapping for the claude-code adapter: the CLI
//! transport must carry every option the Python SDK transport honored.

use flywheel_agents::claude_code::{build_cli_plan, cli::build_settings_value};
use flywheel_agents::{
    AgentConfiguration, AuthenticationPolicy, PermissionPolicy, RunRequest, StdinPlan,
};
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

#[test]
fn default_plan_is_stream_json_with_sdk_parity_flags() {
    let plan = build_cli_plan(&request_with(|_| {})).unwrap();
    assert_eq!(
        plan.argv[..5],
        [
            "claude",
            "--print",
            "--verbose",
            "--output-format",
            "stream-json"
        ]
        .map(String::from)
    );
    assert_eq!(
        value_after(&plan.argv, "--add-dir").as_deref(),
        Some("/tmp/sandbox")
    );
    assert_eq!(
        value_after(&plan.argv, "--settings").as_deref(),
        Some("{\"includeCoAuthoredBy\":false}")
    );
    assert_eq!(
        value_after(&plan.argv, "--allowedTools").as_deref(),
        Some("Skill")
    );
    assert!(
        plan.argv
            .contains(&"--setting-sources=user,project".to_string())
    );
    assert_eq!(
        plan.argv[plan.argv.len() - 2..],
        ["--input-format", "stream-json"].map(String::from)
    );
    assert!(!plan.argv.contains(&"--permission-mode".to_string()));
    assert!(!plan.argv.contains(&"--model".to_string()));
    assert!(!plan.argv.contains(&"--max-turns".to_string()));
    assert_eq!(plan.stdin, StdinPlan::Open);
    assert_eq!(plan.cwd, Some(PathBuf::from("/tmp/sandbox")));
    assert!(plan.denied_environment.is_empty());
}

#[test]
fn text_input_mode_matches_the_container_command() {
    let plan = build_cli_plan(&options(&[("input_mode", json!("text"))])).unwrap();
    assert_eq!(
        plan.argv[plan.argv.len() - 2..],
        ["-p", "-"].map(String::from)
    );
    assert!(!plan.argv.contains(&"--input-format".to_string()));
    assert_eq!(plan.stdin, StdinPlan::Payload("do the task".into()));
}

#[test]
fn unknown_input_mode_rejected() {
    let err = build_cli_plan(&options(&[("input_mode", json!("telepathy"))])).unwrap_err();
    assert_eq!(err.code(), "unsupported_capability");
}

#[test]
fn permission_policies_map_to_cli_modes() {
    let auto = argv(&request_with(|c| {
        c.permission_policy = PermissionPolicy::Auto
    }));
    assert_eq!(
        value_after(&auto, "--permission-mode").as_deref(),
        Some("bypassPermissions")
    );
    let plan = argv(&request_with(|c| {
        c.permission_policy = PermissionPolicy::Plan
    }));
    assert_eq!(
        value_after(&plan, "--permission-mode").as_deref(),
        Some("plan")
    );
    let supervised = argv(&request_with(|c| {
        c.permission_policy = PermissionPolicy::Supervised
    }));
    assert!(!supervised.contains(&"--permission-mode".to_string()));
    let err = build_cli_plan(&request_with(|c| {
        c.permission_policy = PermissionPolicy::ReadOnly
    }))
    .unwrap_err();
    assert_eq!(err.code(), "unsupported_capability");
}

#[test]
fn model_max_turns_effort_and_fallback_passthrough() {
    let argv = argv(&request_with(|c| {
        c.model_id = Some("claude-opus-4".into());
        c.max_turns = Some(7);
        c.reasoning_id = Some("high".into());
        c.adapter_options
            .insert("fallback_model".into(), json!("claude-sonnet-4"));
        c.adapter_options
            .insert("max_budget_usd".into(), json!(2.5));
        c.adapter_options
            .insert("max_thinking_tokens".into(), json!(8000));
    }));
    assert_eq!(
        value_after(&argv, "--model").as_deref(),
        Some("claude-opus-4")
    );
    assert_eq!(value_after(&argv, "--max-turns").as_deref(), Some("7"));
    assert_eq!(value_after(&argv, "--effort").as_deref(), Some("high"));
    assert_eq!(
        value_after(&argv, "--fallback-model").as_deref(),
        Some("claude-sonnet-4")
    );
    assert_eq!(
        value_after(&argv, "--max-budget-usd").as_deref(),
        Some("2.5")
    );
    assert_eq!(
        value_after(&argv, "--max-thinking-tokens").as_deref(),
        Some("8000")
    );
}

#[test]
fn suppress_coauthor_opt_out_drops_settings() {
    let argv = argv(&options(&[("suppress_coauthor", json!(false))]));
    assert!(!argv.contains(&"--settings".to_string()));
}

#[test]
fn settings_merge_sandbox_and_hooks() {
    let request = options(&[
        ("settings", json!({"model": "x"})),
        (
            "sandbox_exec",
            json!({"enabled": true, "autoAllowBashIfSandboxed": true}),
        ),
        (
            "hooks",
            json!({"PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo hi"}]}]}),
        ),
    ]);
    let settings: Value = serde_json::from_str(
        &build_settings_value(&request.configuration)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(settings["model"], json!("x"));
    assert_eq!(settings["includeCoAuthoredBy"], json!(false));
    assert_eq!(settings["sandbox"]["enabled"], json!(true));
    assert_eq!(
        settings["hooks"]["PostToolUse"][0]["matcher"],
        json!("Bash")
    );
}

#[test]
fn settings_string_passthrough_and_json_string_merge() {
    let passthrough = options(&[
        ("settings", json!("/etc/claude/settings.json")),
        ("suppress_coauthor", json!(false)),
    ]);
    assert_eq!(
        build_settings_value(&passthrough.configuration)
            .unwrap()
            .as_deref(),
        Some("/etc/claude/settings.json")
    );
    let merged = options(&[("settings", json!("{\"a\": 1}"))]);
    let settings: Value = serde_json::from_str(
        &build_settings_value(&merged.configuration)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(settings, json!({"a": 1, "includeCoAuthoredBy": false}));
    let bad = options(&[("settings", json!(42))]);
    assert_eq!(
        build_settings_value(&bad.configuration).unwrap_err().code(),
        "agent_startup_failed"
    );
    let missing_file = options(&[("settings", json!("/nonexistent/settings.json"))]);
    assert_eq!(
        build_cli_plan(&missing_file).unwrap_err().code(),
        "agent_startup_failed"
    );
}

#[test]
fn skills_list_becomes_rules_and_null_disables() {
    let listed = argv(&options(&[
        ("skills", json!(["alpha", "beta"])),
        ("allowed_tools", json!(["Read"])),
    ]));
    assert_eq!(
        value_after(&listed, "--allowedTools").as_deref(),
        Some("Read,Skill(alpha),Skill(beta)")
    );
    let disabled = argv(&options(&[("skills", Value::Null)]));
    assert!(!disabled.contains(&"--allowedTools".to_string()));
    assert!(!disabled.iter().any(|a| a.starts_with("--setting-sources")));
    let invalid = build_cli_plan(&options(&[("skills", json!(["bad name"]))])).unwrap_err();
    assert_eq!(invalid.code(), "agent_startup_failed");
    let explicit_sources = argv(&options(&[("setting_sources", json!(["project"]))]));
    assert!(explicit_sources.contains(&"--setting-sources=project".to_string()));
}

#[test]
fn tool_lists_mcp_and_agents_passthrough() {
    let servers = json!({"serena": {"type": "stdio", "command": "serena"}});
    let argv = argv(&options(&[
        ("disallowed_tools", json!(["WebSearch"])),
        ("tools", json!(["Bash", "Edit"])),
        ("mcp_servers", servers.clone()),
        ("mcp_strict", json!(true)),
        (
            "agents",
            json!({"reviewer": {"description": "Reviews", "prompt": "You review"}}),
        ),
    ]));
    assert_eq!(
        value_after(&argv, "--disallowedTools").as_deref(),
        Some("WebSearch")
    );
    assert_eq!(value_after(&argv, "--tools").as_deref(), Some("Bash,Edit"));
    let mcp: Value = serde_json::from_str(&value_after(&argv, "--mcp-config").unwrap()).unwrap();
    assert_eq!(mcp, json!({"mcpServers": servers}));
    assert!(argv.contains(&"--strict-mcp-config".to_string()));
    let agents: Value = serde_json::from_str(&value_after(&argv, "--agents").unwrap()).unwrap();
    assert_eq!(agents["reviewer"]["description"], json!("Reviews"));
    let file = self::argv(&options(&[("mcp_servers", json!("/repo/.mcp.json"))]));
    assert_eq!(
        value_after(&file, "--mcp-config").as_deref(),
        Some("/repo/.mcp.json")
    );
}

#[test]
fn mcp_server_names_select_from_the_project_mcp_json() {
    let sandbox = tempfile::tempdir().unwrap();
    let serena = json!({"command": "uvx", "args": ["serena", "start-mcp-server"]});
    let proto = json!({"type": "http", "url": "https://proto.example/mcp"});
    std::fs::write(
        sandbox.path().join(".mcp.json"),
        json!({"mcpServers": {"serena": serena.clone(), "proto": proto, "other": {"command": "x"}}})
            .to_string(),
    )
    .unwrap();
    let request = |names: Value| {
        let mut request = options(&[("mcp_servers", names), ("mcp_strict", json!(true))]);
        request.working_directory = sandbox.path().to_path_buf();
        request
    };
    let argv = argv(&request(json!(["serena"])));
    let mcp: Value = serde_json::from_str(&value_after(&argv, "--mcp-config").unwrap()).unwrap();
    assert_eq!(mcp, json!({"mcpServers": {"serena": serena}}));
    assert!(argv.contains(&"--strict-mcp-config".to_string()));

    let err = build_cli_plan(&request(json!(["serena", "missing"])))
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not declare missing"), "{err}");
    assert!(err.contains("declared: other, proto, serena"), "{err}");

    let bare = tempfile::tempdir().unwrap();
    let mut no_file = options(&[("mcp_servers", json!(["serena"]))]);
    no_file.working_directory = bare.path().to_path_buf();
    let err = build_cli_plan(&no_file).unwrap_err().to_string();
    assert!(err.contains("cannot be read"), "{err}");
}

#[test]
fn session_resume_flags_use_equals_form() {
    let argv = argv(&options(&[
        ("resume", json!("-abc")),
        ("session_id", json!("11111111-2222-3333-4444-555555555555")),
        ("fork_session", json!(true)),
        ("continue_conversation", json!(true)),
    ]));
    assert!(argv.contains(&"--resume=-abc".to_string()));
    assert!(argv.contains(&"--session-id=11111111-2222-3333-4444-555555555555".to_string()));
    assert!(argv.contains(&"--fork-session".to_string()));
    assert!(argv.contains(&"--continue".to_string()));
}

#[test]
fn streaming_flags_prompts_and_extra_args() {
    let argv = argv(&options(&[
        ("include_partial_messages", json!(true)),
        ("include_hook_events", json!(true)),
        ("system_prompt", json!("be terse")),
        ("append_system_prompt", json!("and kind")),
        ("add_dirs", json!(["/extra"])),
        ("extra_cli_args", json!(["--debug", "api"])),
    ]));
    assert!(argv.contains(&"--include-partial-messages".to_string()));
    assert!(argv.contains(&"--include-hook-events".to_string()));
    assert_eq!(
        value_after(&argv, "--system-prompt").as_deref(),
        Some("be terse")
    );
    assert_eq!(
        value_after(&argv, "--append-system-prompt").as_deref(),
        Some("and kind")
    );
    assert_eq!(argv.iter().filter(|a| *a == "--add-dir").count(), 2);
    let debug = argv.iter().position(|a| a == "--debug").unwrap();
    assert_eq!(argv[debug + 1], "api");
    assert!(
        debug < argv.len() - 2,
        "extra args precede the input-format tail"
    );
}

#[test]
fn account_only_denies_api_key_and_environment_passes_through() {
    let plan = build_cli_plan(&request_with(|c| {
        c.authentication_policy = AuthenticationPolicy::AccountOnly;
        c.environment.insert("FOO".into(), "bar".into());
    }))
    .unwrap();
    assert_eq!(
        plan.denied_environment,
        vec!["ANTHROPIC_API_KEY".to_string()]
    );
    assert_eq!(plan.environment.get("FOO").map(String::as_str), Some("bar"));
}

#[test]
fn command_override_replaces_executable() {
    let argv = argv(&request_with(|c| {
        c.command_override = Some(vec!["/usr/bin/env".into(), "fake".into()])
    }));
    assert_eq!(argv[..2], ["/usr/bin/env", "fake"].map(String::from));
    assert_eq!(argv[2], "--print");
}
