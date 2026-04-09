package dag

import (
	"strings"
	"testing"

	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/task"
)

func makeTask(id string, priority int, prereqs ...string) task.Task {
	return task.Task{
		ID:            id,
		Description:   "Task " + id,
		Category:      "feat",
		Priority:      priority,
		Prerequisites: prereqs,
		Commit:        "implement " + id,
		Steps:         []string{"step 1"},
	}
}

func TestBuild_ValidDAG(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1),
		makeTask("B", 2),
		makeTask("C", 3, "A", "B"),
	}

	d, err := Build(tasks)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	if d.NodeCount() != 3 {
		t.Errorf("expected 3 nodes, got %d", d.NodeCount())
	}

	// A has no prerequisites, two dependents.
	if len(d.Prerequisites("A")) != 0 {
		t.Errorf("expected A to have 0 prerequisites, got %v", d.Prerequisites("A"))
	}
	depA := d.Dependents("A")
	if len(depA) != 1 || depA[0] != "C" {
		t.Errorf("expected A dependents [C], got %v", depA)
	}

	// B has no prerequisites, one dependent.
	depB := d.Dependents("B")
	if len(depB) != 1 || depB[0] != "C" {
		t.Errorf("expected B dependents [C], got %v", depB)
	}

	// C has two prerequisites, no dependents.
	prereqC := d.Prerequisites("C")
	if len(prereqC) != 2 {
		t.Errorf("expected C to have 2 prerequisites, got %v", prereqC)
	}
	if len(d.Dependents("C")) != 0 {
		t.Errorf("expected C to have 0 dependents, got %v", d.Dependents("C"))
	}
}

func TestBuild_DetectCycle(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1, "C"),
		makeTask("B", 2, "A"),
		makeTask("C", 3, "B"),
	}

	_, err := Build(tasks)
	if err == nil {
		t.Fatal("expected cycle error, got nil")
	}
	if !strings.Contains(err.Error(), "cycle detected") {
		t.Errorf("expected 'cycle detected' in error, got: %v", err)
	}
	// Verify the error includes task IDs from the cycle.
	for _, id := range []string{"A", "B", "C"} {
		if !strings.Contains(err.Error(), id) {
			t.Errorf("expected cycle error to mention %q, got: %v", id, err)
		}
	}
}

func TestBuild_MissingPrerequisite(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1, "Z"),
	}

	_, err := Build(tasks)
	if err == nil {
		t.Fatal("expected missing prerequisite error, got nil")
	}
	if !strings.Contains(err.Error(), "Z") {
		t.Errorf("expected error to mention 'Z', got: %v", err)
	}
	if !strings.Contains(err.Error(), "does not exist") {
		t.Errorf("expected error to say 'does not exist', got: %v", err)
	}
}

func TestBuild_DuplicateTaskID(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1),
		makeTask("A", 2),
	}

	_, err := Build(tasks)
	if err == nil {
		t.Fatal("expected duplicate ID error, got nil")
	}
	if !strings.Contains(err.Error(), "duplicate") {
		t.Errorf("expected 'duplicate' in error, got: %v", err)
	}
}

func TestPrerequisites_And_Dependents(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1),
		makeTask("B", 2),
		makeTask("C", 3, "A"),
		makeTask("D", 4, "A", "B"),
	}

	d, err := Build(tasks)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	// C depends on A only.
	prereqC := d.Prerequisites("C")
	if len(prereqC) != 1 || prereqC[0] != "A" {
		t.Errorf("expected C prerequisites [A], got %v", prereqC)
	}

	// D depends on A and B.
	prereqD := d.Prerequisites("D")
	if len(prereqD) != 2 {
		t.Errorf("expected D to have 2 prerequisites, got %v", prereqD)
	}

	// A has dependents C and D.
	depA := d.Dependents("A")
	if len(depA) != 2 {
		t.Errorf("expected A to have 2 dependents, got %v", depA)
	}

	// B has dependent D.
	depB := d.Dependents("B")
	if len(depB) != 1 || depB[0] != "D" {
		t.Errorf("expected B dependents [D], got %v", depB)
	}

	// Non-existent task returns nil.
	if d.Prerequisites("Z") != nil {
		t.Error("expected nil for non-existent task prerequisites")
	}
	if d.Dependents("Z") != nil {
		t.Error("expected nil for non-existent task dependents")
	}
}

func TestRoots(t *testing.T) {
	tasks := []task.Task{
		makeTask("C", 3, "A", "B"),
		makeTask("A", 1),
		makeTask("B", 2),
		makeTask("D", 4, "C"),
	}

	d, err := Build(tasks)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	roots := d.Roots()
	if len(roots) != 2 {
		t.Fatalf("expected 2 roots, got %v", roots)
	}
	// Should be sorted by priority: A(1), B(2).
	if roots[0] != "A" || roots[1] != "B" {
		t.Errorf("expected roots [A, B], got %v", roots)
	}
}

