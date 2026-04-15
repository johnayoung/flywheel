package main

import (
	"context"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/johnayoung/flywheel/internal/config"
)

func TestPreflightGit_NotARepo(t *testing.T) {
	dir := t.TempDir()
	cfg := config.LoadWithDefaults()
	cfg.Repo = dir

	err := preflightGit(context.Background(), cfg)
	if err == nil {
		t.Fatal("expected error for non-git directory, got nil")
	}
	if !strings.Contains(err.Error(), "is not a git repository") {
		t.Errorf("expected 'is not a git repository' in error, got: %v", err)
	}
}

func TestPreflightGit_RepoWithoutCommits(t *testing.T) {
	dir := t.TempDir()
	run(t, dir, "git", "init", "--initial-branch=main")

	cfg := config.LoadWithDefaults()
	cfg.Repo = dir

	err := preflightGit(context.Background(), cfg)
	if err == nil {
		t.Fatal("expected error for repo with no commits, got nil")
	}
	if !strings.Contains(err.Error(), "does not resolve") {
		t.Errorf("expected 'does not resolve' in error, got: %v", err)
	}
}

func TestPreflightGit_Valid(t *testing.T) {
	dir := t.TempDir()
	run(t, dir, "git", "init", "--initial-branch=main")
	run(t, dir, "git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "--allow-empty", "-m", "init")

	cfg := config.LoadWithDefaults()
	cfg.Repo = dir

	if err := preflightGit(context.Background(), cfg); err != nil {
		t.Fatalf("expected nil for valid git repo, got %v", err)
	}
}

func TestPreflightGit_MissingBaseRef(t *testing.T) {
	dir := t.TempDir()
	run(t, dir, "git", "init", "--initial-branch=main")
	run(t, dir, "git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "--allow-empty", "-m", "init")

	cfg := config.LoadWithDefaults()
	cfg.Repo = dir
	cfg.BaseRef = "develop" // does not exist

	err := preflightGit(context.Background(), cfg)
	if err == nil {
		t.Fatal("expected error for missing base_ref, got nil")
	}
	if !strings.Contains(err.Error(), "does not resolve") {
		t.Errorf("expected 'does not resolve' in error, got: %v", err)
	}
}

func run(t *testing.T, dir string, name string, args ...string) {
	t.Helper()
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("%s %s failed: %v\n%s", name, filepath.Base(name), err, out)
	}
}
