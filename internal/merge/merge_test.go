package merge

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
)

// run executes a command in dir and fails the test on error.
func run(t *testing.T, dir string, name string, args ...string) string {
	t.Helper()
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("command %q %v failed: %v\n%s", name, args, err, out)
	}
	return strings.TrimSpace(string(out))
}

// setupTestRepo creates a git repo with an initial commit on the default branch.
func setupTestRepo(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	run(t, dir, "git", "init", "-b", "main")
	run(t, dir, "git", "config", "user.email", "test@test.com")
	run(t, dir, "git", "config", "user.name", "Test")

	// Initial commit so main exists.
	f := filepath.Join(dir, "README")
	if err := os.WriteFile(f, []byte("init\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, dir, "git", "add", ".")
	run(t, dir, "git", "commit", "-m", "initial commit")
	return dir
}

func TestCleanMerge(t *testing.T) {
	repo := setupTestRepo(t)

	// Create a feature branch with a non-conflicting change.
	run(t, repo, "git", "checkout", "-b", "feature-a")
	if err := os.WriteFile(filepath.Join(repo, "a.txt"), []byte("feature a\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, repo, "git", "add", ".")
	run(t, repo, "git", "commit", "-m", "add a.txt")
	run(t, repo, "git", "checkout", "main")

	// Record the original main SHA for comparison.
	origMainSHA := run(t, repo, "git", "rev-parse", "main")

	ws := filepath.Join(repo, ".flywheel", "merge-workspace")
	lock := filepath.Join(repo, ".flywheel", "merge.lock")
	m := NewSequentialMerger(repo, ws, lock)

	res, err := m.Merge(context.Background(), MergeRequest{
		Branch:    "feature-a",
		TargetRef: "main",
		TaskID:    "task-1",
	})
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !res.Success {
		t.Fatal("expected Success=true")
	}
	if res.MergeCommitSHA == "" {
		t.Fatal("expected non-empty MergeCommitSHA")
	}
	if res.HasConflicts {
		t.Fatal("expected no conflicts")
	}

	// main ref should be updated to the merge commit.
	newMainSHA := run(t, repo, "git", "rev-parse", "main")
	if newMainSHA == origMainSHA {
		t.Fatal("main ref was not updated")
	}
	if newMainSHA != res.MergeCommitSHA {
		t.Fatalf("main ref %s != merge commit %s", newMainSHA, res.MergeCommitSHA)
	}

	// The user's working directory (repo root) should still be on main with
	// the original checkout state (no new files checked out from the merge).
	currentBranch := run(t, repo, "git", "rev-parse", "--abbrev-ref", "HEAD")
	if currentBranch != "main" {
		t.Fatalf("expected repo to remain on main, got %s", currentBranch)
	}
}

func TestConflictDetection(t *testing.T) {
	repo := setupTestRepo(t)

	// Branch A: modify shared file.
	run(t, repo, "git", "checkout", "-b", "branch-a")
	if err := os.WriteFile(filepath.Join(repo, "shared.txt"), []byte("branch a content\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, repo, "git", "add", ".")
	run(t, repo, "git", "commit", "-m", "branch a change")
	run(t, repo, "git", "checkout", "main")

	// Branch B: modify same file differently.
	run(t, repo, "git", "checkout", "-b", "branch-b")
	if err := os.WriteFile(filepath.Join(repo, "shared.txt"), []byte("branch b content\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, repo, "git", "add", ".")
	run(t, repo, "git", "commit", "-m", "branch b change")
	run(t, repo, "git", "checkout", "main")

	ws := filepath.Join(repo, ".flywheel", "merge-workspace")
	lock := filepath.Join(repo, ".flywheel", "merge.lock")
	m := NewSequentialMerger(repo, ws, lock)

	// First merge (branch-a) should succeed.
	res1, err := m.Merge(context.Background(), MergeRequest{
		Branch:    "branch-a",
		TargetRef: "main",
		TaskID:    "task-1",
	})
	if err != nil {
		t.Fatalf("first merge error: %v", err)
	}
	if !res1.Success {
		t.Fatal("first merge should succeed")
	}

	// Second merge (branch-b) should conflict on shared.txt.
	res2, err := m.Merge(context.Background(), MergeRequest{
		Branch:    "branch-b",
		TargetRef: "main",
		TaskID:    "task-2",
	})
	if err != nil {
		t.Fatalf("second merge error: %v", err)
	}
	if res2.Success {
		t.Fatal("second merge should not succeed")
	}
	if !res2.HasConflicts {
		t.Fatal("expected HasConflicts=true")
	}

	found := false
	for _, f := range res2.ConflictFiles {
		if f == "shared.txt" {
			found = true
			break
		}
	}
	if !found {
		t.Fatalf("expected shared.txt in ConflictFiles, got %v", res2.ConflictFiles)
	}

	// Verify the merge workspace is clean (merge was aborted).
	status := run(t, ws, "git", "status", "--porcelain")
	if status != "" {
		t.Fatalf("merge workspace not clean after abort: %s", status)
	}
}

func TestMergeLockPreventsConcurrentMerges(t *testing.T) {
	repo := setupTestRepo(t)

	// Create two non-conflicting branches.
	run(t, repo, "git", "checkout", "-b", "feat-1")
	if err := os.WriteFile(filepath.Join(repo, "one.txt"), []byte("one\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, repo, "git", "add", ".")
	run(t, repo, "git", "commit", "-m", "feat 1")
	run(t, repo, "git", "checkout", "main")

	run(t, repo, "git", "checkout", "-b", "feat-2")
	if err := os.WriteFile(filepath.Join(repo, "two.txt"), []byte("two\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, repo, "git", "add", ".")
	run(t, repo, "git", "commit", "-m", "feat 2")
	run(t, repo, "git", "checkout", "main")

	ws := filepath.Join(repo, ".flywheel", "merge-workspace")
	lock := filepath.Join(repo, ".flywheel", "merge.lock")
	m := NewSequentialMerger(repo, ws, lock)

	var wg sync.WaitGroup
	results := make([]*MergeResult, 2)
	errs := make([]error, 2)

	wg.Add(2)
	go func() {
		defer wg.Done()
		results[0], errs[0] = m.Merge(context.Background(), MergeRequest{
			Branch:    "feat-1",
			TargetRef: "main",
			TaskID:    "t1",
		})
	}()
	go func() {
		defer wg.Done()
		results[1], errs[1] = m.Merge(context.Background(), MergeRequest{
			Branch:    "feat-2",
			TargetRef: "main",
			TaskID:    "t2",
		})
	}()
	wg.Wait()

	for i := 0; i < 2; i++ {
		if errs[i] != nil {
			t.Fatalf("merge %d error: %v", i, errs[i])
		}
		if !results[i].Success {
			t.Fatalf("merge %d should succeed", i)
		}
		if results[i].MergeCommitSHA == "" {
			t.Fatalf("merge %d has empty SHA", i)
		}
	}

	// Both commits should be different.
	if results[0].MergeCommitSHA == results[1].MergeCommitSHA {
		t.Fatal("both merges produced the same SHA -- they should be distinct commits")
	}
}

func TestMergeWorktreeReuse(t *testing.T) {
	repo := setupTestRepo(t)

	// Two branches with non-conflicting changes.
	run(t, repo, "git", "checkout", "-b", "br-1")
	if err := os.WriteFile(filepath.Join(repo, "file1.txt"), []byte("1\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, repo, "git", "add", ".")
	run(t, repo, "git", "commit", "-m", "br-1")
	run(t, repo, "git", "checkout", "main")

	run(t, repo, "git", "checkout", "-b", "br-2")
	if err := os.WriteFile(filepath.Join(repo, "file2.txt"), []byte("2\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, repo, "git", "add", ".")
	run(t, repo, "git", "commit", "-m", "br-2")
	run(t, repo, "git", "checkout", "main")

	ws := filepath.Join(repo, ".flywheel", "merge-workspace")
	lock := filepath.Join(repo, ".flywheel", "merge.lock")
	m := NewSequentialMerger(repo, ws, lock)

	// First merge creates the worktree.
	res1, err := m.Merge(context.Background(), MergeRequest{
		Branch:    "br-1",
		TargetRef: "main",
		TaskID:    "t1",
	})
	if err != nil {
		t.Fatalf("first merge error: %v", err)
	}
	if !res1.Success {
		t.Fatal("first merge should succeed")
	}

	// Confirm worktree directory exists.
	if _, err := os.Stat(ws); err != nil {
		t.Fatalf("workspace should exist after first merge: %v", err)
	}

	// Second merge reuses the same worktree.
	res2, err := m.Merge(context.Background(), MergeRequest{
		Branch:    "br-2",
		TargetRef: "main",
		TaskID:    "t2",
	})
	if err != nil {
		t.Fatalf("second merge error: %v", err)
	}
	if !res2.Success {
		t.Fatal("second merge should succeed")
	}

	// Workspace should still exist (reused, not recreated).
	if _, err := os.Stat(ws); err != nil {
		t.Fatalf("workspace should still exist after second merge: %v", err)
	}

	// Both merges produced distinct commits.
	if res1.MergeCommitSHA == res2.MergeCommitSHA {
		t.Fatal("expected different merge SHAs for the two merges")
	}
}
