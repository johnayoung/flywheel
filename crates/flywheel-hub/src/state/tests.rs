use super::*;
use crate::layout::Tab;
use std::collections::HashMap;
use std::fs;

fn git_repo(root: &Path, name: &str) -> PathBuf {
    let repo = root.join(name);
    fs::create_dir_all(repo.join(".git")).unwrap();
    repo
}

fn input(name: &str, repos: Vec<PathBuf>) -> WorkspaceInput {
    WorkspaceInput {
        name: Some(name.into()),
        repos: Some(repos),
        ..WorkspaceInput::default()
    }
}

fn paths(workspace: &Workspace) -> Vec<PathBuf> {
    workspace.folders.iter().map(|f| f.path.clone()).collect()
}

#[test]
fn templates_are_saved_replaced_removed_and_go_with_their_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(WorkspaceInput {
            name: Some("w".into()),
            folders: Some(vec![FolderSpec::Path(repo.clone())]),
            repos: None,
        })
        .unwrap();
    let template = |name: &str, model: &str| LaunchTemplate {
        name: name.into(),
        kind: SessionKind::Claude,
        prompt: Some("Fix the flaky test.\nThen run it.\n".into()),
        launch: AgentLaunch {
            model: Some(model.into()),
            permission_mode: Some("plan".into()),
            ..AgentLaunch::default()
        },
        worktree: true,
        updated_at: Utc::now(),
    };
    let saved = store
        .save_template(&ws.id, template(" fix ", "opus"), false)
        .unwrap();
    assert_eq!(saved.name, "fix");
    assert_eq!(
        saved.prompt.as_deref(),
        Some("Fix the flaky test.\nThen run it.")
    );
    store
        .save_template(&ws.id, template("Audit", "sonnet"), false)
        .unwrap();
    // Same name: refused unless replacing, then replaced, not added.
    assert!(matches!(
        store.save_template(&ws.id, template("fix ", "haiku"), false),
        Err(StateError::Conflict(_))
    ));
    store
        .save_template(&ws.id, template("fix", "haiku"), true)
        .unwrap();
    let names = |store: &HubStore| -> Vec<(String, Option<String>)> {
        store
            .templates(&ws.id)
            .unwrap()
            .into_iter()
            .map(|t| (t.name, t.launch.model))
            .collect()
    };
    assert_eq!(
        names(&store),
        [
            ("Audit".to_owned(), Some("sonnet".to_owned())),
            ("fix".to_owned(), Some("haiku".to_owned()))
        ]
    );
    // Checked like a launch.
    let mut bad = template("bad", "opus");
    bad.kind = SessionKind::Shell;
    bad.prompt = None;
    bad.launch = AgentLaunch::default();
    assert!(matches!(
        store.save_template(&ws.id, bad, false),
        Err(StateError::Invalid(_))
    ));
    let mut bad = template("bad", "opus");
    bad.launch.approval = Some("never".into());
    assert!(matches!(
        store.save_template(&ws.id, bad, false),
        Err(StateError::Invalid(_))
    ));
    assert!(matches!(
        store.save_template("nope", template("x", "opus"), false),
        Err(StateError::NotFound(_))
    ));
    // Persisted (the current schema), and loaded back.
    drop(store);
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
    assert_eq!(raw["version"], STATE_VERSION);
    assert_eq!(raw["templates"][&ws.id][1]["permission_mode"], "plan");
    let mut store = HubStore::open(dir.path()).unwrap();
    assert_eq!(names(&store).len(), 2);
    store.remove_template(&ws.id, "fix").unwrap();
    assert!(matches!(
        store.remove_template(&ws.id, "fix"),
        Err(StateError::NotFound(_))
    ));
    assert_eq!(names(&store).len(), 1);
    // Deleting the workspace deletes its templates.
    store.delete_workspace(&ws.id).unwrap();
    assert!(store.all_templates().is_empty());
}

fn running_session(workspace_id: &str, cwd: &Path) -> SessionRecord {
    SessionRecord {
        id: new_id(),
        workspace_id: workspace_id.into(),
        name: "shell".into(),
        kind: SessionKind::Shell,
        cwd: cwd.into(),
        command: vec!["/bin/sh".into()],
        agent_session_id: None,
        created_at: Utc::now(),
        state: SessionState::Running,
        fallback_command: None,
        worktree: None,
        companion_of: None,
        launch: None,
        reviewer: None,
        review_of: None,
        account: None,
    }
}

