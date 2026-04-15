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
	StatusInterrupted      Status = "interrupted"
)

// Attempt outcomes recorded in Attempt.Outcome.
const (
	OutcomeSucceeded         = "succeeded"
	OutcomeValidationFailed  = "validation_failed"
	OutcomeRejected          = "rejected"
	OutcomeAgentError        = "agent_error"
	OutcomeCancelled         = "cancelled"
	OutcomeMergeConflictFail = "merge_conflict_failed"
	OutcomeMergeError        = "merge_error"
	OutcomeInternalError     = "internal_error"
)

// Attempt records a single end-to-end execution attempt at a task.
// Each retry appends a new Attempt; existing attempts are never mutated
// after their EndedAt is set.
type Attempt struct {
	Number             int        `json:"number"`
	StartedAt          time.Time  `json:"started_at"`
	EndedAt            *time.Time `json:"ended_at,omitempty"`
	Outcome            string     `json:"outcome,omitempty"`
	AgentOutput        string     `json:"agent_output,omitempty"`
	Error              string     `json:"error,omitempty"`
	ValidationFailures string     `json:"validation_failures,omitempty"`
	ReviewFeedback     string     `json:"review_feedback,omitempty"`
	// RunID is the engine run under which this attempt executed. Populated
	// on all attempts written after the cross-run failure-cap upgrade;
	// legacy attempts have an empty RunID.
	RunID string `json:"run_id,omitempty"`
}

// IsFailureOutcome reports whether an attempt outcome constitutes a failed
// end-to-end run of the task. Cancellation is explicitly not a failure: the
// attempt was interrupted, not rejected by the pipeline.
func IsFailureOutcome(o string) bool {
	switch o {
	case OutcomeAgentError,
		OutcomeValidationFailed,
		OutcomeRejected,
		OutcomeMergeConflictFail,
		OutcomeMergeError,
		OutcomeInternalError:
		return true
	}
	return false
}

// ConsecutiveFailedRuns walks lc.Attempts from the end backward and returns
// the count of consecutive *runs* that ended in failure. A "run" is a set of
// contiguous failing attempts sharing the same RunID. The walk stops at the
// first non-failure attempt. Attempts with empty RunID (legacy, pre-upgrade)
// collapse into a single "legacy" run so historical state doesn't burn the
// cap on the first post-upgrade start.
func ConsecutiveFailedRuns(lc *Lifecycle) int {
	if lc == nil || len(lc.Attempts) == 0 {
		return 0
	}
	count := 0
	var lastRunID string
	seenLegacy := false
	for i := len(lc.Attempts) - 1; i >= 0; i-- {
		a := lc.Attempts[i]
		if !IsFailureOutcome(a.Outcome) {
			break
		}
		if a.RunID == "" {
			// Collapse any streak of legacy attempts into one run.
			if !seenLegacy {
				count++
				seenLegacy = true
				lastRunID = ""
			}
			continue
		}
		if a.RunID != lastRunID {
			count++
			lastRunID = a.RunID
			seenLegacy = false
		}
	}
	return count
}

// ResetForRetry clears the mutable top-level error/feedback fields on the
// lifecycle so the next attempt starts from a clean slate. Attempts history
// is preserved (it is the source of truth for cross-run failure counting).
func ResetForRetry(lc *Lifecycle) {
	if lc == nil {
		return
	}
	lc.Error = ""
	lc.ReviewResult = ""
	lc.ConflictDetails = ""
	lc.ResolveAttempts = 0
}

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
	Attempts        []Attempt  `json:"attempts,omitempty"`
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
