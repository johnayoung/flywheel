package review

import (
	"context"
	"fmt"
	"strings"

	"github.com/johnayoung/flywheel/internal/agent"
	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/task"
)

// Reviewer evaluates task implementation quality and correctness.
type Reviewer interface {
	Review(ctx context.Context, req ReviewRequest) (*ReviewResult, error)
}

// ReviewRequest contains everything needed to review a task's implementation.
type ReviewRequest struct {
	Task         task.Task
	Lifecycle    lifecycle.Lifecycle
	Diff         string
	WorktreePath string
}

// ReviewResult captures the reviewer's verdict.
type ReviewResult struct {
	Approved    bool
	Feedback    string
	Suggestions []string
}

// AgentReviewer delegates code review to an AI agent.
type AgentReviewer struct {
	agent agent.Agent
}

// NewAgentReviewer creates a reviewer backed by the given agent.
func NewAgentReviewer(ag agent.Agent) *AgentReviewer {
	return &AgentReviewer{agent: ag}
}

// Review sends the diff and task context to the agent for evaluation.
func (r *AgentReviewer) Review(ctx context.Context, req ReviewRequest) (*ReviewResult, error) {
	prompt := buildReviewPrompt(req)

	execResult, err := r.agent.Execute(ctx, agent.ExecutionRequest{
		WorktreePath: req.WorktreePath,
		TaskID:       req.Task.ID,
		Description:  prompt,
		Steps:        []string{"Review the code changes and provide a verdict"},
	})
	if err != nil {
		return nil, fmt.Errorf("agent review execution failed: %w", err)
	}

	return parseReviewOutput(execResult.Output), nil
}

// buildReviewPrompt constructs the full prompt sent to the review agent.
func buildReviewPrompt(req ReviewRequest) string {
	var b strings.Builder

	b.WriteString("You are reviewing code changes for a task.\n\n")

	b.WriteString("## Task\n")
	b.WriteString(fmt.Sprintf("ID: %s\n", req.Task.ID))
	b.WriteString(fmt.Sprintf("Description: %s\n\n", req.Task.Description))

	if len(req.Task.AcceptanceCriteria) > 0 {
		b.WriteString("## Acceptance Criteria\n")
		for _, ac := range req.Task.AcceptanceCriteria {
			b.WriteString(fmt.Sprintf("- %s\n", ac))
		}
		b.WriteString("\n")
	}

	b.WriteString("## Diff\n```\n")
	b.WriteString(req.Diff)
	b.WriteString("\n```\n\n")

	b.WriteString("## Instructions\n")
	b.WriteString("Review the diff for:\n")
	b.WriteString("1. Code quality (readability, maintainability, idiomatic patterns)\n")
	b.WriteString("2. Correctness (logic errors, edge cases, error handling)\n")
	b.WriteString("3. Adherence to the task description and acceptance criteria\n\n")
	b.WriteString("Respond with exactly one of APPROVED or REJECTED on its own line.\n")
	b.WriteString("Then provide feedback explaining your reasoning.\n")
	b.WriteString("If you have non-blocking suggestions, list each on a line starting with \"SUGGESTION: \".\n")

	return b.String()
}

// parseReviewOutput extracts the verdict, feedback, and suggestions from agent output.
func parseReviewOutput(output string) *ReviewResult {
	result := &ReviewResult{}

	lines := strings.Split(output, "\n")

	var feedbackLines []string
	for _, line := range lines {
		trimmed := strings.TrimSpace(line)

		if strings.Contains(strings.ToUpper(trimmed), "APPROVED") && !result.Approved {
			result.Approved = true
			continue
		}
		if strings.Contains(strings.ToUpper(trimmed), "REJECTED") {
			result.Approved = false
			continue
		}

		if strings.HasPrefix(trimmed, "SUGGESTION: ") {
			result.Suggestions = append(result.Suggestions, strings.TrimPrefix(trimmed, "SUGGESTION: "))
			continue
		}

		if trimmed != "" {
			feedbackLines = append(feedbackLines, trimmed)
		}
	}

	result.Feedback = strings.Join(feedbackLines, "\n")
	return result
}