#[test]
fn agent_session_ids_persist_and_are_omitted_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let session = running_session(&ws.id, &repo);
    store.insert_session(session.clone()).unwrap();
    let raw = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
    assert!(!raw.contains("agent_session_id"), "{raw}");
    assert!(store.set_agent_session_id(&session.id, "conv-1").unwrap());
    assert!(!store.set_agent_session_id(&session.id, "conv-1").unwrap());
    let reopened = HubStore::open(dir.path()).unwrap();
    assert_eq!(
        reopened
            .session(&session.id)
            .unwrap()
            .agent_session_id
            .as_deref(),
        Some("conv-1")
    );
    assert!(matches!(
        store.set_agent_session_id("missing", "x"),
        Err(StateError::NotFound(_))
    ));
}

#[test]
fn workspace_crud_round_trips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let a = git_repo(dir.path(), "a");
    let b = git_repo(dir.path(), "b");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("  proj  ", vec![a.clone(), b.clone(), a.clone()]))
        .unwrap();
    assert_eq!(ws.name, "proj");
    assert_eq!(
        paths(&ws),
        vec![a.canonicalize().unwrap(), b.canonicalize().unwrap()]
    );
    let renamed = store
        .update_workspace(
            &ws.id,
            WorkspaceInput {
                name: Some("renamed".into()),
                ..WorkspaceInput::default()
            },
        )
        .unwrap();
    assert_eq!(renamed.folders.len(), 2);

    let reopened = HubStore::open(dir.path()).unwrap();
    assert_eq!(reopened.workspaces(), std::slice::from_ref(&renamed));

    let mut store = reopened;
    store.delete_workspace(&ws.id).unwrap();
    assert!(store.workspaces().is_empty());
    assert!(HubStore::open(dir.path()).unwrap().workspaces().is_empty());
    assert!(matches!(
        store.delete_workspace(&ws.id),
        Err(StateError::NotFound(_))
    ));
}

#[test]
fn rejects_bad_workspace_input() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let file = dir.path().join("file");
    fs::write(&file, "").unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    let invalid = |r: Result<Workspace, StateError>| matches!(r, Err(StateError::Invalid(_)));
    assert!(invalid(
        store.create_workspace(input(" ", vec![repo.clone()]))
    ));
    assert!(invalid(store.create_workspace(input("x", vec![]))));
    assert!(invalid(
        store.create_workspace(input("x", vec![PathBuf::from("rel")]))
    ));
    assert!(invalid(
        store.create_workspace(input("x", vec![dir.path().join("missing")]))
    ));
    assert!(invalid(store.create_workspace(input("x", vec![file]))));
    assert!(invalid(store.create_workspace(WorkspaceInput {
        name: Some("x".into()),
        folders: Some(vec![FolderSpec::Path(repo.clone())]),
        repos: Some(vec![repo.clone()]),
    })));
    assert!(invalid(store.create_workspace(WorkspaceInput {
        name: Some("x".into()),
        folders: Some(vec![FolderSpec::Labeled {
            path: repo.clone(),
            name: Some("a\tb".into()),
        }]),
        repos: None,
    })));
    assert!(invalid(
        store.create_workspace(input("a\nb", vec![repo.clone()]))
    ));
    store
        .create_workspace(input("x", vec![repo.clone()]))
        .unwrap();
    assert!(matches!(
        store.create_workspace(input("x", vec![repo])),
        Err(StateError::Conflict(_))
    ));
}

#[test]
fn linked_worktree_with_git_file_is_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let wt = dir.path().join("wt");
    fs::create_dir(&wt).unwrap();
    fs::write(wt.join(".git"), "gitdir: /elsewhere\n").unwrap();
    assert!(is_git_root(&wt));
    let folders = validate_folders(&[FolderSpec::Path(wt)], &[]).unwrap();
    assert!(folders[0].git);
}

#[test]
fn running_sessions_block_workspace_delete_and_load_as_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let session = running_session(&ws.id, &repo);
    store.insert_session(session.clone()).unwrap();
    match store.delete_workspace(&ws.id) {
        // Named for the operator, not by its id.
        Err(StateError::Conflict(message)) => {
            assert_eq!(
                message,
                "workspace \"w\" has 1 running session; stop it first"
            );
        }
        other => panic!("unexpected {other:?}"),
    }

    let reopened = HubStore::open(dir.path()).unwrap();
    assert!(matches!(
        reopened.session(&session.id).unwrap().state,
        SessionState::Stopped { .. }
    ));
    let mut store = reopened;
    store.delete_workspace(&ws.id).unwrap();
    assert!(store.sessions().is_empty());
}

