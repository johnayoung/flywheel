package lifecycle

import "time"

type Status string

const (
	StatusPending          Status = "pending"
	StatusReady            Status = "ready"
	StatusRunning          Status = "running"
	StatusValidating       Status = "validating"
	StatusFailedValidation Status = "failed_validation"
	StatusReviewing        Status = "reviewing"
	StatusRejected         Status = "rejected"
	StatusMerging          Status = "merging"
	StatusConflict         Status = "conflict"
	StatusResolving        Status = "resolving"
	StatusMerged           Status = "merged"
	StatusFailed           Status = "failed"
)

type Timestamps struct {
	CreatedAt   *time.Time `json:"created_at,omitempty"`
	ReadyAt     *time.Time `json:"ready_at,omitempty"`
	StartedAt   *time.Time `json:"started_at,omitempty"`
	CompletedAt *time.Time `json:"completed_at,omitempty"`
	ReviewedAt  *time.Time `json:"reviewed_at,omitempty"`
	MergedAt    *time.Time `json:"merged_at,omitempty"`
	FailedAt    *time.Time `json:"failed_at,omitempty"`
}

type Lifecycle struct {
	TaskID          string     `json:"task_id"`
	RunID           string     `json:"run_id"`
	WorkerID        string     `json:"worker_id,omitempty"`
	Status          Status     `json:"status"`
	Branch          string     `json:"branch,omitempty"`
	WorktreePath    string     `json:"worktree_path,omitempty"`
	BaseRef         string     `json:"base_ref"`
	BaseSHA         string     `json:"base_sha,omitempty"`
	CurrentStep     int        `json:"current_step"`
	StepsCompleted  []string   `json:"steps_completed,omitempty"`
	Timestamps      Timestamps `json:"timestamps"`
	Version         int        `json:"version"`
	Retries         int        `json:"retries"`
	ResolveAttempts int        `json:"resolve_attempts"`
	Error           string     `json:"error,omitempty"`
	AgentOutput     string     `json:"agent_output,omitempty"`
	ImplNotes       string     `json:"implementation_notes,omitempty"`
	ReviewResult    string     `json:"review_result,omitempty"`
	MergeCommitSHA  string     `json:"merge_commit_sha,omitempty"`
	ConflictDetails string     `json:"conflict_details,omitempty"`
}

func NewLifecycle(taskID, runID, baseRef string) *Lifecycle {
	now := time.Now()
	return &Lifecycle{
		TaskID:  taskID,
		RunID:   runID,
		BaseRef: baseRef,
		Status:  StatusPending,
		Timestamps: Timestamps{
			CreatedAt: &now,
		},
	}
}
