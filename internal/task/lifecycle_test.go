package task

import (
	"encoding/json"
	"testing"
)

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
	if got := ConsecutiveFailedRuns(lc); got != 1 {
		t.Errorf("got %d, want 1", got)
	}
}

func TestConsecutiveFailedRuns_StopsAtCancelled(t *testing.T) {
	lc := &Lifecycle{Attempts: []Attempt{
		{Number: 1, Outcome: OutcomeAgentError, RunID: "run-A"},
		{Number: 2, Outcome: OutcomeCancelled, RunID: "run-B"},
	}}
	if got := ConsecutiveFailedRuns(lc); got != 0 {
		t.Errorf("got %d, want 0 (cancelled breaks the streak)", got)
	}
}

func TestConsecutiveFailedRuns_LegacyCollapse(t *testing.T) {
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

func TestSessionIDRoundTrip(t *testing.T) {
	lc := &Lifecycle{
		TaskID:    "T-001",
		SessionID: "sess-abc123",
	}
	data, err := json.Marshal(lc)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got Lifecycle
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if got.SessionID != "sess-abc123" {
		t.Errorf("SessionID = %q, want %q", got.SessionID, "sess-abc123")
	}
}

func TestArtifactsDirRoundTrip(t *testing.T) {
	lc := &Lifecycle{
		TaskID:       "T-001",
		ArtifactsDir: ".flywheel/artifacts/T-001",
	}
	data, err := json.Marshal(lc)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	var got Lifecycle
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if got.ArtifactsDir != ".flywheel/artifacts/T-001" {
		t.Errorf("ArtifactsDir = %q, want %q", got.ArtifactsDir, ".flywheel/artifacts/T-001")
	}
}

func TestAttemptDir(t *testing.T) {
	lc := &Lifecycle{ArtifactsDir: ".flywheel/artifacts/T-001"}
	if got := lc.AttemptDir(1); got != ".flywheel/artifacts/T-001/attempt-1/" {
		t.Errorf("AttemptDir(1) = %q", got)
	}
	if got := lc.AttemptDir(3); got != ".flywheel/artifacts/T-001/attempt-3/" {
		t.Errorf("AttemptDir(3) = %q", got)
	}
}

func TestInterruptedToReadyPreservesSessionID(t *testing.T) {
	lc := NewLifecycle("T-001", "run-1", "main")
	lc.SessionID = "sess-xyz"
	lc.Status = StatusRunning
	if err := Transition(lc, StatusInterrupted); err != nil {
		t.Fatalf("transition to interrupted: %v", err)
	}
	if err := Transition(lc, StatusReady); err != nil {
		t.Fatalf("transition to ready: %v", err)
	}
	if lc.SessionID != "sess-xyz" {
		t.Errorf("SessionID cleared after interrupted->ready: got %q", lc.SessionID)
	}
}

func TestResetForRetryPreservesSessionID(t *testing.T) {
	lc := &Lifecycle{
		SessionID:    "sess-abc",
		Error:        "some error",
		ReviewResult: "rejected",
	}
	ResetForRetry(lc)
	if lc.SessionID != "sess-abc" {
		t.Errorf("SessionID cleared by ResetForRetry: got %q", lc.SessionID)
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