#[test]
fn repo_changes_may_not_strand_a_running_session() {
    let dir = tempfile::tempdir().unwrap();
    let a = git_repo(dir.path(), "a").canonicalize().unwrap();
    let b = git_repo(dir.path(), "b").canonicalize().unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![a.clone(), b.clone()]))
        .unwrap();
    store.insert_session(running_session(&ws.id, &a)).unwrap();
    let repos = |r: Vec<PathBuf>| WorkspaceInput {
        repos: Some(r),
        ..WorkspaceInput::default()
    };
    assert!(matches!(
        store.update_workspace(&ws.id, repos(vec![b.clone()])),
        Err(StateError::Conflict(_))
    ));
    assert_eq!(store.workspace(&ws.id).unwrap().folders.len(), 2);
    // Dropping a repo no running session uses is fine.
    let updated = store
        .update_workspace(&ws.id, repos(vec![a.clone()]))
        .unwrap();
    assert_eq!(paths(&updated), vec![a]);
}

#[test]
fn plain_directories_are_folders_flagged_not_git_and_rechecked_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "repo").canonicalize().unwrap();
    let plain = dir.path().join("notes");
    fs::create_dir(&plain).unwrap();
    let plain = plain.canonicalize().unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(WorkspaceInput {
            name: Some("life".into()),
            folders: Some(vec![
                FolderSpec::Labeled {
                    path: repo.clone(),
                    name: Some(" Main ".into()),
                },
                FolderSpec::Path(plain.clone()),
            ]),
            repos: None,
        })
        .unwrap();
    assert_eq!(
        ws.folders,
        vec![
            Folder {
                path: repo.clone(),
                name: Some("Main".into()),
                git: true,
                missing: false,
            },
            Folder {
                path: plain.clone(),
                name: None,
                git: false,
                missing: false,
            },
        ]
    );
    // Persisted with a legacy `repos` mirror of every folder.
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
    assert_eq!(
        raw["workspaces"][0]["repos"],
        serde_json::json!([repo, plain])
    );
    // The flag follows the disk on the next load.
    fs::create_dir(plain.join(".git")).unwrap();
    let reopened = HubStore::open(dir.path()).unwrap();
    assert!(reopened.workspaces()[0].folders.iter().all(|f| f.git));
    // An update that names a path without a label keeps its label;
    // an empty label clears it.
    let mut store = reopened;
    let kept = store
        .update_workspace(
            &ws.id,
            WorkspaceInput {
                folders: Some(vec![FolderSpec::Path(repo.clone())]),
                ..WorkspaceInput::default()
            },
        )
        .unwrap();
    assert_eq!(kept.folders[0].name.as_deref(), Some("Main"));
    let cleared = store
        .update_workspace(
            &ws.id,
            WorkspaceInput {
                folders: Some(vec![FolderSpec::Labeled {
                    path: repo.clone(),
                    name: Some(String::new()),
                }]),
                ..WorkspaceInput::default()
            },
        )
        .unwrap();
    assert_eq!(cleared.folders[0].name, None);
}

#[test]
fn a_folder_gone_from_disk_is_flagged_missing_and_may_stay_in_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    let a = git_repo(dir.path(), "a").canonicalize().unwrap();
    let plain = dir.path().join("b");
    fs::create_dir(&plain).unwrap();
    let b = plain.canonicalize().unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![a.clone(), b.clone()]))
        .unwrap();
    assert!(ws.folders.iter().all(|f| !f.missing));
    fs::rename(&b, dir.path().join("b-moved")).unwrap();
    // Not mistaken for a plain folder: flagged on load and on refresh.
    let mut listed = ws.clone();
    listed.refresh_disk();
    assert!(listed.folders[1].missing && !listed.folders[1].git);
    let mut store2 = HubStore::open(dir.path()).unwrap();
    assert!(store2.workspaces()[0].folders[1].missing);
    assert!(!store2.workspaces()[0].folders[0].missing);
    // An edit that keeps it (and changes the rest) is accepted; adding
    // a different missing path is not.
    let kept = store2
        .update_workspace(&ws.id, input("w2", vec![b.clone()]))
        .unwrap();
    assert!(kept.folders[0].missing);
    assert!(matches!(
        store2.update_workspace(&ws.id, input("w2", vec![dir.path().join("nope")])),
        Err(StateError::Invalid(_))
    ));
    drop(store);
}

