package config

import (
	"os"
	"path/filepath"
	"testing"
)

func TestLoadWithDefaults(t *testing.T) {
	cfg := LoadWithDefaults()

	checks := []struct {
		name string
		got  any
		want any
	}{
		{"Version", cfg.Version, "1"},
		{"Repo", cfg.Repo, "."},
		{"BaseRef", cfg.BaseRef, "main"},
		{"BranchPrefix", cfg.BranchPrefix, "flywheel/"},
		{"MaxParallel", cfg.MaxParallel, 3},
		{"Store.Backend", cfg.Store.Backend, "jsonl"},
		{"MergeStrategy", cfg.MergeStrategy, "sequential"},
		{"Review", cfg.Review, "agent"},
		{"Agent", cfg.Agent, "claude-code"},
		{"Timeout", cfg.Timeout, "30m"},
		{"MaxRetries", cfg.MaxRetries, 2},
		{"MaxResolveAttempts", cfg.MaxResolveAttempts, 2},
	}

	for _, c := range checks {
		if c.got != c.want {
			t.Errorf("%s = %v, want %v", c.name, c.got, c.want)
		}
	}
}

func TestLoadFromFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "flywheel.json")

	data := `{
		"version": "2",
		"repo": "/tmp/myrepo",
		"base_ref": "develop",
		"max_parallel": 5,
		"timeout": "1h"
	}`
	if err := os.WriteFile(path, []byte(data), 0644); err != nil {
		t.Fatal(err)
	}

	cfg, err := Load(path)
	if err != nil {
		t.Fatalf("Load returned error: %v", err)
	}

	if cfg.Version != "2" {
		t.Errorf("Version = %q, want %q", cfg.Version, "2")
	}
	if cfg.Repo != "/tmp/myrepo" {
		t.Errorf("Repo = %q, want %q", cfg.Repo, "/tmp/myrepo")
	}
	if cfg.BaseRef != "develop" {
		t.Errorf("BaseRef = %q, want %q", cfg.BaseRef, "develop")
	}
	if cfg.MaxParallel != 5 {
		t.Errorf("MaxParallel = %d, want %d", cfg.MaxParallel, 5)
	}
	if cfg.Timeout != "1h" {
		t.Errorf("Timeout = %q, want %q", cfg.Timeout, "1h")
	}
}

func TestLoadAppliesDefaults(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "flywheel.json")

	// Minimal JSON -- everything else should come from defaults.
	data := `{"repo": "myrepo"}`
	if err := os.WriteFile(path, []byte(data), 0644); err != nil {
		t.Fatal(err)
	}

	cfg, err := Load(path)
	if err != nil {
		t.Fatalf("Load returned error: %v", err)
	}

	if cfg.Repo != "myrepo" {
		t.Errorf("Repo = %q, want %q", cfg.Repo, "myrepo")
	}
	// Fields not in the JSON should retain defaults.
	if cfg.BranchPrefix != "flywheel/" {
		t.Errorf("BranchPrefix = %q, want default %q", cfg.BranchPrefix, "flywheel/")
	}
	if cfg.Store.Backend != "jsonl" {
		t.Errorf("Store.Backend = %q, want default %q", cfg.Store.Backend, "jsonl")
	}
	if cfg.Review != "agent" {
		t.Errorf("Review = %q, want default %q", cfg.Review, "agent")
	}
}

func TestLoadMissingFile(t *testing.T) {
	_, err := Load("/nonexistent/path/flywheel.json")
	if err == nil {
		t.Fatal("expected error for missing file, got nil")
	}
}

func TestLoadInvalidJSON(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "bad.json")
	if err := os.WriteFile(path, []byte(`{not json`), 0644); err != nil {
		t.Fatal(err)
	}
	_, err := Load(path)
	if err == nil {
		t.Fatal("expected error for invalid JSON, got nil")
	}
}

func TestValidateAcceptsValid(t *testing.T) {
	cfg := LoadWithDefaults()
	if err := Validate(cfg); err != nil {
		t.Fatalf("Validate rejected valid config: %v", err)
	}
}

func TestValidateRejectsEmptyRepo(t *testing.T) {
	cfg := LoadWithDefaults()
	cfg.Repo = ""
	if err := Validate(cfg); err == nil {
		t.Fatal("expected error for empty repo")
	}
}

func TestValidateRejectsInvalidMergeStrategy(t *testing.T) {
	cfg := LoadWithDefaults()
	cfg.MergeStrategy = "parallel"
	err := Validate(cfg)
	if err == nil {
		t.Fatal("expected error for unsupported merge strategy")
	}
	if want := `merge strategy "parallel" not yet implemented`; err.Error() != want {
		t.Errorf("error = %q, want %q", err.Error(), want)
	}
}

func TestValidateRejectsInvalidStoreBackend(t *testing.T) {
	cfg := LoadWithDefaults()
	cfg.Store.Backend = "sqlite"
	err := Validate(cfg)
	if err == nil {
		t.Fatal("expected error for unsupported store backend")
	}
	if want := `store backend "sqlite" not yet implemented`; err.Error() != want {
		t.Errorf("error = %q, want %q", err.Error(), want)
	}
}

func TestValidateRejectsInvalidReview(t *testing.T) {
	cfg := LoadWithDefaults()
	cfg.Review = "auto"
	err := Validate(cfg)
	if err == nil {
		t.Fatal("expected error for invalid review mode")
	}
}

func TestValidateRejectsMaxParallelLessThanOne(t *testing.T) {
	cfg := LoadWithDefaults()
	cfg.MaxParallel = 0
	if err := Validate(cfg); err == nil {
		t.Fatal("expected error for max_parallel < 1")
	}
}

func TestValidateRejectsInvalidTimeout(t *testing.T) {
	cfg := LoadWithDefaults()
	cfg.Timeout = "notaduration"
	if err := Validate(cfg); err == nil {
		t.Fatal("expected error for invalid timeout")
	}
}

func TestValidateRejectsNegativeMaxRetries(t *testing.T) {
	cfg := LoadWithDefaults()
	cfg.MaxRetries = -1
	if err := Validate(cfg); err == nil {
		t.Fatal("expected error for negative max_retries")
	}
}

func TestValidateRejectsNegativeMaxResolveAttempts(t *testing.T) {
	cfg := LoadWithDefaults()
	cfg.MaxResolveAttempts = -1
	if err := Validate(cfg); err == nil {
		t.Fatal("expected error for negative max_resolve_attempts")
	}
}

func TestReviewModeForTaskOverride(t *testing.T) {
	cfg := *LoadWithDefaults() // Review defaults to "agent"
	got := ReviewModeForTask(cfg, "human")
	if got != "human" {
		t.Errorf("ReviewModeForTask with override = %q, want %q", got, "human")
	}
}

func TestReviewModeForTaskDefault(t *testing.T) {
	cfg := *LoadWithDefaults()
	got := ReviewModeForTask(cfg, "")
	if got != "agent" {
		t.Errorf("ReviewModeForTask without override = %q, want %q", got, "agent")
	}
}
