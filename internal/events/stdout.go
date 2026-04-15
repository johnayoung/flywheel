package events

import (
	"fmt"
	"io"
	"strings"
	"sync"
)

// StdoutReporter writes one human-readable line per event to an io.Writer.
// agent_output_chunk events are only printed when verbose=true.
type StdoutReporter struct {
	w       io.Writer
	verbose bool
	mu      sync.Mutex
}

// NewStdoutReporter wires a reporter to the given writer.
func NewStdoutReporter(w io.Writer, verbose bool) *StdoutReporter {
	return &StdoutReporter{w: w, verbose: verbose}
}

// Handle implements Subscriber.
func (r *StdoutReporter) Handle(e Event) {
	line := r.format(e)
	if line == "" {
		return
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	fmt.Fprintln(r.w, line)
}

func (r *StdoutReporter) format(e Event) string {
	switch e.Type {
	case TypeRunStarted:
		return fmt.Sprintf("[run %s] started", e.RunID)
	case TypeRunCompleted:
		merged, _ := e.Data["merged"].(int)
		failed, _ := e.Data["failed"].(int)
		pending, _ := e.Data["pending"].(int)
		return fmt.Sprintf("[run %s] complete: merged=%d failed=%d pending=%d", e.RunID, merged, failed, pending)
	case TypeTaskClaimed:
		return fmt.Sprintf("%s claimed task=%s", prefix(e), e.TaskID)
	case TypeTaskPhaseChanged:
		from, _ := e.Data["from"].(string)
		to, _ := e.Data["to"].(string)
		return fmt.Sprintf("%s task=%s phase %s -> %s", prefix(e), e.TaskID, from, to)
	case TypeTaskAttemptStarted:
		return fmt.Sprintf("%s task=%s attempt=%d started", prefix(e), e.TaskID, e.Attempt)
	case TypeTaskAttemptEnded:
		outcome, _ := e.Data["outcome"].(string)
		base := fmt.Sprintf("%s task=%s attempt=%d ended outcome=%s", prefix(e), e.TaskID, e.Attempt, outcome)
		if detail, _ := e.Data["detail"].(string); detail != "" {
			// Collapse multi-line details to a single line so each event stays
			// on one log line. Trim to keep the terminal readable; the full
			// text is still on the lifecycle record.
			detail = strings.ReplaceAll(strings.TrimSpace(detail), "\n", " \u21b5 ")
			const max = 500
			if len(detail) > max {
				detail = detail[:max] + "..."
			}
			return base + " detail=" + detail
		}
		return base
	case TypePlanSummary:
		total, _ := asInt(e.Data["total"])
		ready, _ := asInt(e.Data["ready"])
		blocked, _ := asInt(e.Data["blocked"])
		failed, _ := asInt(e.Data["failed"])
		merged, _ := asInt(e.Data["merged"])
		return fmt.Sprintf("[run %s] plan: total=%d ready=%d blocked=%d failed=%d merged=%d",
			e.RunID, total, ready, blocked, failed, merged)
	case TypeTaskAutoReset:
		prior, _ := asInt(e.Data["prior_run_failures"])
		reason, _ := e.Data["reason"].(string)
		return fmt.Sprintf("[engine] task=%s auto-reset (reason=%s prior_run_failures=%d)",
			e.TaskID, reason, prior)
	case TypeRunHeartbeat:
		workers, _ := e.Data["workers"].([]map[string]any)
		if len(workers) == 0 {
			return fmt.Sprintf("[run %s] heartbeat: idle", e.RunID)
		}
		parts := make([]string, 0, len(workers))
		for _, w := range workers {
			wid, _ := w["worker_id"].(string)
			tid, _ := w["task_id"].(string)
			phase, _ := w["phase"].(string)
			parts = append(parts, fmt.Sprintf("%s=%s/%s", wid, tid, phase))
		}
		return fmt.Sprintf("[run %s] heartbeat: %s", e.RunID, strings.Join(parts, " "))
	case TypeRunDeadlocked:
		blocked, _ := e.Data["blocked_tasks"].([]string)
		failed, _ := e.Data["failed_tasks"].([]string)
		return fmt.Sprintf("[run %s] DEADLOCKED: %d tasks blocked by %d failed prereqs (%s); use 'flywheel retry --all-failed --force' to reset",
			e.RunID, len(blocked), len(failed), strings.Join(failed, ","))
	case TypeAgentOutputChunk:
		if !r.verbose {
			return ""
		}
		text, _ := e.Data["text"].(string)
		text = strings.TrimRight(text, "\n")
		if text == "" {
			return ""
		}
		return fmt.Sprintf("%s task=%s | %s", prefix(e), e.TaskID, text)
	}
	return ""
}

// asInt coerces a numeric value from map[string]any (which may carry int,
// int64, or float64 depending on how the event was constructed) to int.
func asInt(v any) (int, bool) {
	switch n := v.(type) {
	case int:
		return n, true
	case int64:
		return int(n), true
	case float64:
		return int(n), true
	}
	return 0, false
}

func prefix(e Event) string {
	if e.WorkerID != "" {
		return "[" + e.WorkerID + "]"
	}
	return "[engine]"
}