#[test]
fn a_legacy_repos_state_file_loads_as_folders() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r").canonicalize().unwrap();
    let legacy = serde_json::json!({
        "version": 1,
        "workspaces": [{
            "id": "w1",
            "name": "old",
            "repos": [repo],
            "created_at": "2026-10-01T00:00:00Z"
        }],
        "sessions": []
    });
    fs::write(
        dir.path().join(STATE_FILE),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    let store = HubStore::open(dir.path()).unwrap();
    let ws = &store.workspaces()[0];
    assert_eq!(ws.folders.len(), 1);
    assert_eq!(ws.folders[0].path, repo);
    assert!(ws.folders[0].git, "recomputed on load");
    assert!(ws.has_folder(&repo));
}

#[test]
fn import_creates_or_with_update_replaces_folders() {
    let dir = tempfile::tempdir().unwrap();
    let a = git_repo(dir.path(), "a").canonicalize().unwrap();
    let b = git_repo(dir.path(), "b").canonicalize().unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    let (ws, created) = store
        .import_workspace("proj", vec![FolderSpec::Path(a.clone())], false)
        .unwrap();
    assert!(created);
    assert!(matches!(
        store.import_workspace("proj", vec![FolderSpec::Path(b.clone())], false),
        Err(StateError::Conflict(_))
    ));
    store.insert_session(running_session(&ws.id, &a)).unwrap();
    // Update may not drop the folder a running session is in.
    assert!(matches!(
        store.import_workspace("proj", vec![FolderSpec::Path(b.clone())], true),
        Err(StateError::Conflict(_))
    ));
    let (updated, created) = store
        .import_workspace(
            "proj",
            vec![FolderSpec::Path(a.clone()), FolderSpec::Path(b.clone())],
            true,
        )
        .unwrap();
    assert!(!created);
    assert_eq!(updated.id, ws.id);
    assert_eq!(paths(&updated), vec![a, b]);
}

#[test]
fn a_session_stopped_by_shutdown_stays_stopped_when_its_exit_lands() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let session = running_session(&ws.id, &repo);
    store.insert_session(session.clone()).unwrap();
    store.mark_all_stopped(&HashMap::new()).unwrap();
    store
        .mark_exited(
            &session.id,
            ExitInfo {
                code: None,
                signal: Some("Hangup".into()),
            },
        )
        .unwrap();
    assert!(matches!(
        HubStore::open(dir.path())
            .unwrap()
            .session(&session.id)
            .unwrap()
            .state,
        SessionState::Stopped { .. }
    ));
}

#[test]
fn only_a_clean_shutdown_makes_sessions_restorable() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let crashed = running_session(&ws.id, &repo);
    store.insert_session(crashed.clone()).unwrap();
    drop(store);
    // Found running on load: the hub died without recording it.
    let mut store = HubStore::open(dir.path()).unwrap();
    assert!(!store.session(&crashed.id).unwrap().state.is_restorable());
    let raw = fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
    assert!(!raw.contains("\"clean\""), "{raw}");
    // Says why it was not restored.
    assert!(matches!(
        &store.session(&crashed.id).unwrap().state,
        SessionState::Stopped { restore_error: Some(e), .. } if e == UNCLEAN_STOP
    ));
    assert!(matches!(
        store.mark_restored(&crashed.id, None),
        Err(StateError::Conflict(_))
    ));

    let clean = running_session(&ws.id, &repo);
    store.insert_session(clean.clone()).unwrap();
    let cwds = HashMap::from([(clean.id.clone(), PathBuf::from("/somewhere/deeper"))]);
    store.mark_all_stopped(&cwds).unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    assert!(store.session(&clean.id).unwrap().state.is_restorable());
    assert!(matches!(
        &store.session(&clean.id).unwrap().state,
        SessionState::Stopped { last_cwd: Some(cwd), .. } if cwd == Path::new("/somewhere/deeper")
    ));
    store.set_restore_error(&clean.id, "cwd gone").unwrap();
    let reopened = HubStore::open(dir.path()).unwrap();
    match &reopened.session(&clean.id).unwrap().state {
        SessionState::Stopped {
            clean: true,
            restore_error: Some(message),
            ..
        } => assert_eq!(message, "cwd gone"),
        other => panic!("unexpected {other:?}"),
    }
    let mut store = reopened;
    let restored = store
        .mark_restored(&clean.id, Some(vec!["/bin/bash".into()]))
        .unwrap();
    assert!(restored.state.is_running());
    // What it is stays recorded; what runs in its place is separate.
    assert_eq!(restored.command, vec!["/bin/sh".to_owned()]);
    assert_eq!(restored.kind, SessionKind::Shell);
    assert_eq!(
        restored.fallback_command,
        Some(vec!["/bin/bash".to_owned()])
    );
    // Same place in the session order.
    assert_eq!(store.sessions()[1].id, clean.id);
    assert!(matches!(
        store.set_restore_error(&clean.id, "x"),
        Err(StateError::Conflict(_))
    ));
}

