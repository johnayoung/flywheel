package worktree

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// setupTestRepo creates a real git repo in a temp dir with one commit.
func setupTestRepo(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()

	run := func(args ...string) {
		t.Helper()
		cmd := exec.Command(args[0], args[1:]...)
		cmd.Dir = dir
		out, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("%s failed: %v: %s", strings.Join(args, " "), err, out)
		}
	}

	run("git", "init", "-b", "main")
	run("git", "config", "user.name", "test")
	run("git", "config", "user.email", "test@test.com")

	if err := os.WriteFile(filepath.Join(dir, "README"), []byte("init"), 0644); err != nil {
		t.Fatal(err)
	}
	run("git", "add", ".")
	run("git", "commit", "-m", "initial commit")

	return dir
}

func TestCreateWorktree(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "task-1", "HEAD")
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	// Directory must exist.
	if _, err := os.Stat(wt.Path); os.IsNotExist(err) {
		t.Fatal("worktree directory does not exist")
	}

	// Branch must exist.
	cmd := exec.Command("git", "branch", "--list", wt.Branch)
	cmd.Dir = repo
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git branch --list: %v: %s", err, out)
	}
	if strings.TrimSpace(string(out)) == "" {
		t.Fatalf("branch %s not found", wt.Branch)
	}

	// BaseSHA must be non-empty.
	if wt.BaseSHA == "" {
		t.Fatal("BaseSHA is empty")
	}
}

func TestCreateTwoWorktrees(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	wt1, err := mgr.Create(ctx, "task-a", "HEAD")
	if err != nil {
		t.Fatalf("Create task-a: %v", err)
	}
	wt2, err := mgr.Create(ctx, "task-b", "HEAD")
	if err != nil {
		t.Fatalf("Create task-b: %v", err)
	}

	if wt1.Path == wt2.Path {
		t.Fatal("worktrees have same path")
	}
	if wt1.Branch == wt2.Branch {
		t.Fatal("worktrees have same branch")
	}
}

func TestRemoveWorktree(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "task-rm", "HEAD")
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	if err := mgr.Remove(ctx, "task-rm", ""); err != nil {
		t.Fatalf("Remove: %v", err)
	}

	// Directory must be gone.
	if _, err := os.Stat(wt.Path); !os.IsNotExist(err) {
		t.Fatal("worktree directory still exists after Remove")
	}

	// Branch must be gone.
	cmd := exec.Command("git", "branch", "--list", wt.Branch)
	cmd.Dir = repo
	out, _ := cmd.CombinedOutput()
	if strings.TrimSpace(string(out)) != "" {
		t.Fatalf("branch %s still exists after Remove", wt.Branch)
	}
}

func TestRemoveWorktree_ArchivesBranchWithCommits(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "task-keep", "HEAD")
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	// Commit something on the branch so it's ahead of baseRef.
	if err := os.WriteFile(filepath.Join(wt.Path, "new.txt"), []byte("work"), 0644); err != nil {
		t.Fatal(err)
	}
	for _, args := range [][]string{
		{"git", "-C", wt.Path, "add", "."},
		{"git", "-C", wt.Path, "commit", "-m", "add work"},
	} {
		cmd := exec.Command(args[0], args[1:]...)
		out, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("%v: %v: %s", args, err, out)
		}
	}

	if err := mgr.Remove(ctx, "task-keep", "HEAD"); err != nil {
		t.Fatalf("Remove: %v", err)
	}

	// Original branch gone.
	cmd := exec.Command("git", "branch", "--list", wt.Branch)
	cmd.Dir = repo
	out, _ := cmd.CombinedOutput()
	if strings.TrimSpace(string(out)) != "" {
		t.Fatalf("original branch %s still exists", wt.Branch)
	}

	// Archive branch exists.
	cmd = exec.Command("git", "branch", "--list", "flywheel/archive/task-keep-*")
	cmd.Dir = repo
	out, _ = cmd.CombinedOutput()
	if strings.TrimSpace(string(out)) == "" {
		t.Fatalf("archive branch not found. branches:\n%s", listAllBranches(t, repo))
	}
}

