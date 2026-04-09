package conflict

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/johnayoung/flywheel/internal/agent"
)

// testAgent is a real implementation of agent.Agent for testing.
type testAgent struct {
	output string
	err    error
}

func (a *testAgent) Execute(_ context.Context, _ agent.ExecutionRequest) (*agent.ExecutionResult, error) {
	if a.err != nil {
		return nil, a.err
	}
	return &agent.ExecutionResult{Success: true, Output: a.output}, nil
}

func TestBuildConflictPrompt(t *testing.T) {
	req := ResolveRequest{
		MergeWorkspace: "/tmp/workspace",
		Branch:         "feature-x",
		TargetRef:      "main",
		TaskDesc:       "Add caching layer",
		TargetDesc:     "Base implementation with no caching",
		ConflictFiles:  []string{"app.go", "config.go"},
	}
	contents := map[string]string{
		"app.go":    "<<<<<<< HEAD\nold code\n=======\nnew code\n>>>>>>> feature-x\n",
		"config.go": "<<<<<<< HEAD\nold config\n=======\nnew config\n>>>>>>> feature-x\n",
	}

	prompt := buildConflictPrompt(req, contents)

	checks := []struct {
		label string
		want  string
	}{
		{"branch name", "feature-x"},
		{"target ref", "main"},
		{"task description", "Add caching layer"},
		{"target description", "Base implementation with no caching"},
		{"conflict marker open", "<<<<<<<"},
		{"conflict marker separator", "======="},
		{"conflict marker close", ">>>>>>>"},
		{"first file path", "app.go"},
		{"second file path", "config.go"},
		{"format instruction FILE", "--- FILE:"},
		{"format instruction END", "--- END FILE ---"},
	}
	for _, c := range checks {
		if !strings.Contains(prompt, c.want) {
			t.Errorf("prompt missing %s (%q)", c.label, c.want)
		}
	}
}

func TestParseResolvedFiles(t *testing.T) {
	output := "Some preamble text\n" +
		"--- FILE: app.go ---\npackage main\n\nfunc main() {}\n--- END FILE ---\n" +
		"--- FILE: config.go ---\npackage config\n--- END FILE ---\n"

	result := parseResolvedFiles(output)

	if got, ok := result["app.go"]; !ok {
		t.Fatal("missing app.go")
	} else if want := "package main\n\nfunc main() {}"; got != want {
		t.Errorf("app.go: got %q, want %q", got, want)
	}

	if got, ok := result["config.go"]; !ok {
		t.Fatal("missing config.go")
	} else if want := "package config"; got != want {
		t.Errorf("config.go: got %q, want %q", got, want)
	}
}

// setupConflictRepo creates a real git repo with a merge conflict and returns
// the repo path and the conflicting file name. The caller must clean up the directory.
func setupConflictRepo(t *testing.T) (repoPath string, conflictFile string) {
	t.Helper()

	dir := t.TempDir()

	run := func(args ...string) string {
		t.Helper()
		cmd := exec.Command(args[0], args[1:]...)
		cmd.Dir = dir
		out, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("command %v failed: %s: %v", args, string(out), err)
		}
		return string(out)
	}

	run("git", "init", "-b", "main")
	run("git", "config", "user.email", "test@test.com")
	run("git", "config", "user.name", "Test")

	// Initial commit on main.
	conflictFile = "shared.txt"
	if err := os.WriteFile(filepath.Join(dir, conflictFile), []byte("line 1\nline 2\nline 3\n"), 0644); err != nil {
		t.Fatal(err)
	}
	run("git", "add", conflictFile)
	run("git", "commit", "-m", "initial")

	// Create feature branch and modify.
	run("git", "checkout", "-b", "feature-branch")
	if err := os.WriteFile(filepath.Join(dir, conflictFile), []byte("line 1\nfeature change\nline 3\n"), 0644); err != nil {
		t.Fatal(err)
	}
	run("git", "add", conflictFile)
	run("git", "commit", "-m", "feature change")

	// Back to main and make conflicting change.
	run("git", "checkout", "main")
	if err := os.WriteFile(filepath.Join(dir, conflictFile), []byte("line 1\nmain change\nline 3\n"), 0644); err != nil {
		t.Fatal(err)
	}
	run("git", "add", conflictFile)
	run("git", "commit", "-m", "main change")

	// Start merge (will fail with conflict).
	mergeCmd := exec.Command("git", "merge", "feature-branch")
	mergeCmd.Dir = dir
	mergeOut, mergeErr := mergeCmd.CombinedOutput()
	if mergeErr == nil {
		t.Fatal("expected merge conflict but merge succeeded: " + string(mergeOut))
	}

	return dir, conflictFile
}