#[test]
fn forgoing_restore_keeps_sessions_stopped_on_later_starts() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let session = running_session(&ws.id, &repo);
    store.insert_session(session.clone()).unwrap();
    store.mark_all_stopped(&HashMap::new()).unwrap();
    store.set_restore_error(&session.id, "cwd gone").unwrap();
    assert_eq!(store.forgo_restore().unwrap(), 1);
    assert_eq!(store.forgo_restore().unwrap(), 0);
    // A later clean shutdown (nothing running) leaves it alone.
    store.mark_all_stopped(&HashMap::new()).unwrap();
    let store = HubStore::open(dir.path()).unwrap();
    assert!(matches!(
        store.session(&session.id).unwrap().state,
        SessionState::Stopped {
            clean: false,
            restore_error: None,
            ..
        }
    ));
}

#[test]
fn mark_running_clears_a_restore_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let mut agent = running_session(&ws.id, &repo);
    agent.kind = SessionKind::Claude;
    agent.command = vec!["claude".into()];
    store.insert_session(agent.clone()).unwrap();
    store.mark_all_stopped(&HashMap::new()).unwrap();
    let restored = store
        .mark_restored(&agent.id, Some(vec!["/bin/sh".into()]))
        .unwrap();
    assert_eq!(restored.kind, SessionKind::Claude);
    assert_eq!(restored.command, vec!["claude".to_owned()]);
    store
        .mark_exited(
            &agent.id,
            ExitInfo {
                code: Some(0),
                signal: None,
            },
        )
        .unwrap();
    let relaunched = store.mark_running(&agent.id).unwrap();
    assert_eq!(relaunched.fallback_command, None);
    assert_eq!(relaunched.kind, SessionKind::Claude);
}

#[test]
fn mark_running_relaunches_an_ended_record_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let session = running_session(&ws.id, &repo);
    store.insert_session(session.clone()).unwrap();
    assert!(matches!(
        store.mark_running(&session.id),
        Err(StateError::Conflict(_))
    ));
    store
        .mark_exited(
            &session.id,
            ExitInfo {
                code: Some(1),
                signal: None,
            },
        )
        .unwrap();
    let record = store.mark_running(&session.id).unwrap();
    assert_eq!(record.state, SessionState::Running);
    assert_eq!(record.name, session.name);
    assert_eq!(store.sessions().len(), 1);
    assert!(matches!(
        store.mark_running("missing"),
        Err(StateError::NotFound(_))
    ));
}

#[test]
fn session_exit_and_removal_persist() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let session = running_session(&ws.id, &repo);
    store.insert_session(session.clone()).unwrap();
    store
        .mark_exited(
            &session.id,
            ExitInfo {
                code: Some(3),
                signal: None,
            },
        )
        .unwrap();
    let reopened = HubStore::open(dir.path()).unwrap();
    match &reopened.session(&session.id).unwrap().state {
        SessionState::Exited { exit, .. } => assert_eq!(exit.code, Some(3)),
        other => panic!("unexpected {other:?}"),
    }
    let mut store = reopened;
    store.remove_session(&session.id).unwrap();
    assert!(HubStore::open(dir.path()).unwrap().sessions().is_empty());
    // Unknown workspace is rejected.
    let mut orphan = running_session("nope", &repo);
    orphan.workspace_id = "nope".into();
    assert!(matches!(
        store.insert_session(orphan),
        Err(StateError::NotFound(_))
    ));
}

#[test]
fn state_file_is_private_and_versioned() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    store.create_workspace(input("w", vec![repo])).unwrap();
    let path = dir.path().join(STATE_FILE);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    fs::write(&path, r#"{"version": 99}"#).unwrap();
    assert!(matches!(
        HubStore::open(dir.path()),
        Err(StateError::Persist { .. })
    ));
    fs::write(&path, "not json").unwrap();
    assert!(matches!(
        HubStore::open(dir.path()),
        Err(StateError::Persist { .. })
    ));
}

