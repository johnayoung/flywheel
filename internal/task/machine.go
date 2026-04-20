package task

import (
	"fmt"
	"time"
)

var validTransitions = map[Status][]Status{
	StatusPending:          {StatusReady},
	StatusReady:            {StatusRunning},
	StatusRunning:          {StatusValidating, StatusFailedValidation, StatusFailed, StatusInterrupted},
	StatusValidating:       {StatusDone, StatusFailedValidation, StatusFailed, StatusInterrupted},
	StatusFailedValidation: {StatusReady, StatusFailed},
	StatusInterrupted:      {StatusReady},
	StatusDone:             {},
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
	return s == StatusDone || s == StatusFailed
}

// CanRetry reports whether the lifecycle is eligible for retry.
func CanRetry(lc *Lifecycle, maxRetries int) bool {
	if lc.Retries >= maxRetries {
		return false
	}
	return lc.Status == StatusFailedValidation
}

// Transition moves the lifecycle to a new status, applying side effects.
func Transition(lc *Lifecycle, to Status) error {
	if !CanTransition(lc.Status, to) {
		return fmt.Errorf("invalid transition from %s to %s", lc.Status, to)
	}

	if (to == StatusFailed || to == StatusFailedValidation) && lc.Error == "" {
		return fmt.Errorf("transition to %s requires Error to be set", to)
	}

	now := time.Now()
	from := lc.Status

	if to == StatusReady && from == StatusFailedValidation {
		lc.Retries++
		lc.Error = ""
	}
	if to == StatusReady && from == StatusInterrupted {
		lc.Error = ""
	}

	switch to {
	case StatusReady:
		lc.Timestamps.ReadyAt = &now
	case StatusRunning:
		lc.Timestamps.StartedAt = &now
	case StatusValidating:
		lc.Timestamps.CompletedAt = &now
	case StatusDone:
		lc.Timestamps.CompletedAt = &now
	case StatusFailed, StatusFailedValidation:
		lc.Timestamps.FailedAt = &now
	}

	lc.Status = to

	return nil
}
