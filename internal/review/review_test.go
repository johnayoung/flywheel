package review

import (
	"bytes"
	"context"
	"strings"
	"testing"

	"github.com/johnayoung/flywheel/internal/agent"
	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/task"
)

// testAgent is a real implementation of agent.Agent for testing purposes.
type testAgent struct {
	output string
	err    error
}

func (a *testAgent) Execute(_ context.Context, _ agent.ExecutionRequest) (*agent.ExecutionResult, error) {
	if a.err != nil {
		return nil, a.err
	}
	return &agent.ExecutionResult{
		Success: true,
		Output:  a.output,
	}, nil
}

func sampleRequest() ReviewRequest {
	return ReviewRequest{
		Task: task.Task{
			ID:          "task-1",
			Description: "Add user authentication",
			AcceptanceCriteria: []string{
				"Login endpoint returns JWT",
				"Password is hashed with bcrypt",
			},
		},
		Lifecycle:    lifecycle.Lifecycle{TaskID: "task-1", RunID: "run-1"},
		Diff:         "+++ b/auth.go\n+func Login() {}\n",
		WorktreePath: "/tmp/worktree",
	}
}

func TestBuildReviewPrompt(t *testing.T) {
	req := sampleRequest()
	prompt := buildReviewPrompt(req)

	checks := []string{
		req.Diff,
		req.Task.Description,
		"Login endpoint returns JWT",
		"Password is hashed with bcrypt",
	}
	for _, want := range checks {
		if !strings.Contains(prompt, want) {
			t.Errorf("prompt missing expected content: %q", want)
		}
	}
}

func TestAgentReviewerApproval(t *testing.T) {
	ag := &testAgent{output: "Looks good.\nAPPROVED\nSUGGESTION: Add a timeout to the HTTP client."}
	reviewer := NewAgentReviewer(ag)

	result, err := reviewer.Review(context.Background(), sampleRequest())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !result.Approved {
		t.Error("expected Approved=true")
	}
	if len(result.Suggestions) != 1 {
		t.Fatalf("expected 1 suggestion, got %d", len(result.Suggestions))
	}
	if result.Suggestions[0] != "Add a timeout to the HTTP client." {
		t.Errorf("unexpected suggestion: %q", result.Suggestions[0])
	}
}

func TestAgentReviewerRejection(t *testing.T) {
	ag := &testAgent{output: "Missing error handling in Login.\nREJECTED\nPlease add proper error returns."}
	reviewer := NewAgentReviewer(ag)

	result, err := reviewer.Review(context.Background(), sampleRequest())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Approved {
		t.Error("expected Approved=false")
	}
	if result.Feedback == "" {
		t.Error("expected non-empty feedback")
	}
}

func TestHITLReviewerApprove(t *testing.T) {
	in := bytes.NewBufferString("a\n")
	out := &bytes.Buffer{}
	reviewer := NewHITLReviewer(in, out)

	result, err := reviewer.Review(context.Background(), sampleRequest())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !result.Approved {
		t.Error("expected Approved=true")
	}
}

func TestHITLReviewerReject(t *testing.T) {
	in := bytes.NewBufferString("r\nbad code\n")
	out := &bytes.Buffer{}
	reviewer := NewHITLReviewer(in, out)

	result, err := reviewer.Review(context.Background(), sampleRequest())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Approved {
		t.Error("expected Approved=false")
	}
	if result.Feedback != "bad code" {
		t.Errorf("expected feedback %q, got %q", "bad code", result.Feedback)
	}
}

func TestHITLReviewerDiffThenApprove(t *testing.T) {
	in := bytes.NewBufferString("d\na\n")
	out := &bytes.Buffer{}
	req := sampleRequest()
	reviewer := NewHITLReviewer(in, out)

	result, err := reviewer.Review(context.Background(), req)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if !result.Approved {
		t.Error("expected Approved=true after d then a")
	}
	if !strings.Contains(out.String(), req.Diff) {
		t.Error("expected full diff to be printed to output")
	}
}

func TestHITLReviewerSkip(t *testing.T) {
	in := bytes.NewBufferString("s\n")
	out := &bytes.Buffer{}
	reviewer := NewHITLReviewer(in, out)

	result, err := reviewer.Review(context.Background(), sampleRequest())
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if result.Approved {
		t.Error("expected Approved=false for skip")
	}
	if result.Feedback != "skipped" {
		t.Errorf("expected feedback %q, got %q", "skipped", result.Feedback)
	}
}