#[test]
fn sessions_rename_persistently_and_reject_bad_names() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let session = running_session(&ws.id, &repo);
    store.insert_session(session.clone()).unwrap();

    let renamed = store.rename_session(&session.id, "  build  ").unwrap();
    assert_eq!(renamed.name, "build");
    assert_eq!(
        HubStore::open(dir.path())
            .unwrap()
            .session(&session.id)
            .unwrap()
            .name,
        "build"
    );
    let mut store = HubStore::open(dir.path()).unwrap();
    assert!(matches!(
        store.rename_session(&session.id, " "),
        Err(StateError::Invalid(_))
    ));
    assert!(matches!(
        store.rename_session(&session.id, "a\tb"),
        Err(StateError::Invalid(_))
    ));
    assert!(matches!(
        store.rename_session("missing", "x"),
        Err(StateError::NotFound(_))
    ));
    assert_eq!(store.session(&session.id).unwrap().name, "build");
}

#[test]
fn records_without_a_kind_load_as_shells_and_kinds_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let legacy = format!(
        r#"{{"version":1,"workspaces":[{{"id":"w1","name":"w","repos":[{repo:?}],
            "created_at":"2026-11-01T00:00:00Z"}}],"sessions":[{{"id":"s1","workspace_id":"w1",
            "name":"old","cwd":{repo:?},"command":["/bin/sh"],
            "created_at":"2026-11-01T00:00:00Z","state":"exited","code":0,"signal":null,
            "at":"2026-11-01T00:00:01Z"}}]}}"#
    );
    fs::write(dir.path().join(STATE_FILE), legacy).unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    assert_eq!(store.session("s1").unwrap().kind, SessionKind::Shell);

    let mut agent = running_session("w1", &repo);
    agent.kind = SessionKind::Codex;
    store.insert_session(agent.clone()).unwrap();
    let reopened = HubStore::open(dir.path()).unwrap();
    assert_eq!(
        reopened.session(&agent.id).unwrap().kind,
        SessionKind::Codex
    );
}

#[test]
fn tilde_expands_to_home_only_as_a_leading_component() {
    let home = Path::new("/home/me");
    assert_eq!(
        expand_home(Path::new("~/code/x"), Some(home)),
        PathBuf::from("/home/me/code/x")
    );
    assert_eq!(
        expand_home(Path::new("~"), Some(home)),
        PathBuf::from("/home/me")
    );
    assert_eq!(
        expand_home(Path::new("~other/x"), Some(home)),
        PathBuf::from("~other/x")
    );
    assert_eq!(
        expand_home(Path::new("/a/~/b"), Some(home)),
        PathBuf::from("/a/~/b")
    );
    assert_eq!(expand_home(Path::new("~/x"), None), PathBuf::from("~/x"));
}

#[test]
fn validate_folder_explains_each_failure() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let file = dir.path().join("file");
    fs::write(&file, "").unwrap();
    let plain = dir.path().join("plain");
    fs::create_dir(&plain).unwrap();
    assert_eq!(
        validate_folder(&repo).unwrap(),
        repo.canonicalize().unwrap()
    );
    // Plain directories are folders too.
    assert_eq!(
        validate_folder(&plain).unwrap(),
        plain.canonicalize().unwrap()
    );
    assert!(!is_git_root(&plain));
    let message = |p: &Path| validate_folder(p).unwrap_err().to_string();
    assert!(message(Path::new("rel/x")).contains("must be absolute"));
    assert!(message(&dir.path().join("missing")).ends_with("missing does not exist"));
    assert!(message(&file).contains("not a directory"));
}

fn pane(id: &str, session: Option<&str>) -> crate::layout::Node {
    crate::layout::Node::Pane {
        id: id.into(),
        session: session.map(Into::into),
    }
}

fn two_pane_tab(id: &str, name: &str, a: &str, b: &str) -> Tab {
    Tab {
        id: id.into(),
        name: name.into(),
        root: crate::layout::Node::Split {
            dir: crate::layout::SplitDir::Right,
            ratio: 0.6,
            a: Box::new(pane(&format!("{id}a"), Some(a))),
            b: Box::new(pane(&format!("{id}b"), Some(b))),
        },
    }
}

