package task

import (
	"testing"
)

func TestCanTransition_AllValid(t *testing.T) {
	for from, targets := range validTransitions {
		for _, to := range targets {
			if !CanTransition(from, to) {
				t.Errorf("expected CanTransition(%s, %s) to be true", from, to)
			}
		}
	}
}

func TestCanTransition_Invalid(t *testing.T) {
	invalid := []struct {
		from, to Status
	}{
		{StatusPending, StatusRunning},
		{StatusPending, StatusFailed},
		{StatusMerged, StatusReady},
		{StatusMerged, StatusFailed},
		{StatusFailed, StatusReady},
		{StatusFailed, StatusPending},
		{StatusReady, StatusValidating},
		{StatusRunning, StatusMerging},
	}
	for _, tc := range invalid {
		if CanTransition(tc.from, tc.to) {
			t.Errorf("expected CanTransition(%s, %s) to be false", tc.from, tc.to)
		}
	}
}

func TestCanTransition_UnknownStatus(t *testing.T) {
	if CanTransition(Status("unknown"), StatusReady) {
		t.Error("expected CanTransition from unknown status to be false")
	}
}

func TestIsTerminal(t *testing.T) {
	terminals := []Status{StatusMerged, StatusFailed}
	for _, s := range terminals {
		if !IsTerminal(s) {
			t.Errorf("expected IsTerminal(%s) to be true", s)
		}
	}

	nonTerminals := []Status{
		StatusPending, StatusReady, StatusRunning, StatusValidating,
		StatusFailedValidation, StatusReviewing, StatusRejected,
		StatusMerging, StatusConflict, StatusResolving,
	}
	for _, s := range nonTerminals {
		if IsTerminal(s) {
			t.Errorf("expected IsTerminal(%s) to be false", s)
		}
	}
}

func TestCanRetry(t *testing.T) {
	t.Run("failed_validation with retries remaining", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		lc.Status = StatusFailedValidation
		lc.Retries = 0
		if !CanRetry(lc, 3) {
			t.Error("expected CanRetry to be true")
		}
	})

	t.Run("rejected with retries remaining", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		lc.Status = StatusRejected
		lc.Retries = 1
		if !CanRetry(lc, 3) {
			t.Error("expected CanRetry to be true")
		}
	})

	t.Run("failed_validation at max retries", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		lc.Status = StatusFailedValidation
		lc.Retries = 3
		if CanRetry(lc, 3) {
			t.Error("expected CanRetry to be false when at max retries")
		}
	})

	t.Run("failed is not retryable", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		lc.Status = StatusFailed
		lc.Retries = 0
		if CanRetry(lc, 3) {
			t.Error("expected CanRetry to be false for terminal failed status")
		}
	})

	t.Run("running is not retryable", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		lc.Status = StatusRunning
		lc.Retries = 0
		if CanRetry(lc, 3) {
			t.Error("expected CanRetry to be false for running status")
		}
	})
}

func TestTransition_HappyPath(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")

	steps := []Status{
		StatusReady, StatusRunning, StatusValidating,
		StatusReviewing, StatusMerging, StatusMerged,
	}

	for _, to := range steps {
		if err := Transition(lc, to); err != nil {
			t.Fatalf("unexpected error transitioning to %s: %v", to, err)
		}
		if lc.Status != to {
			t.Fatalf("expected status %s, got %s", to, lc.Status)
		}
	}

	if lc.Version != 0 {
		t.Errorf("expected Transition to leave Version untouched, got %d", lc.Version)
	}
}

func TestTransition_InvalidTransition(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")

	if err := Transition(lc, StatusRunning); err == nil {
		t.Error("expected error for invalid transition pending -> running")
	}
	if lc.Status != StatusPending {
		t.Error("status should not change on invalid transition")
	}
	if lc.Version != 0 {
		t.Error("version should not change on invalid transition")
	}
}