func TestResolveSuccess(t *testing.T) {
	repoPath, conflictFile := setupConflictRepo(t)

	// Read the conflicted file to confirm markers are present.
	data, err := os.ReadFile(filepath.Join(repoPath, conflictFile))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(data), "<<<<<<<") {
		t.Fatal("expected conflict markers in file")
	}

	// Prepare test agent that returns properly formatted resolved content.
	resolvedContent := "line 1\nmerged: main change + feature change\nline 3\n"
	agentOutput := fmt.Sprintf("--- FILE: %s ---\n%s\n--- END FILE ---\n", conflictFile, resolvedContent)

	ag := &testAgent{output: agentOutput}
	resolver := New(ag)

	result, err := resolver.Resolve(context.Background(), ResolveRequest{
		MergeWorkspace: repoPath,
		Branch:         "feature-branch",
		TargetRef:      "main",
		TaskDesc:       "Feature change",
		TargetDesc:     "Main change",
		ConflictFiles:  []string{conflictFile},
	})
	if err != nil {
		t.Fatalf("Resolve returned error: %v", err)
	}
	if !result.Success {
		t.Fatalf("expected success, got error: %s", result.Error)
	}
	if len(result.ResolvedFiles) != 1 || result.ResolvedFiles[0] != conflictFile {
		t.Errorf("unexpected resolved files: %v", result.ResolvedFiles)
	}

	// Verify the file content on disk.
	final, err := os.ReadFile(filepath.Join(repoPath, conflictFile))
	if err != nil {
		t.Fatal(err)
	}
	if string(final) != resolvedContent {
		t.Errorf("file content mismatch: got %q, want %q", string(final), resolvedContent)
	}

	// Verify merge commit exists.
	cmd := exec.Command("git", "-C", repoPath, "log", "--oneline", "-1")
	out, err := cmd.Output()
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(strings.ToLower(string(out)), "merge") {
		t.Errorf("expected merge commit, got: %s", string(out))
	}
}

func TestResolveFailure_ConflictsRemain(t *testing.T) {
	repoPath, conflictFile := setupConflictRepo(t)

	// Agent returns garbage that won't resolve anything.
	ag := &testAgent{output: "I don't know how to resolve this."}
	resolver := New(ag)

	result, err := resolver.Resolve(context.Background(), ResolveRequest{
		MergeWorkspace: repoPath,
		Branch:         "feature-branch",
		TargetRef:      "main",
		TaskDesc:       "Feature change",
		TargetDesc:     "Main change",
		ConflictFiles:  []string{conflictFile},
	})
	if err != nil {
		t.Fatalf("Resolve returned unexpected error: %v", err)
	}
	if result.Success {
		t.Fatal("expected failure but got success")
	}
	if result.Error == "" {
		t.Fatal("expected non-empty error message")
	}
}

func TestResolveFailure_AgentError(t *testing.T) {
	repoPath, conflictFile := setupConflictRepo(t)

	ag := &testAgent{err: fmt.Errorf("agent crashed")}
	resolver := New(ag)

	_, err := resolver.Resolve(context.Background(), ResolveRequest{
		MergeWorkspace: repoPath,
		Branch:         "feature-branch",
		TargetRef:      "main",
		TaskDesc:       "Feature change",
		TargetDesc:     "Main change",
		ConflictFiles:  []string{conflictFile},
	})
	if err == nil {
		t.Fatal("expected error from agent failure")
	}
	if !strings.Contains(err.Error(), "agent crashed") {
		t.Errorf("error should contain agent message, got: %v", err)
	}
}
