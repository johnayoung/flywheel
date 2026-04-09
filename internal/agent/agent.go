package agent

import "context"

// Agent is the abstraction that allows different coding agents to be plugged in.
type Agent interface {
	Execute(ctx context.Context, req ExecutionRequest) (*ExecutionResult, error)
}

// ExecutionRequest contains the information needed for an agent to execute a task.
type ExecutionRequest struct {
	WorktreePath string
	TaskID       string
	Description  string
	Steps        []string
	ResumeFrom   int // 0 = start from beginning, N = resume from step N (used on retry)
}

// ExecutionResult captures the outcome of an agent execution.
type ExecutionResult struct {
	Success             bool
	StepsCompleted      int
	Output              string // full agent output/logs
	ImplementationNotes string // agent's summary of what it did
	Error               string // error message if failed
}
