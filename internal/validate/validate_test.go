package validate

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/task"
)

// initRepo creates a git repo in dir with one initial commit and returns the commit SHA.
func initRepo(t *testing.T, dir string) string {
	t.Helper()
	cmds := [][]string{
		{"git", "init", "-b", "main"},
		{"git", "config", "user.email", "test@test.com"},
		{"git", "config", "user.name", "Test"},
		{"git", "config", "commit.gpgsign", "false"},
	}
	for _, args := range cmds {
		cmd := exec.Command(args[0], args[1:]...)
		cmd.Dir = dir
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("command %v failed: %v\n%s", args, err, out)
		}
	}

	// Create initial commit
	readmePath := filepath.Join(dir, "README.md")
	if err := os.WriteFile(readmePath, []byte("# init\n"), 0644); err != nil {
		t.Fatal(err)
	}
	run(t, dir, "git", "add", "README.md")
	run(t, dir, "git", "commit", "-m", "initial commit")
	return strings.TrimSpace(runOutput(t, dir, "git", "rev-parse", "HEAD"))
}

func run(t *testing.T, dir string, name string, args ...string) {
	t.Helper()
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("%s %v failed: %v\n%s", name, args, err, out)
	}
}

func runOutput(t *testing.T, dir string, name string, args ...string) string {
	t.Helper()
	cmd := exec.Command(name, args...)
	cmd.Dir = dir
	out, err := cmd.Output()
	if err != nil {
		t.Fatalf("%s %v failed: %v", name, args, err)
	}
	return string(out)
}

func newTask(commitPrefix string) task.Task {
	return task.Task{
		ID:          "test-1",
		Description: "test task",
		Category:    "feat",
		Priority:    1,
		Commit:      commitPrefix,
		Steps:       []string{"do something"},
	}
}

func TestAllChecksPass(t *testing.T) {
	dir := t.TempDir()
	baseSHA := initRepo(t, dir)

	// Create a new branch and add a properly-prefixed commit
	run(t, dir, "git", "checkout", "-b", "work")
	if err := os.WriteFile(filepath.Join(dir, "feature.go"), []byte("package main\n"), 0644); err != nil {
		t.Fatal(err)
	}
	run(t, dir, "git", "add", "feature.go")
	run(t, dir, "git", "commit", "-m", "feat(foo): add feature")

	tk := newTask("feat(foo)")
	lc := lifecycle.Lifecycle{
		WorktreePath: dir,
		BaseSHA:      baseSHA,
	}

	v := New("true")
	result, err := v.Validate(context.Background(), tk, lc)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !result.Passed {
		for _, c := range result.Checks {
			t.Logf("check %s: passed=%v detail=%s", c.Name, c.Passed, c.Detail)
		}
		t.Fatal("expected all checks to pass")
	}

	// Verify we got exactly 4 checks (3 git checks + build, no acceptance criteria since agent=nil)
	if len(result.Checks) != 4 {
		t.Fatalf("expected 4 checks, got %d", len(result.Checks))
	}
}

func TestDirtyWorktreeFails(t *testing.T) {
	dir := t.TempDir()
	baseSHA := initRepo(t, dir)

	run(t, dir, "git", "checkout", "-b", "work")
	if err := os.WriteFile(filepath.Join(dir, "feature.go"), []byte("package main\n"), 0644); err != nil {
		t.Fatal(err)
	}
	run(t, dir, "git", "add", "feature.go")
	run(t, dir, "git", "commit", "-m", "feat(bar): add feature")

	// Create an untracked file to dirty the worktree
	if err := os.WriteFile(filepath.Join(dir, "leftover.txt"), []byte("oops"), 0644); err != nil {
		t.Fatal(err)
	}

	tk := newTask("feat(bar)")
	lc := lifecycle.Lifecycle{
		WorktreePath: dir,
		BaseSHA:      baseSHA,
	}

	v := New("")
	result, err := v.Validate(context.Background(), tk, lc)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Passed {
		t.Fatal("expected validation to fail due to dirty worktree")
	}

	// clean_worktree should fail, but commit_exists and commit_message should pass
	checkMap := make(map[string]CheckResult)
	for _, c := range result.Checks {
		checkMap[c.Name] = c
	}
	if checkMap["clean_worktree"].Passed {
		t.Error("expected clean_worktree to fail")
	}
	if !checkMap["commit_exists"].Passed {
		t.Error("expected commit_exists to pass")
	}
	if !checkMap["commit_message"].Passed {
		t.Error("expected commit_message to pass")
	}
}

