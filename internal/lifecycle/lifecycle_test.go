package lifecycle

import "testing"

func TestConsecutiveFailedRuns_EmptyAndNilSafe(t *testing.T) {
	if got := ConsecutiveFailedRuns(nil); got != 0 {
		t.Errorf("nil: got %d, want 0", got)
	}
	lc := &Lifecycle{}
	if got := ConsecutiveFailedRuns(lc); got != 0 {
		t.Errorf("empty: got %d, want 0", got)
	}
}

func TestConsecutiveFailedRuns_SingleRun(t *testing.T) {
	lc := &Lifecycle{Attempts: []Attempt{
		{Number: 1, Outcome: OutcomeAgentError, RunID: "run-A"},
	}}
	if got := ConsecutiveFailedRuns(lc); got != 1 {
		t.Errorf("got %d, want 1", got)
	}
}

func TestConsecutiveFailedRuns_MultipleRuns(t *testing.T) {
	lc := &Lifecycle{Attempts: []Attempt{
		{Number: 1, Outcome: OutcomeAgentError, RunID: "run-A"},
		{Number: 2, Outcome: OutcomeValidationFailed, RunID: "run-B"},
		{Number: 3, Outcome: OutcomeRejected, RunID: "run-C"},
	}}
	if got := ConsecutiveFailedRuns(lc); got != 3 {
		t.Errorf("got %d, want 3", got)
	}
}

func TestConsecutiveFailedRuns_WithinRunCollapses(t *testing.T) {
	// Two failed attempts sharing a RunID count as one run.
	lc := &Lifecycle{Attempts: []Attempt{
		{Number: 1, Outcome: OutcomeValidationFailed, RunID: "run-A"},
		{Number: 2, Outcome: OutcomeValidationFailed, RunID: "run-A"},
	}}
	if got := ConsecutiveFailedRuns(lc); got != 1 {
		t.Errorf("got %d, want 1", got)
	}
}

func TestConsecutiveFailedRuns_StopsAtSuccess(t *testing.T) {
	lc := &Lifecycle{Attempts: []Attempt{
		{Number: 1, Outcome: OutcomeAgentError, RunID: "run-A"},
		{Number: 2, Outcome: OutcomeSucceeded, RunID: "run-B"},
		{Number: 3, Outcome: OutcomeAgentError, RunID: "run-C"},
	}}
	// Only the trailing run-C counts; run-B was success.
	if got := ConsecutiveFailedRuns(lc); got != 1 {
		t.Errorf("got %d, want 1", got)
	}
}

func TestConsecutiveFailedRuns_StopsAtCancelled(t *testing.T) {
	// Cancelled is not a failure outcome.
	lc := &Lifecycle{Attempts: []Attempt{
		{Number: 1, Outcome: OutcomeAgentError, RunID: "run-A"},
		{Number: 2, Outcome: OutcomeCancelled, RunID: "run-B"},
	}}
	if got := ConsecutiveFailedRuns(lc); got != 0 {
		t.Errorf("got %d, want 0 (cancelled breaks the streak)", got)
	}
}

func TestConsecutiveFailedRuns_LegacyCollapse(t *testing.T) {
	// Pre-upgrade attempts have empty RunID; they collapse into one run.
	lc := &Lifecycle{Attempts: []Attempt{
		{Number: 1, Outcome: OutcomeAgentError},
		{Number: 2, Outcome: OutcomeValidationFailed},
		{Number: 3, Outcome: OutcomeRejected},
	}}
	if got := ConsecutiveFailedRuns(lc); got != 1 {
		t.Errorf("got %d, want 1 (legacy collapses)", got)
	}
}

func TestConsecutiveFailedRuns_LegacyThenModern(t *testing.T) {
	lc := &Lifecycle{Attempts: []Attempt{
		{Number: 1, Outcome: OutcomeAgentError},
		{Number: 2, Outcome: OutcomeAgentError, RunID: "run-A"},
	}}
	// Legacy (1 run) + modern run-A = 2 runs.
	if got := ConsecutiveFailedRuns(lc); got != 2 {
		t.Errorf("got %d, want 2", got)
	}
}

func TestIsFailureOutcome(t *testing.T) {
	failing := []string{
		OutcomeAgentError, OutcomeValidationFailed, OutcomeRejected,
		OutcomeMergeConflictFail, OutcomeMergeError, OutcomeInternalError,
	}
	for _, o := range failing {
		if !IsFailureOutcome(o) {
			t.Errorf("IsFailureOutcome(%q) = false, want true", o)
		}
	}
	nonFailing := []string{OutcomeSucceeded, OutcomeCancelled, "", "unknown"}
	for _, o := range nonFailing {
		if IsFailureOutcome(o) {
			t.Errorf("IsFailureOutcome(%q) = true, want false", o)
		}
	}
}

func TestResetForRetry_ClearsMutableFieldsPreservesAttempts(t *testing.T) {
	lc := &Lifecycle{
		Error:           "boom",
		ReviewResult:    "rejected",
		ConflictDetails: "file.go",
		ResolveAttempts: 2,
		Attempts: []Attempt{
			{Number: 1, Outcome: OutcomeAgentError, RunID: "run-A"},
		},
	}
	ResetForRetry(lc)
	if lc.Error != "" {
		t.Errorf("Error not cleared: %q", lc.Error)
	}
	if lc.ReviewResult != "" {
		t.Errorf("ReviewResult not cleared: %q", lc.ReviewResult)
	}
	if lc.ConflictDetails != "" {
		t.Errorf("ConflictDetails not cleared: %q", lc.ConflictDetails)
	}
	if lc.ResolveAttempts != 0 {
		t.Errorf("ResolveAttempts not cleared: %d", lc.ResolveAttempts)
	}
	if len(lc.Attempts) != 1 {
		t.Errorf("Attempts history lost: %d entries", len(lc.Attempts))
	}
}
