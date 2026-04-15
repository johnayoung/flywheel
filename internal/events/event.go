// Package events defines the in-process event bus used by the engine to
// broadcast lifecycle progress to subscribers (stdout reporter, JSONL log,
// future TUIs). The bus is synchronous: subscribers must be fast and must
// not call back into the engine.
package events

import "time"

// Event is the canonical record emitted by the engine. Data carries
// type-specific payload (e.g., from/to phase, outcome, chunk text).
type Event struct {
	Timestamp time.Time      `json:"ts"`
	RunID     string         `json:"run_id"`
	TaskID    string         `json:"task_id,omitempty"`
	WorkerID  string         `json:"worker_id,omitempty"`
	Type      string         `json:"type"`
	Attempt   int            `json:"attempt,omitempty"`
	Data      map[string]any `json:"data,omitempty"`
}

// Event type constants.
const (
	TypeRunStarted         = "run_started"
	TypeRunCompleted       = "run_completed"
	TypeTaskClaimed        = "task_claimed"
	TypeTaskPhaseChanged   = "task_phase_changed"
	TypeTaskAttemptStarted = "task_attempt_started"
	TypeTaskAttemptEnded   = "task_attempt_ended"
	TypeAgentOutputChunk   = "agent_output_chunk"
	TypeRunHeartbeat       = "run_heartbeat"
	TypePlanSummary        = "plan_summary"
	TypeTaskAutoReset      = "task_auto_reset"
	TypeRunDeadlocked      = "run_deadlocked"
)

// Publisher is implemented by anything that emits events.
type Publisher interface {
	Publish(Event)
}

// Subscriber receives events. Handle must not block on long operations.
type Subscriber interface {
	Handle(Event)
}

// NopPublisher is a Publisher that drops every event. Useful for tests
// and as a default when no real bus is wired.
type NopPublisher struct{}

// Publish discards the event.
func (NopPublisher) Publish(Event) {}
