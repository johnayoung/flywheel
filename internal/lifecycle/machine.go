package lifecycle

import (
	"fmt"
	"time"
)

var validTransitions = map[Status][]Status{
	StatusPending:          {StatusReady},
	StatusReady:            {StatusRunning},
	StatusRunning:          {StatusValidating, StatusFailedValidation, StatusFailed, StatusReady, StatusInterrupted},
	StatusValidating:       {StatusReviewing, StatusFailedValidation, StatusReady, StatusInterrupted},
	StatusFailedValidation: {StatusReady, StatusFailed},
	StatusReviewing:        {StatusMerging, StatusRejected, StatusReady, StatusInterrupted},
	StatusRejected:         {StatusReady, StatusFailed},
	StatusMerging:          {StatusMerged, StatusConflict, StatusReady, StatusInterrupted},
	StatusConflict:         {StatusResolving, StatusInterrupted},
	StatusResolving:        {StatusMerging, StatusFailed, StatusReady, StatusInterrupted},
	StatusInterrupted:      {StatusReady},
	StatusMerged:           {},
	StatusFailed:           {},
}

// CanTransition reports whether a transition from one status to another is valid.
func CanTransition(from, to Status) bool {
	targets, ok := validTransitions[from]
	if !ok {
		return false
	}
	for _, t := range targets {
		if t == to {
			return true
		}
	}
	return false
}

// IsTerminal reports whether the given status is a terminal state.
func IsTerminal(s Status) bool {
	return s == StatusMerged || s == StatusFailed
}

// CanRetry reports whether the lifecycle is eligible for retry.
// Only failed_validation and rejected are retryable; failed is terminal and not retryable.
func CanRetry(lc *Lifecycle, maxRetries int) bool {
	if lc.Retries >= maxRetries {
		return false
	}
	return lc.Status == StatusFailedValidation || lc.Status == StatusRejected
}

// Transition moves the lifecycle to a new status, applying side effects.
func Transition(lc *Lifecycle, to Status) error {
	if !CanTransition(lc.Status, to) {
		return fmt.Errorf("invalid transition from %s to %s", lc.Status, to)
	}

	// Transitioning to failed or failed_validation requires Error to be set beforehand.
	if (to == StatusFailed || to == StatusFailedValidation) && lc.Error == "" {
		return fmt.Errorf("transition to %s requires Error to be set", to)
	}

	now := time.Now()
	from := lc.Status

	// Retry side effects: moving from a retryable state back to ready.
	if to == StatusReady && (from == StatusFailedValidation || from == StatusRejected) {
		lc.Retries++
		lc.Error = ""
	}
	// Resume side effect: returning from interrupted does NOT consume the
	// retry budget (the prior attempt was cancelled, not failed).
	if to == StatusReady && from == StatusInterrupted {
		lc.Error = ""
	}

	// Set timestamps based on target state.
	switch to {
	case StatusReady:
		lc.Timestamps.ReadyAt = &now
	case StatusRunning:
		lc.Timestamps.StartedAt = &now
	case StatusReviewing:
		lc.Timestamps.ReviewedAt = &now
	case StatusMerged:
		lc.Timestamps.MergedAt = &now
	case StatusFailed, StatusFailedValidation:
		lc.Timestamps.FailedAt = &now
	case StatusValidating:
		lc.Timestamps.CompletedAt = &now
	}

	lc.Status = to

	return nil
}