func TestTransition_TerminalStatesRejectAll(t *testing.T) {
	allStatuses := []Status{
		StatusPending, StatusReady, StatusRunning, StatusValidating,
		StatusFailedValidation, StatusReviewing, StatusRejected,
		StatusMerging, StatusConflict, StatusResolving, StatusMerged, StatusFailed,
	}

	for _, terminal := range []Status{StatusMerged, StatusFailed} {
		for _, to := range allStatuses {
			lc := NewLifecycle("t1", "r1", "main")
			lc.Status = terminal
			if err := Transition(lc, to); err == nil {
				t.Errorf("expected error transitioning from terminal %s to %s", terminal, to)
			}
		}
	}
}

func TestTransition_RetryFromFailedValidation(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")
	lc.Status = StatusFailedValidation
	lc.Error = "validation failed"
	lc.Retries = 0

	if err := Transition(lc, StatusReady); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if lc.Retries != 1 {
		t.Errorf("expected retries 1, got %d", lc.Retries)
	}
	if lc.Error != "" {
		t.Errorf("expected error to be cleared, got %q", lc.Error)
	}
	if lc.Status != StatusReady {
		t.Errorf("expected status ready, got %s", lc.Status)
	}
}

func TestTransition_RetryFromRejected(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")
	lc.Status = StatusRejected
	lc.Error = "review rejected"
	lc.Retries = 2

	if err := Transition(lc, StatusReady); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if lc.Retries != 3 {
		t.Errorf("expected retries 3, got %d", lc.Retries)
	}
	if lc.Error != "" {
		t.Errorf("expected error to be cleared, got %q", lc.Error)
	}
}

func TestTransition_CrashRecovery(t *testing.T) {
	recoverable := []Status{
		StatusRunning, StatusValidating, StatusReviewing, StatusMerging, StatusResolving,
	}
	for _, from := range recoverable {
		lc := NewLifecycle("t1", "r1", "main")
		lc.Status = from
		if err := Transition(lc, StatusReady); err != nil {
			t.Errorf("expected crash recovery %s -> ready to succeed, got: %v", from, err)
		}
		if lc.Status != StatusReady {
			t.Errorf("expected status ready after recovery from %s, got %s", from, lc.Status)
		}
	}
}

func TestTransition_Timestamps(t *testing.T) {
	t.Run("ready sets ReadyAt", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		_ = Transition(lc, StatusReady)
		if lc.Timestamps.ReadyAt == nil {
			t.Error("expected ReadyAt to be set")
		}
	})

	t.Run("running sets StartedAt", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		_ = Transition(lc, StatusReady)
		_ = Transition(lc, StatusRunning)
		if lc.Timestamps.StartedAt == nil {
			t.Error("expected StartedAt to be set")
		}
	})

	t.Run("validating sets CompletedAt", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		_ = Transition(lc, StatusReady)
		_ = Transition(lc, StatusRunning)
		_ = Transition(lc, StatusValidating)
		if lc.Timestamps.CompletedAt == nil {
			t.Error("expected CompletedAt to be set")
		}
	})

	t.Run("reviewing sets ReviewedAt", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		_ = Transition(lc, StatusReady)
		_ = Transition(lc, StatusRunning)
		_ = Transition(lc, StatusValidating)
		_ = Transition(lc, StatusReviewing)
		if lc.Timestamps.ReviewedAt == nil {
			t.Error("expected ReviewedAt to be set")
		}
	})

	t.Run("merged sets MergedAt", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		_ = Transition(lc, StatusReady)
		_ = Transition(lc, StatusRunning)
		_ = Transition(lc, StatusValidating)
		_ = Transition(lc, StatusReviewing)
		_ = Transition(lc, StatusMerging)
		_ = Transition(lc, StatusMerged)
		if lc.Timestamps.MergedAt == nil {
			t.Error("expected MergedAt to be set")
		}
	})

	t.Run("failed sets FailedAt", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		_ = Transition(lc, StatusReady)
		_ = Transition(lc, StatusRunning)
		lc.Error = "something broke"
		_ = Transition(lc, StatusFailed)
		if lc.Timestamps.FailedAt == nil {
			t.Error("expected FailedAt to be set")
		}
	})

	t.Run("failed_validation sets FailedAt", func(t *testing.T) {
		lc := NewLifecycle("t1", "r1", "main")
		_ = Transition(lc, StatusReady)
		_ = Transition(lc, StatusRunning)
		_ = Transition(lc, StatusValidating)
		lc.Error = "validation error"
		_ = Transition(lc, StatusFailedValidation)
		if lc.Timestamps.FailedAt == nil {
			t.Error("expected FailedAt to be set")
		}
	})
}