func TestNoCommitsFails(t *testing.T) {
	dir := t.TempDir()
	baseSHA := initRepo(t, dir)

	// Create branch but don't add any commits
	run(t, dir, "git", "checkout", "-b", "work")

	tk := newTask("feat(x)")
	lc := lifecycle.Lifecycle{
		WorktreePath: dir,
		BaseSHA:      baseSHA,
	}

	v := New("")
	result, err := v.Validate(context.Background(), tk, lc)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Passed {
		t.Fatal("expected validation to fail due to no commits")
	}

	checkMap := make(map[string]CheckResult)
	for _, c := range result.Checks {
		checkMap[c.Name] = c
	}
	if !checkMap["clean_worktree"].Passed {
		t.Error("expected clean_worktree to pass")
	}
	if checkMap["commit_exists"].Passed {
		t.Error("expected commit_exists to fail")
	}
	if checkMap["commit_message"].Passed {
		t.Error("expected commit_message to fail (no commits to check)")
	}
}

func TestWrongCommitMessage(t *testing.T) {
	dir := t.TempDir()
	baseSHA := initRepo(t, dir)

	run(t, dir, "git", "checkout", "-b", "work")
	if err := os.WriteFile(filepath.Join(dir, "f.go"), []byte("package main\n"), 0644); err != nil {
		t.Fatal(err)
	}
	run(t, dir, "git", "add", "f.go")
	run(t, dir, "git", "commit", "-m", "fix(wrong): something else")

	tk := newTask("feat(expected)")
	lc := lifecycle.Lifecycle{
		WorktreePath: dir,
		BaseSHA:      baseSHA,
	}

	v := New("")
	result, err := v.Validate(context.Background(), tk, lc)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Passed {
		t.Fatal("expected validation to fail due to wrong commit message")
	}

	checkMap := make(map[string]CheckResult)
	for _, c := range result.Checks {
		checkMap[c.Name] = c
	}
	if !checkMap["clean_worktree"].Passed {
		t.Error("expected clean_worktree to pass")
	}
	if !checkMap["commit_exists"].Passed {
		t.Error("expected commit_exists to pass")
	}
	if checkMap["commit_message"].Passed {
		t.Error("expected commit_message to fail")
	}
}

func TestBuildFailure(t *testing.T) {
	dir := t.TempDir()
	baseSHA := initRepo(t, dir)

	run(t, dir, "git", "checkout", "-b", "work")
	if err := os.WriteFile(filepath.Join(dir, "f.go"), []byte("package main\n"), 0644); err != nil {
		t.Fatal(err)
	}
	run(t, dir, "git", "add", "f.go")
	run(t, dir, "git", "commit", "-m", "feat(ok): add file")

	tk := newTask("feat(ok)")
	lc := lifecycle.Lifecycle{
		WorktreePath: dir,
		BaseSHA:      baseSHA,
	}

	v := New("false")
	result, err := v.Validate(context.Background(), tk, lc)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Passed {
		t.Fatal("expected validation to fail due to build failure")
	}

	checkMap := make(map[string]CheckResult)
	for _, c := range result.Checks {
		checkMap[c.Name] = c
	}
	if !checkMap["clean_worktree"].Passed {
		t.Error("expected clean_worktree to pass")
	}
	if !checkMap["commit_exists"].Passed {
		t.Error("expected commit_exists to pass")
	}
	if !checkMap["commit_message"].Passed {
		t.Error("expected commit_message to pass")
	}
	if checkMap["build"].Passed {
		t.Error("expected build to fail")
	}
}
