package main

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/johnayoung/flywheel/internal/config"
)

// preflightGit verifies the environmental preconditions flywheel needs before
// it will attempt any engine work: the configured repo must be a git
// repository, and the configured base_ref must resolve to a commit. Config
// validation can't catch these because they're runtime state, not config
// shape. Running these checks from both init and run turns opaque failures
// (fatal: not a git repository, buried in a wrapped error) into a single
// actionable line.
func preflightGit(ctx context.Context, cfg *config.Config) error {
	gitDir := filepath.Join(cfg.Repo, ".git")
	if _, err := os.Stat(gitDir); err != nil {
		if os.IsNotExist(err) {
			return fmt.Errorf("%s is not a git repository; run `git init` there first (and commit at least once so %q exists)", cfg.Repo, cfg.BaseRef)
		}
		return fmt.Errorf("checking git repository at %s: %w", cfg.Repo, err)
	}

	cmd := exec.CommandContext(ctx, "git", "-C", cfg.Repo, "rev-parse", "--verify", cfg.BaseRef+"^{commit}")
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("base_ref %q does not resolve in %s: %s (create it with `git commit` on a fresh repo, or set base_ref in flywheel.json)", cfg.BaseRef, cfg.Repo, trimTrailingNewline(out))
	}

	return nil
}

func trimTrailingNewline(b []byte) string {
	s := string(b)
	for len(s) > 0 && (s[len(s)-1] == '\n' || s[len(s)-1] == '\r') {
		s = s[:len(s)-1]
	}
	return s
}