func TestTransition_FailedRequiresError(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")
	_ = Transition(lc, StatusReady)
	_ = Transition(lc, StatusRunning)

	err := Transition(lc, StatusFailed)
	if err == nil {
		t.Error("expected error when transitioning to failed without Error set")
	}
	if lc.Status != StatusRunning {
		t.Error("status should not change when transition is rejected")
	}

	lc.Error = "crash"
	if err := Transition(lc, StatusFailed); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if lc.Status != StatusFailed {
		t.Error("expected status to be failed")
	}
}

func TestTransition_FailedValidationRequiresError(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")
	_ = Transition(lc, StatusReady)
	_ = Transition(lc, StatusRunning)
	_ = Transition(lc, StatusValidating)

	err := Transition(lc, StatusFailedValidation)
	if err == nil {
		t.Error("expected error when transitioning to failed_validation without Error set")
	}
	if lc.Status != StatusValidating {
		t.Error("status should not change when transition is rejected")
	}

	lc.Error = "lint failed"
	if err := Transition(lc, StatusFailedValidation); err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if lc.Status != StatusFailedValidation {
		t.Error("expected status to be failed_validation")
	}
}

func TestTransition_DoesNotModifyVersion(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")
	if lc.Version != 0 {
		t.Fatalf("expected initial version 0, got %d", lc.Version)
	}
	_ = Transition(lc, StatusReady)
	if lc.Version != 0 {
		t.Errorf("Transition must not touch Version, got %d", lc.Version)
	}
	_ = Transition(lc, StatusRunning)
	if lc.Version != 0 {
		t.Errorf("Transition must not touch Version, got %d", lc.Version)
	}
}

func TestTransition_InterruptedDoesNotConsumeRetryBudget(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")
	_ = Transition(lc, StatusReady)
	_ = Transition(lc, StatusRunning)

	if err := Transition(lc, StatusInterrupted); err != nil {
		t.Fatalf("Transition Running -> Interrupted: %v", err)
	}
	if lc.Retries != 0 {
		t.Errorf("Interrupted must not bump Retries, got %d", lc.Retries)
	}

	if err := Transition(lc, StatusReady); err != nil {
		t.Fatalf("Transition Interrupted -> Ready: %v", err)
	}
	if lc.Retries != 0 {
		t.Errorf("Resuming from Interrupted must not bump Retries, got %d", lc.Retries)
	}
	if lc.Error != "" {
		t.Errorf("expected Error cleared on resume, got %q", lc.Error)
	}
}

func TestTransition_RetryFromValidationStillConsumesBudget(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")
	_ = Transition(lc, StatusReady)
	_ = Transition(lc, StatusRunning)
	_ = Transition(lc, StatusValidating)
	lc.Error = "build failed"
	_ = Transition(lc, StatusFailedValidation)
	_ = Transition(lc, StatusReady)
	if lc.Retries != 1 {
		t.Errorf("FailedValidation -> Ready must bump Retries to 1, got %d", lc.Retries)
	}
}

func TestTransition_ConflictResolutionPath(t *testing.T) {
	lc := NewLifecycle("t1", "r1", "main")
	_ = Transition(lc, StatusReady)
	_ = Transition(lc, StatusRunning)
	_ = Transition(lc, StatusValidating)
	_ = Transition(lc, StatusReviewing)
	_ = Transition(lc, StatusMerging)
	_ = Transition(lc, StatusConflict)
	_ = Transition(lc, StatusResolving)
	_ = Transition(lc, StatusMerging)
	_ = Transition(lc, StatusMerged)

	if lc.Status != StatusMerged {
		t.Errorf("expected merged, got %s", lc.Status)
	}
}
