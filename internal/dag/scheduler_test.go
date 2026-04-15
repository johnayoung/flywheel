package dag

import (
	"testing"

	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/task"
)

func TestBlockedByFailed_Direct(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1),
		makeTask("B", 2, "A"),
	}
	d, err := Build(tasks)
	if err != nil {
		t.Fatalf("Build: %v", err)
	}
	rc := NewReadinessChecker(d)
	statuses := map[string]lifecycle.Status{
		"A": lifecycle.StatusFailed,
		"B": lifecycle.StatusPending,
	}
	if !rc.BlockedByFailed("B", statuses) {
		t.Error("B should be blocked by failed A")
	}
	if rc.BlockedByFailed("A", statuses) {
		t.Error("A has no prereqs, should not be blocked")
	}
}

func TestBlockedByFailed_Transitive(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1),
		makeTask("B", 2, "A"),
		makeTask("C", 3, "B"),
	}
	d, _ := Build(tasks)
	rc := NewReadinessChecker(d)
	statuses := map[string]lifecycle.Status{
		"A": lifecycle.StatusFailed,
		"B": lifecycle.StatusPending,
		"C": lifecycle.StatusPending,
	}
	if !rc.BlockedByFailed("C", statuses) {
		t.Error("C should be transitively blocked by failed A")
	}
}

func TestBlockedByFailed_Unblocked(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1),
		makeTask("B", 2, "A"),
	}
	d, _ := Build(tasks)
	rc := NewReadinessChecker(d)
	statuses := map[string]lifecycle.Status{
		"A": lifecycle.StatusMerged,
		"B": lifecycle.StatusReady,
	}
	if rc.BlockedByFailed("B", statuses) {
		t.Error("B should not be blocked when A is merged")
	}
}

func TestPlanSummary_MixedStatuses(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1),
		makeTask("B", 2, "A"),
		makeTask("C", 3),
		makeTask("D", 4, "A"),
	}
	d, _ := Build(tasks)
	rc := NewReadinessChecker(d)
	statuses := map[string]lifecycle.Status{
		"A": lifecycle.StatusFailed,
		"B": lifecycle.StatusPending, // blocked
		"C": lifecycle.StatusReady,
		"D": lifecycle.StatusPending, // blocked
	}
	sum := rc.PlanSummary(statuses)
	if sum.Total != 4 {
		t.Errorf("Total: got %d, want 4", sum.Total)
	}
	if sum.Failed != 1 {
		t.Errorf("Failed: got %d, want 1", sum.Failed)
	}
	if sum.Ready != 1 {
		t.Errorf("Ready: got %d, want 1", sum.Ready)
	}
	if sum.Blocked != 2 {
		t.Errorf("Blocked: got %d, want 2", sum.Blocked)
	}
	if sum.Merged != 0 {
		t.Errorf("Merged: got %d, want 0", sum.Merged)
	}
}