func TestIsReady(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1),
		makeTask("B", 2),
		makeTask("C", 3, "A", "B"),
	}

	d, err := Build(tasks)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	rc := NewReadinessChecker(d)

	// All merged -- C is ready.
	statuses := map[string]lifecycle.Status{
		"A": lifecycle.StatusMerged,
		"B": lifecycle.StatusMerged,
		"C": lifecycle.StatusPending,
	}
	if !rc.IsReady("C", statuses) {
		t.Error("expected C to be ready when A and B are merged")
	}

	// B not merged -- C is not ready.
	statuses["B"] = lifecycle.StatusRunning
	if rc.IsReady("C", statuses) {
		t.Error("expected C to not be ready when B is running")
	}

	// A has no prerequisites, always ready.
	if !rc.IsReady("A", statuses) {
		t.Error("expected A to always be ready (no prerequisites)")
	}
}

func TestReadyTasks_PriorityOrder(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 3),
		makeTask("B", 1),
		makeTask("C", 2),
		makeTask("D", 4, "A"),
	}

	d, err := Build(tasks)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	rc := NewReadinessChecker(d)

	statuses := map[string]lifecycle.Status{
		"A": lifecycle.StatusPending,
		"B": lifecycle.StatusPending,
		"C": lifecycle.StatusPending,
		"D": lifecycle.StatusPending,
	}

	ready := rc.ReadyTasks(statuses)
	// A, B, C are roots (pending, all prereqs met). D depends on A (not merged).
	if len(ready) != 3 {
		t.Fatalf("expected 3 ready tasks, got %v", ready)
	}
	// Sorted by priority: B(1), C(2), A(3).
	if ready[0] != "B" || ready[1] != "C" || ready[2] != "A" {
		t.Errorf("expected ready [B, C, A], got %v", ready)
	}

	// Mark A as merged, now D should become ready.
	statuses["A"] = lifecycle.StatusMerged
	ready = rc.ReadyTasks(statuses)
	// B(1), C(2), D(4) -- A is merged so not pending.
	if len(ready) != 3 {
		t.Fatalf("expected 3 ready tasks, got %v", ready)
	}
	if ready[0] != "B" || ready[1] != "C" || ready[2] != "D" {
		t.Errorf("expected ready [B, C, D], got %v", ready)
	}
}

func TestComputeWaves(t *testing.T) {
	// Graph: A, B independent; C depends on A,B; D depends on C.
	tasks := []task.Task{
		makeTask("A", 2),
		makeTask("B", 1),
		makeTask("C", 1, "A", "B"),
		makeTask("D", 1, "C"),
	}

	d, err := Build(tasks)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	rc := NewReadinessChecker(d)

	statuses := map[string]lifecycle.Status{
		"A": lifecycle.StatusPending,
		"B": lifecycle.StatusPending,
		"C": lifecycle.StatusPending,
		"D": lifecycle.StatusPending,
	}

	waves := rc.ComputeWaves(statuses)
	if len(waves) != 3 {
		t.Fatalf("expected 3 waves, got %d: %v", len(waves), waves)
	}

	// Wave 1: B(1), A(2) -- sorted by priority.
	if len(waves[0]) != 2 || waves[0][0] != "B" || waves[0][1] != "A" {
		t.Errorf("wave 1: expected [B, A], got %v", waves[0])
	}

	// Wave 2: C.
	if len(waves[1]) != 1 || waves[1][0] != "C" {
		t.Errorf("wave 2: expected [C], got %v", waves[1])
	}

	// Wave 3: D.
	if len(waves[2]) != 1 || waves[2][0] != "D" {
		t.Errorf("wave 3: expected [D], got %v", waves[2])
	}
}

func TestComputeWaves_WithPremergedTasks(t *testing.T) {
	tasks := []task.Task{
		makeTask("A", 1),
		makeTask("B", 2, "A"),
		makeTask("C", 3, "B"),
	}

	d, err := Build(tasks)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	rc := NewReadinessChecker(d)

	// A is already merged.
	statuses := map[string]lifecycle.Status{
		"A": lifecycle.StatusMerged,
		"B": lifecycle.StatusPending,
		"C": lifecycle.StatusPending,
	}

	waves := rc.ComputeWaves(statuses)
	if len(waves) != 2 {
		t.Fatalf("expected 2 waves (A already merged), got %d: %v", len(waves), waves)
	}

	// Wave 1: B (A is already merged, so B's prereqs are satisfied).
	if len(waves[0]) != 1 || waves[0][0] != "B" {
		t.Errorf("wave 1: expected [B], got %v", waves[0])
	}

	// Wave 2: C.
	if len(waves[1]) != 1 || waves[1][0] != "C" {
		t.Errorf("wave 2: expected [C], got %v", waves[1])
	}
}
