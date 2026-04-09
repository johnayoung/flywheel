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

	if err := mgr.Remove(ctx, "task-rm"); err != nil {
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