#[test]
fn layouts_persist_per_workspace_with_revisions() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let other = store
        .create_workspace(input("o", vec![repo.clone()]))
        .unwrap();
    let s1 = running_session(&ws.id, &repo);
    let s2 = running_session(&ws.id, &repo);
    let foreign = running_session(&other.id, &repo);
    for s in [&s1, &s2, &foreign] {
        store.insert_session(s.clone()).unwrap();
    }
    assert_eq!(store.layout(&ws.id).unwrap(), Layout::default());
    assert!(matches!(store.layout("nope"), Err(StateError::NotFound(_))));

    let saved = store
        .set_layout(
            &ws.id,
            0,
            vec![two_pane_tab("t1", "agents", &s1.id, &s2.id)],
        )
        .unwrap();
    assert_eq!(saved.rev, 1);
    // Written against an old revision: refused, nothing changes.
    assert!(matches!(
        store.set_layout(&ws.id, 0, Vec::new()),
        Err(StateError::Conflict(_))
    ));
    // Another workspace's session is not this layout's to show.
    assert!(matches!(
        store.set_layout(
            &ws.id,
            1,
            vec![two_pane_tab("t1", "a", &s1.id, &foreign.id)]
        ),
        Err(StateError::Invalid(_))
    ));
    assert_eq!(store.layout(&ws.id).unwrap(), saved);
    drop(store);

    // Survives a restart (running sessions load as stopped; layouts
    // keep naming them, so a restore re-attaches in place).
    let mut store = HubStore::open(dir.path()).unwrap();
    assert_eq!(store.layout(&ws.id).unwrap(), saved);
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
    assert_eq!(raw["version"], STATE_VERSION);
    assert_eq!(raw["layouts"][&ws.id]["rev"], 1);

    // Removing a session closes its pane and bumps the revision.
    assert_eq!(
        store.remove_session(&s2.id).unwrap(),
        Some((ws.id.clone(), 2))
    );
    let layout = store.layout(&ws.id).unwrap();
    assert_eq!(layout.tabs[0].root, pane("t1a", Some(&s1.id)));
    // A session no pane shows leaves the layout alone.
    assert_eq!(store.remove_session(&foreign.id).unwrap(), None);
    // Deleting the workspace drops its layout.
    store
        .mark_exited(
            &s1.id,
            ExitInfo {
                code: Some(0),
                signal: None,
            },
        )
        .unwrap();
    store.delete_workspace(&ws.id).unwrap();
    drop(store);
    let raw: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join(STATE_FILE)).unwrap()).unwrap();
    assert!(raw["layouts"].get(&ws.id).is_none(), "{raw}");
}

#[test]
fn version_1_files_migrate_and_stale_layout_entries_are_pruned_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r").canonicalize().unwrap();
    let path = dir.path().join(STATE_FILE);
    fs::write(
        &path,
        format!(
            r#"{{"version":1,"workspaces":[{{"id":"w1","name":"w","repos":[{repo:?}],
                "created_at":"2026-01-01T00:00:00Z"}}],"sessions":[]}}"#
        ),
    )
    .unwrap();
    let store = HubStore::open(dir.path()).unwrap();
    assert_eq!(store.layout("w1").unwrap(), Layout::default());
    drop(store);
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        raw["version"], STATE_VERSION,
        "rewritten as the current version"
    );

    // A layout naming a gone session or workspace (hand edit, crash).
    raw["layouts"] = serde_json::json!({
        "w1": {"rev": 4, "tabs": [{"id": "t1", "name": "a", "root":
            {"type": "split", "dir": "down", "ratio": 0.5,
             "a": {"type": "pane", "id": "p1", "session": "gone"},
             "b": {"type": "pane", "id": "p2", "session": null}}}]},
        "w-gone": {"rev": 1, "tabs": []}
    });
    fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
    let store = HubStore::open(dir.path()).unwrap();
    let layout = store.layout("w1").unwrap();
    assert_eq!(layout.rev, 5);
    assert_eq!(layout.tabs[0].root, pane("p2", None));
    drop(store);
    let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(raw["layouts"].get("w-gone").is_none(), "{raw}");
}