func TestRemoveWorktree_DeletesEmptyBranch(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "task-empty", "HEAD")
	if err != nil {
		t.Fatalf("Create: %v", err)
	}
	// No commits — branch is at baseRef.
	if err := mgr.Remove(ctx, "task-empty", "HEAD"); err != nil {
		t.Fatalf("Remove: %v", err)
	}
	// No archive branch should be created; the original branch is gone.
	cmd := exec.Command("git", "branch", "--list", "flywheel/archive/task-empty-*")
	cmd.Dir = repo
	out, _ := cmd.CombinedOutput()
	if strings.TrimSpace(string(out)) != "" {
		t.Fatalf("unexpected archive branch: %s", string(out))
	}
	cmd = exec.Command("git", "branch", "--list", wt.Branch)
	cmd.Dir = repo
	out, _ = cmd.CombinedOutput()
	if strings.TrimSpace(string(out)) != "" {
		t.Fatalf("branch %s still exists", wt.Branch)
	}
}

func listAllBranches(t *testing.T, repo string) string {
	t.Helper()
	cmd := exec.Command("git", "branch", "-a")
	cmd.Dir = repo
	out, _ := cmd.CombinedOutput()
	return string(out)
}

func TestListWorktrees(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	_, err := mgr.Create(ctx, "list-1", "HEAD")
	if err != nil {
		t.Fatalf("Create list-1: %v", err)
	}
	_, err = mgr.Create(ctx, "list-2", "HEAD")
	if err != nil {
		t.Fatalf("Create list-2: %v", err)
	}

	listed, err := mgr.List(ctx)
	if err != nil {
		t.Fatalf("List: %v", err)
	}

	if len(listed) != 2 {
		t.Fatalf("expected 2 worktrees, got %d", len(listed))
	}

	ids := map[string]bool{}
	for _, wt := range listed {
		ids[wt.TaskID] = true
		if wt.Path == "" {
			t.Fatal("listed worktree has empty Path")
		}
		if wt.Branch == "" {
			t.Fatal("listed worktree has empty Branch")
		}
	}
	if !ids["list-1"] || !ids["list-2"] {
		t.Fatalf("expected task IDs list-1 and list-2, got %v", ids)
	}
}

func TestIsCleanTrue(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "clean-1", "HEAD")
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	clean, err := mgr.IsClean(ctx, wt.Path)
	if err != nil {
		t.Fatalf("IsClean: %v", err)
	}
	if !clean {
		t.Fatal("expected clean worktree")
	}
}

func TestIsCleanFalse(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "dirty-1", "HEAD")
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	// Create an untracked file.
	if err := os.WriteFile(filepath.Join(wt.Path, "untracked.txt"), []byte("noise"), 0644); err != nil {
		t.Fatal(err)
	}

	clean, err := mgr.IsClean(ctx, wt.Path)
	if err != nil {
		t.Fatalf("IsClean: %v", err)
	}
	if clean {
		t.Fatal("expected dirty worktree")
	}
}

func TestCreate_CleansStaleBranchWithNoCommits(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	// Pre-create a stale branch with the same name that Create would pick.
	run := func(args ...string) {
		cmd := exec.Command(args[0], args[1:]...)
		cmd.Dir = repo
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("%v: %v: %s", args, err, out)
		}
	}
	run("git", "branch", "flywheel/stale-task", "HEAD")

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "stale-task", "HEAD")
	if err != nil {
		t.Fatalf("Create should succeed despite stale branch: %v", err)
	}
	if _, err := os.Stat(wt.Path); os.IsNotExist(err) {
		t.Fatal("worktree directory not created")
	}

	// No archive branch should be created for an empty stale branch.
	cmd := exec.Command("git", "branch", "--list", "flywheel/archive/stale-task-*")
	cmd.Dir = repo
	out, _ := cmd.CombinedOutput()
	if strings.TrimSpace(string(out)) != "" {
		t.Fatalf("unexpected archive branch for empty stale: %s", out)
	}
}

