package claudecode

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"strings"

	"github.com/johnayoung/flywheel/internal/agent"
)

// ClaudeCode implements agent.Agent using the Claude Code CLI.
type ClaudeCode struct {
	claudePath string
}

// Option configures a ClaudeCode instance.
type Option func(*ClaudeCode)

// WithClaudePath sets the path to the claude CLI binary.
func WithClaudePath(path string) Option {
	return func(c *ClaudeCode) {
		c.claudePath = path
	}
}

// New creates a new ClaudeCode agent with the given options.
func New(opts ...Option) *ClaudeCode {
	c := &ClaudeCode{
		claudePath: "claude",
	}
	for _, opt := range opts {
		opt(c)
	}
	return c
}

// claudeOutput represents the JSON output from claude --output-format json.
type claudeOutput struct {
	Result  string `json:"result"`
	IsError bool   `json:"is_error"`
}

// BuildPrompt constructs the prompt string for the Claude Code CLI from an execution request.
func BuildPrompt(req agent.ExecutionRequest) string {
	var b strings.Builder

	fmt.Fprintf(&b, "Task: %s\n\n", req.Description)

	if len(req.Steps) > 0 {
		b.WriteString("Steps:\n")
		for i, step := range req.Steps {
			fmt.Fprintf(&b, "%d. %s\n", i+1, step)
		}
		b.WriteString("\n")
	}

	if req.ResumeFrom > 0 {
		fmt.Fprintf(&b, "Resume from step %d. Steps 1 through %d are already completed.\n\n", req.ResumeFrom, req.ResumeFrom-1)
	}

	fmt.Fprintf(&b, "When finished, commit your work with the message: \"task(%s): implement changes\"\n\n", req.TaskID)
	b.WriteString("Stay within scope: only modify files directly related to this task. Do not refactor unrelated code or make changes outside the task requirements.\n")

	return b.String()
}

// Execute runs the Claude Code CLI against the given request.
func (c *ClaudeCode) Execute(ctx context.Context, req agent.ExecutionRequest) (*agent.ExecutionResult, error) {
	prompt := BuildPrompt(req)

	cmd := exec.CommandContext(ctx, c.claudePath, "--dangerously-skip-permissions", "-p", prompt, "--output-format", "json")
	cmd.Dir = req.WorktreePath

	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	err := cmd.Run()
	if ctx.Err() != nil {
		return nil, fmt.Errorf("execution cancelled: %w", ctx.Err())
	}

	if err != nil {
		exitErr := ""
		if stderr.Len() > 0 {
			exitErr = stderr.String()
		} else {
			exitErr = err.Error()
		}
		// Try to parse JSON output even on non-zero exit
		var out claudeOutput
		if jsonErr := json.Unmarshal(stdout.Bytes(), &out); jsonErr == nil && out.Result != "" {
			return &agent.ExecutionResult{
				Success:             false,
				StepsCompleted:      0,
				Output:              out.Result,
				ImplementationNotes: out.Result,
				Error:               exitErr,
			}, nil
		}
		return &agent.ExecutionResult{
			Success:             false,
			StepsCompleted:      0,
			Output:              stdout.String(),
			ImplementationNotes: "",
			Error:               exitErr,
		}, nil
	}

	var out claudeOutput
	if jsonErr := json.Unmarshal(stdout.Bytes(), &out); jsonErr != nil {
		return &agent.ExecutionResult{
			Success:             false,
			StepsCompleted:      0,
			Output:              stdout.String(),
			ImplementationNotes: "",
			Error:               fmt.Sprintf("failed to parse claude output: %v", jsonErr),
		}, nil
	}

	if out.IsError {
		return &agent.ExecutionResult{
			Success:             false,
			StepsCompleted:      0,
			Output:              out.Result,
			ImplementationNotes: out.Result,
			Error:               out.Result,
		}, nil
	}

	return &agent.ExecutionResult{
		Success:             true,
		StepsCompleted:      len(req.Steps),
		Output:              out.Result,
		ImplementationNotes: out.Result,
	}, nil
}
