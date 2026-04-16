package claudecode

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/johnayoung/flywheel/internal/agent"
)

func TestBuildPrompt_Basic(t *testing.T) {
	req := agent.ExecutionRequest{
		TaskID:      "TASK-42",
		Description: "Add logging to the server",
		Steps:       []string{"Create logger package", "Integrate with HTTP handler"},
		Commit:      "feat(logging)",
	}

	prompt := BuildPrompt(req)

	if !strings.Contains(prompt, "Add logging to the server") {
		t.Error("prompt should contain the task description")
	}
	if !strings.Contains(prompt, "1. Create logger package") {
		t.Error("prompt should contain numbered step 1")
	}
	if !strings.Contains(prompt, "2. Integrate with HTTP handler") {
		t.Error("prompt should contain numbered step 2")
	}
	if !strings.Contains(prompt, "feat(logging): implement changes") {
		t.Error("prompt should contain commit message with task commit prefix")
	}
	if strings.Contains(prompt, "Resume from step") {
		t.Error("prompt should not contain resume instruction when ResumeFrom is 0")
	}
}

func TestBuildPrompt_NoCommit(t *testing.T) {
	req := agent.ExecutionRequest{
		TaskID:      "TASK-1",
		Description: "Fix the bug",
		Steps:       []string{"Find and fix"},
	}

	prompt := BuildPrompt(req)

	if !strings.Contains(prompt, "When finished, commit your work.") {
		t.Error("prompt should contain generic commit instruction when no Commit prefix")
	}
}

func TestBuildPrompt_Resume(t *testing.T) {
	req := agent.ExecutionRequest{
		TaskID:      "TASK-99",
		Description: "Refactor database layer",
		Steps:       []string{"Extract interface", "Update callers", "Add tests"},
		ResumeFrom:  2,
	}

	prompt := BuildPrompt(req)

	if !strings.Contains(prompt, "Resume from step 2") {
		t.Error("prompt should indicate resume from step 2")
	}
	if !strings.Contains(prompt, "Steps 1 through 1 are already completed") {
		t.Error("prompt should note prior steps are done")
	}
}

func TestBuildPrompt_NoSteps(t *testing.T) {
	req := agent.ExecutionRequest{
		TaskID:      "TASK-1",
		Description: "Fix the bug",
	}

	prompt := BuildPrompt(req)

	if !strings.Contains(prompt, "Fix the bug") {
		t.Error("prompt should contain description even with no steps")
	}
	if strings.Contains(prompt, "Steps:") {
		t.Error("prompt should not contain Steps header when there are no steps")
	}
}

func writeScript(t *testing.T, dir, name, content string) string {
	t.Helper()
	path := filepath.Join(dir, name)
	err := os.WriteFile(path, []byte(content), 0o755)
	if err != nil {
		t.Fatalf("failed to write script %s: %v", name, err)
	}
	return path
}

func TestExecute_Success(t *testing.T) {
	tmpDir := t.TempDir()
	scriptPath := writeScript(t, tmpDir, "claude", `#!/bin/sh
printf '{"result":"All steps completed successfully.","is_error":false}'
`)

	c := New(WithClaudePath(scriptPath))
	req := agent.ExecutionRequest{
		WorktreePath: tmpDir,
		TaskID:       "TASK-1",
		Description:  "Do the thing",
		Steps:        []string{"Step A", "Step B"},
	}

	result, err := c.Execute(context.Background(), req)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !result.Success {
		t.Errorf("expected Success=true, got false; Error=%s", result.Error)
	}
	if result.StepsCompleted != 2 {
		t.Errorf("expected StepsCompleted=2, got %d", result.StepsCompleted)
	}
	if result.Output != "All steps completed successfully." {
		t.Errorf("unexpected output: %s", result.Output)
	}
}

func TestExecute_IsError(t *testing.T) {
	tmpDir := t.TempDir()
	scriptPath := writeScript(t, tmpDir, "claude", `#!/bin/sh
printf '{"result":"something went wrong","is_error":true}'
`)

	c := New(WithClaudePath(scriptPath))
	req := agent.ExecutionRequest{
		WorktreePath: tmpDir,
		TaskID:       "TASK-2",
		Description:  "Break things",
		Steps:        []string{"Step 1"},
	}

	result, err := c.Execute(context.Background(), req)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Success {
		t.Error("expected Success=false for is_error=true response")
	}
	if result.Error == "" {
		t.Error("expected Error to be populated")
	}
}

