package events

import (
	"bytes"
	"strings"
	"testing"
)

func TestStdoutReporter_PhaseChange(t *testing.T) {
	var buf bytes.Buffer
	r := NewStdoutReporter(&buf, false)
	r.Handle(Event{
		Type:     TypeTaskPhaseChanged,
		TaskID:   "task-1",
		WorkerID: "worker-0",
		Attempt:  1,
		Data:     map[string]any{"from": "running", "to": "validating"},
	})
	got := buf.String()
	if !strings.Contains(got, "[worker-0]") {
		t.Errorf("expected worker prefix, got: %q", got)
	}
	if !strings.Contains(got, "running -> validating") {
		t.Errorf("expected phase transition text, got: %q", got)
	}
}

func TestStdoutReporter_AgentChunkGatedByVerbose(t *testing.T) {
	var buf bytes.Buffer
	r := NewStdoutReporter(&buf, false)
	r.Handle(Event{
		Type:   TypeAgentOutputChunk,
		TaskID: "task-1",
		Data:   map[string]any{"text": "hello"},
	})
	if buf.Len() != 0 {
		t.Errorf("non-verbose reporter must not print agent chunks, got: %q", buf.String())
	}

	buf.Reset()
	r = NewStdoutReporter(&buf, true)
	r.Handle(Event{
		Type:   TypeAgentOutputChunk,
		TaskID: "task-1",
		Data:   map[string]any{"text": "hello"},
	})
	if !strings.Contains(buf.String(), "hello") {
		t.Errorf("verbose reporter must print agent chunks, got: %q", buf.String())
	}
}

func TestStdoutReporter_AttemptLifecycle(t *testing.T) {
	var buf bytes.Buffer
	r := NewStdoutReporter(&buf, false)
	r.Handle(Event{Type: TypeTaskAttemptStarted, TaskID: "t", WorkerID: "w-1", Attempt: 2})
	r.Handle(Event{Type: TypeTaskAttemptEnded, TaskID: "t", WorkerID: "w-1", Attempt: 2, Data: map[string]any{"outcome": "succeeded"}})
	out := buf.String()
	if !strings.Contains(out, "attempt=2 started") {
		t.Errorf("missing attempt started line: %q", out)
	}
	if !strings.Contains(out, "outcome=succeeded") {
		t.Errorf("missing attempt ended line: %q", out)
	}
}