func TestCreate_ArchivesStaleBranchWithCommits(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	// Build a stale branch with a commit ahead of HEAD via a throwaway worktree.
	staging := t.TempDir() + "/staging"
	for _, args := range [][]string{
		{"git", "-C", repo, "worktree", "add", "-b", "flywheel/stale-commits", staging, "HEAD"},
	} {
		cmd := exec.Command(args[0], args[1:]...)
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("%v: %v: %s", args, err, out)
		}
	}
	if err := os.WriteFile(filepath.Join(staging, "work.txt"), []byte("prior"), 0644); err != nil {
		t.Fatal(err)
	}
	for _, args := range [][]string{
		{"git", "-C", staging, "add", "."},
		{"git", "-C", staging, "commit", "-m", "prior work"},
		{"git", "-C", repo, "worktree", "remove", "--force", staging},
	} {
		cmd := exec.Command(args[0], args[1:]...)
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("%v: %v: %s", args, err, out)
		}
	}
	// Branch flywheel/stale-commits now exists with 1 commit ahead of HEAD.

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "stale-commits", "HEAD")
	if err != nil {
		t.Fatalf("Create should succeed, archiving stale branch: %v", err)
	}
	if _, err := os.Stat(wt.Path); os.IsNotExist(err) {
		t.Fatal("worktree directory not created")
	}

	// Archive branch should exist, preserving the prior commit.
	cmd := exec.Command("git", "branch", "--list", "flywheel/archive/stale-commits-*")
	cmd.Dir = repo
	out, _ := cmd.CombinedOutput()
	if strings.TrimSpace(string(out)) == "" {
		t.Fatalf("expected archive branch. branches:\n%s", listAllBranches(t, repo))
	}
}

func TestCreate_CleansStaleWorktreeDir(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	// Leave a bare directory where Create will try to put a worktree.
	stalePath := filepath.Join(wtBase, "dir-clash")
	if err := os.MkdirAll(stalePath, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(stalePath, "junk"), []byte("x"), 0644); err != nil {
		t.Fatal(err)
	}

	ctx := context.Background()
	if _, err := mgr.Create(ctx, "dir-clash", "HEAD"); err != nil {
		t.Fatalf("Create should succeed despite stale dir: %v", err)
	}
}

func TestPruneStale_ArchivesOrphanBranches(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	// Create a worktree then blow away its directory to simulate a crashed run.
	wt, err := mgr.Create(ctx, "crashed", "HEAD")
	if err != nil {
		t.Fatalf("Create: %v", err)
	}
	if err := os.RemoveAll(wt.Path); err != nil {
		t.Fatal(err)
	}

	// Active is empty => everything is orphan.
	if err := mgr.PruneStale(ctx, map[string]bool{}, "HEAD"); err != nil {
		t.Fatalf("PruneStale: %v", err)
	}

	// Branch should be gone.
	cmd := exec.Command("git", "branch", "--list", wt.Branch)
	cmd.Dir = repo
	out, _ := cmd.CombinedOutput()
	if strings.TrimSpace(string(out)) != "" {
		t.Fatalf("orphan branch %s still present", wt.Branch)
	}
}

func TestPruneStale_KeepsActive(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "keep-me", "HEAD")
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	if err := mgr.PruneStale(ctx, map[string]bool{"keep-me": true}, "HEAD"); err != nil {
		t.Fatalf("PruneStale: %v", err)
	}
	if _, err := os.Stat(wt.Path); os.IsNotExist(err) {
		t.Fatal("active worktree was incorrectly pruned")
	}
}

func TestGetCommits(t *testing.T) {
	repo := setupTestRepo(t)
	wtBase := t.TempDir()
	mgr := NewManager(repo, wtBase, "flywheel/")

	ctx := context.Background()
	wt, err := mgr.Create(ctx, "commits-1", "HEAD")
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	// Make a commit in the worktree.
	f := filepath.Join(wt.Path, "new.txt")
	if err := os.WriteFile(f, []byte("data"), 0644); err != nil {
		t.Fatal(err)
	}
	for _, args := range [][]string{
		{"git", "-C", wt.Path, "add", "."},
		{"git", "-C", wt.Path, "commit", "-m", "add new file"},
	} {
		cmd := exec.Command(args[0], args[1:]...)
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("%s: %v: %s", strings.Join(args, " "), err, out)
		}
	}

	commits, err := mgr.GetCommits(ctx, wt.Path, wt.BaseSHA)
	if err != nil {
		t.Fatalf("GetCommits: %v", err)
	}
	if len(commits) != 1 {
		t.Fatalf("expected 1 commit, got %d", len(commits))
	}
	if commits[0].Message != "add new file" {
		t.Fatalf("unexpected message: %s", commits[0].Message)
	}
	if commits[0].SHA == "" {
		t.Fatal("commit SHA is empty")
	}
}