func TestExecute_NonZeroExit(t *testing.T) {
	tmpDir := t.TempDir()
	scriptPath := writeScript(t, tmpDir, "claude", `#!/bin/sh
printf '{"result":"crash","is_error":false}'
exit 1
`)

	c := New(WithClaudePath(scriptPath))
	req := agent.ExecutionRequest{
		WorktreePath: tmpDir,
		TaskID:       "TASK-3",
		Description:  "Fail",
		Steps:        []string{"Step 1"},
	}

	result, err := c.Execute(context.Background(), req)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Success {
		t.Error("expected Success=false for non-zero exit")
	}
	if result.Error == "" {
		t.Error("expected Error to be populated on non-zero exit")
	}
}

func TestExecute_ContextCancelled(t *testing.T) {
	tmpDir := t.TempDir()
	// Use a real script that would take time, but context is already cancelled
	scriptPath := writeScript(t, tmpDir, "claude", `#!/bin/sh
sleep 60
`)

	c := New(WithClaudePath(scriptPath))
	req := agent.ExecutionRequest{
		WorktreePath: tmpDir,
		TaskID:       "TASK-4",
		Description:  "Should be cancelled",
		Steps:        []string{"Step 1"},
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel() // cancel immediately

	_, err := c.Execute(ctx, req)
	if err == nil {
		t.Fatal("expected error for cancelled context")
	}
	if !strings.Contains(err.Error(), "cancelled") {
		t.Errorf("expected cancellation error, got: %v", err)
	}
}

func TestExecute_StderrCapturedOnError(t *testing.T) {
	tmpDir := t.TempDir()
	scriptPath := writeScript(t, tmpDir, "claude", `#!/bin/sh
echo "stderr diagnostic output" >&2
exit 1
`)

	c := New(WithClaudePath(scriptPath))
	req := agent.ExecutionRequest{
		WorktreePath: tmpDir,
		TaskID:       "TASK-5",
		Description:  "Stderr test",
		Steps:        []string{"Step 1"},
	}

	result, err := c.Execute(context.Background(), req)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Success {
		t.Error("expected Success=false")
	}
	if !strings.Contains(result.Error, "stderr diagnostic output") {
		t.Errorf("expected stderr in error, got: %s", result.Error)
	}
}

func TestExecute_Streaming_ParsesNDJSONAndCallsOnChunk(t *testing.T) {
	tmpDir := t.TempDir()
	// Emit two assistant text chunks, one tool_use, then a result event.
	scriptPath := writeScript(t, tmpDir, "claude", `#!/bin/sh
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"world"}]}}'
printf '%s\n' '{"type":"result","result":"all done","is_error":false}'
`)

	var chunks []string
	c := New(
		WithClaudePath(scriptPath),
		WithStreaming(true),
		WithOnChunk(func(s string) { chunks = append(chunks, s) }),
	)
	req := agent.ExecutionRequest{
		WorktreePath: tmpDir,
		TaskID:       "TASK-S",
		Description:  "stream test",
		Steps:        []string{"a", "b"},
	}

	result, err := c.Execute(context.Background(), req)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !result.Success {
		t.Errorf("expected Success, got Error=%q", result.Error)
	}
	if result.Output != "all done" {
		t.Errorf("Output=%q, want 'all done'", result.Output)
	}
	want := []string{"hello", "[tool_use] Bash", "world"}
	if len(chunks) != len(want) {
		t.Fatalf("got %d chunks (%v), want %d (%v)", len(chunks), chunks, len(want), want)
	}
	for i, c := range chunks {
		if c != want[i] {
			t.Errorf("chunk %d = %q, want %q", i, c, want[i])
		}
	}
}

func TestExecute_Streaming_NoResultEventIsError(t *testing.T) {
	tmpDir := t.TempDir()
	scriptPath := writeScript(t, tmpDir, "claude", `#!/bin/sh
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"oops"}]}}'
`)

	c := New(WithClaudePath(scriptPath), WithStreaming(true))
	req := agent.ExecutionRequest{
		WorktreePath: tmpDir,
		TaskID:       "TASK-S2",
		Description:  "no result",
	}
	result, err := c.Execute(context.Background(), req)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Success {
		t.Error("expected Success=false when no result event arrives")
	}
	if !strings.Contains(result.Error, "result event") {
		t.Errorf("expected 'result event' in error, got: %q", result.Error)
	}
}

func TestNew_DefaultClaudePath(t *testing.T) {
	c := New()
	if c.claudePath != "claude" {
		t.Errorf("expected default claudePath='claude', got %q", c.claudePath)
	}
}

func TestNew_WithClaudePath(t *testing.T) {
	c := New(WithClaudePath("/usr/local/bin/claude"))
	if c.claudePath != "/usr/local/bin/claude" {
		t.Errorf("expected claudePath='/usr/local/bin/claude', got %q", c.claudePath)
	}
}