#[test]
fn a_version_4_file_with_promoted_fanouts_migrates_to_queues_losing_neither() {
    use crate::fanout::{Fanout, GraderSpec, Promotion, TaskOrigin};
    use crate::queue::{ItemState, QueueItem, When};
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r").canonicalize().unwrap();
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let mut session = running_session(&ws.id, &repo);
    session.kind = SessionKind::Claude;
    session.command = vec!["claude".into()];
    store.insert_session(session.clone()).unwrap();
    let origin: TaskOrigin = serde_json::from_value(serde_json::json!({
        "folder": repo,
        "task_id": "fix-retry",
        "phase": "01-retry",
        "goal": "retries back off",
        "digest": "d".repeat(64),
        "held_out": 1,
        "not_run": ["rubric"],
        "tags": ["retry"],
        "prompt_seeded": true,
    }))
    .unwrap();
    let promotion = Promotion {
        task_id: "fix-retry".into(),
        phase: "01-retry".into(),
        path: repo.join(".flywheel/tasks/active/01-retry/fix-retry.json"),
        reference: Some("c".repeat(40)),
        at: Utc::now(),
    };
    let fanout = Fanout {
        id: "f1".into(),
        name: "retry".into(),
        workspace_id: ws.id.clone(),
        repo: repo.clone(),
        base: "main".into(),
        base_sha: "b".repeat(40),
        prompt: "p".into(),
        commit_instruction: true,
        graders: vec![
            GraderSpec {
                run: "cargo test".into(),
                name: Some("unit".into()),
                held_out: false,
            },
            GraderSpec {
                run: "./held-out.sh".into(),
                name: Some("held".into()),
                held_out: true,
            },
        ],
        timeout_secs: 60,
        protect: vec![],
        pins: vec![],
        created_at: Utc::now(),
        candidates: vec![],
        winner: None,
        origin: Some(origin),
        promoted: vec![promotion],
        seen_graded_at: None,
    };
    store.insert_fanout(fanout.clone()).unwrap();
    drop(store);

    // Rewrite it as a schema-4 hub (fan-outs and their promotion fields,
    // no queues) left it.
    let path = dir.path().join(STATE_FILE);
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    raw["version"] = 4.into();
    assert!(raw.get("queues").is_none());
    assert_eq!(raw["fanouts"][0]["promoted"][0]["task_id"], "fix-retry");
    assert_eq!(raw["fanouts"][0]["graders"][1]["held_out"], true);
    fs::write(&path, raw.to_string()).unwrap();

    let mut store = HubStore::open(dir.path()).unwrap();
    let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["version"], STATE_VERSION);
    const _: () = assert!(STATE_VERSION == 5, "queues arrived in schema 5");
    assert_eq!(store.fanout("f1").unwrap(), &fanout);
    let now = Utc::now();
    store
        .update_queue(&session.id, |q| {
            q.items.push(QueueItem {
                id: "item-1".into(),
                text: "then write the changelog".into(),
                when: When::Done,
                added_at: now,
                blocked_at: None,
                state: ItemState::Pending,
            });
        })
        .unwrap();
    drop(store);

    let store = HubStore::open(dir.path()).unwrap();
    assert_eq!(store.fanout("f1").unwrap(), &fanout);
    let queue = store.queue(&session.id).unwrap();
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.items[0].text, "then write the changelog");
}

#[test]
fn a_queue_edit_that_records_what_happened_survives_a_failed_save() {
    use crate::queue::{ItemState, QueueItem, When};
    let dir = tempfile::tempdir().unwrap();
    let repo = git_repo(dir.path(), "r");
    let mut store = HubStore::open(dir.path()).unwrap();
    let ws = store
        .create_workspace(input("w", vec![repo.clone()]))
        .unwrap();
    let session = running_session(&ws.id, &repo);
    store.insert_session(session.clone()).unwrap();
    let sending_at = Utc::now();
    store
        .update_queue(&session.id, |q| {
            q.items.push(QueueItem {
                id: "item-1".into(),
                text: "go".into(),
                when: When::Done,
                added_at: sending_at,
                blocked_at: None,
                state: ItemState::Sending { sending_at },
            });
        })
        .unwrap();
    // Every save fails from here: its temp file cannot be created (the
    // directory is read-only; root ignores that, so the test checks).
    let writable = |on: bool| {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            dir.path(),
            fs::Permissions::from_mode(if on { 0o700 } else { 0o500 }),
        )
        .unwrap();
    };
    writable(false);
    if fs::write(dir.path().join("probe"), "x").is_ok() {
        writable(true);
        return;
    }
    let delivered = |q: &mut crate::queue::SessionQueue| {
        q.items[0].state = ItemState::Delivered {
            delivered_at: sending_at,
            reconciled: false,
        };
    };
    // The ordinary update rolls back (still `sending`) ...
    assert!(store.update_queue(&session.id, delivered).is_err());
    assert!(matches!(
        store.queue(&session.id).unwrap().items[0].state,
        ItemState::Sending { .. }
    ));
    // ... the keeping one reports the failure but is not stuck there.
    assert!(store.update_queue_keeping(&session.id, delivered).is_err());
    assert!(store.queue(&session.id).unwrap().items[0].is_delivered());
    // The next save that works writes it.
    writable(true);
    store
        .update_queue(&session.id, |q| q.paused = true)
        .unwrap();
    let reopened = HubStore::open(dir.path()).unwrap();
    assert!(reopened.queue(&session.id).unwrap().items[0].is_delivered());
}
