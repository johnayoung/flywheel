package merge

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/gofrs/flock"
)

// SequentialMerger performs merges one at a time using a file lock and a
// dedicated git worktree so that the caller's working directory is never
// disturbed.
type SequentialMerger struct {
	repoRoot       string
	mergeWorkspace string // e.g., ".flywheel/merge-workspace"
	lockPath       string // e.g., ".flywheel/merge.lock"
}

// NewSequentialMerger returns a SequentialMerger configured for the given repo.
func NewSequentialMerger(repoRoot, mergeWorkspace, lockPath string) *SequentialMerger {
	return &SequentialMerger{
		repoRoot:       repoRoot,
		mergeWorkspace: mergeWorkspace,
		lockPath:       lockPath,
	}
}

// Merge performs a merge of req.Branch into req.TargetRef inside a dedicated
// worktree, serialised by a file lock.
func (s *SequentialMerger) Merge(ctx context.Context, req MergeRequest) (*MergeResult, error) {
	// Ensure parent directory of lock file exists.
	if err := os.MkdirAll(filepath.Dir(s.lockPath), 0o755); err != nil {
		return nil, fmt.Errorf("creating lock dir: %w", err)
	}

	// 1. Acquire file lock.
	fl := flock.New(s.lockPath)
	locked, err := fl.TryLockContext(ctx, 100*time.Millisecond)
	if err != nil {
		return nil, fmt.Errorf("acquiring merge lock: %w", err)
	}
	if !locked {
		return nil, fmt.Errorf("failed to acquire merge lock")
	}
	defer fl.Unlock()

	// 2. Set up merge worktree.
	if err := s.setupWorktree(ctx, req.TargetRef); err != nil {
		return nil, fmt.Errorf("setting up merge worktree: %w", err)
	}

	// 3. Attempt merge.
	msg := fmt.Sprintf("Merge %s", req.Branch)
	mergeOut, mergeErr := s.git(ctx, s.mergeWorkspace, "merge", req.Branch, "--no-ff", "-m", msg)
	if mergeErr == nil {
		// 4. Merge succeeded.
		sha, err := s.git(ctx, s.mergeWorkspace, "rev-parse", "HEAD")
		if err != nil {
			return nil, fmt.Errorf("getting merge commit SHA: %w", err)
		}
		sha = strings.TrimSpace(sha)

		if _, err := s.git(ctx, s.repoRoot, "update-ref", "refs/heads/"+req.TargetRef, sha); err != nil {
			return nil, fmt.Errorf("updating target ref: %w", err)
		}

		return &MergeResult{
			Success:        true,
			MergeCommitSHA: sha,
		}, nil
	}

	// 5. Merge failed -- check for conflicts.
	conflictOut, err := s.git(ctx, s.mergeWorkspace, "diff", "--name-only", "--diff-filter=U")
	if err != nil {
		// diff failed, return original merge error.
		return nil, fmt.Errorf("merge failed: %s", mergeOut)
	}

	conflictFiles := parseLines(conflictOut)

	// Abort the failed merge to leave the worktree clean.
	if _, err := s.git(ctx, s.mergeWorkspace, "merge", "--abort"); err != nil {
		return nil, fmt.Errorf("aborting merge: %w", err)
	}

	if len(conflictFiles) > 0 {
		return &MergeResult{
			HasConflicts:  true,
			ConflictFiles: conflictFiles,
		}, nil
	}

	return nil, fmt.Errorf("merge failed: %s", mergeOut)
}

// setupWorktree ensures the merge workspace exists and is checked out at the
// given target ref.
func (s *SequentialMerger) setupWorktree(ctx context.Context, targetRef string) error {
	if _, err := os.Stat(s.mergeWorkspace); os.IsNotExist(err) {
		if err := os.MkdirAll(filepath.Dir(s.mergeWorkspace), 0o755); err != nil {
			return fmt.Errorf("creating workspace parent dir: %w", err)
		}
		// Use --detach to avoid "already checked out" errors when the target
		// ref is currently checked out in the main repo.
		if _, err := s.git(ctx, s.repoRoot, "worktree", "add", "--detach", s.mergeWorkspace, targetRef); err != nil {
			return fmt.Errorf("creating worktree: %w", err)
		}
		return nil
	}

	// Reset the existing worktree to the current state of the target ref.
	if _, err := s.git(ctx, s.mergeWorkspace, "checkout", "--detach", targetRef); err != nil {
		return fmt.Errorf("checking out target ref: %w", err)
	}
	if _, err := s.git(ctx, s.mergeWorkspace, "reset", "--hard", targetRef); err != nil {
		return fmt.Errorf("resetting to target ref: %w", err)
	}
	return nil
}

// git runs a git command in the given directory, returning combined
// stdout and any error. Stderr is captured for error messages.
func (s *SequentialMerger) git(ctx context.Context, dir string, args ...string) (string, error) {
	cmd := exec.CommandContext(ctx, "git", append([]string{"-C", dir}, args...)...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return stderr.String(), fmt.Errorf("git %s: %s: %w", args[0], stderr.String(), err)
	}
	return stdout.String(), nil
}

// parseLines splits output into non-empty trimmed lines.
func parseLines(s string) []string {
	var lines []string
	for _, line := range strings.Split(s, "\n") {
		line = strings.TrimSpace(line)
		if line != "" {
			lines = append(lines, line)
		}
	}
	return lines
}
