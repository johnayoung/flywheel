package jsonl

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"testing"

	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/store"
)

// writeTaskFile writes a valid task JSON file into dir.
func writeTaskFile(t *testing.T, dir, id, category string, priority int) {
	t.Helper()
	data := map[string]interface{}{
		"id":          id,
		"description": "Task " + id,
		"category":    category,
		"priority":    priority,
		"commit":      "implement " + id,
		"steps":       []string{"step-1"},
	}
	b, err := json.MarshalIndent(data, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, id+".json"), b, 0o644); err != nil {
		t.Fatal(err)
	}
}

// newTestStore creates a Store with the given tasks written as fixture files.
type taskDef struct {
	ID       string
	Category string
	Priority int
}

func newTestStore(t *testing.T, defs []taskDef) *Store {
	t.Helper()
	tasksDir := t.TempDir()
	lcDir := t.TempDir()

	for _, d := range defs {
		writeTaskFile(t, tasksDir, d.ID, d.Category, d.Priority)
	}

	s, err := New(tasksDir, lcDir)
	if err != nil {
		t.Fatal(err)
	}
	return s
}

// createReadyLifecycle creates a lifecycle in the "ready" state for the given task.
func createReadyLifecycle(t *testing.T, s *Store, taskID string) {
	t.Helper()
	ctx := context.Background()
	lc := lifecycle.NewLifecycle(taskID, "run-1", "main")
	if err := s.CreateLifecycle(ctx, lc); err != nil {
		t.Fatal(err)
	}
	// Transition pending -> ready
	lc, err := s.GetLifecycle(ctx, taskID)
	if err != nil {
		t.Fatal(err)
	}
	if err := lifecycle.Transition(lc, lifecycle.StatusReady); err != nil {
		t.Fatal(err)
	}
	// Write back without going through UpdateLifecycle (which increments version again).
	// We need to use UpdateLifecycle but account for the version bump from Transition.
	// Transition already incremented Version, so we need the pre-transition version for the
	// optimistic concurrency check. Re-read to get current disk version, then apply.
	diskLC, err := s.GetLifecycle(ctx, taskID)
	if err != nil {
		t.Fatal(err)
	}
	diskLC.Status = lifecycle.StatusReady
	diskLC.Timestamps = lc.Timestamps
	// UpdateLifecycle will check diskLC.Version matches disk, then increment.
	if err := s.UpdateLifecycle(ctx, diskLC); err != nil {
		t.Fatal(err)
	}
}

func TestListTasks(t *testing.T) {
	s := newTestStore(t, []taskDef{
		{ID: "task-1", Category: "feat", Priority: 1},
		{ID: "task-2", Category: "fix", Priority: 2},
		{ID: "task-3", Category: "feat", Priority: 3},
	})

	ctx := context.Background()

	// No filter returns all.
	tasks, err := s.ListTasks(ctx, store.TaskFilter{})
	if err != nil {
		t.Fatal(err)
	}
	if len(tasks) != 3 {
		t.Fatalf("expected 3 tasks, got %d", len(tasks))
	}

	// Filter by category.
	tasks, err = s.ListTasks(ctx, store.TaskFilter{Categories: []string{"feat"}})
	if err != nil {
		t.Fatal(err)
	}
	if len(tasks) != 2 {
		t.Fatalf("expected 2 feat tasks, got %d", len(tasks))
	}
	for _, tk := range tasks {
		if tk.Category != "feat" {
			t.Fatalf("expected category feat, got %s", tk.Category)
		}
	}
}

func TestCreateAndGetLifecycle(t *testing.T) {
	s := newTestStore(t, []taskDef{
		{ID: "task-1", Category: "feat", Priority: 1},
	})

	ctx := context.Background()
	lc := lifecycle.NewLifecycle("task-1", "run-1", "main")

	if err := s.CreateLifecycle(ctx, lc); err != nil {
		t.Fatal(err)
	}

	got, err := s.GetLifecycle(ctx, "task-1")
	if err != nil {
		t.Fatal(err)
	}

	if got.TaskID != "task-1" {
		t.Fatalf("expected task-1, got %s", got.TaskID)
	}
	if got.RunID != "run-1" {
		t.Fatalf("expected run-1, got %s", got.RunID)
	}
	if got.Status != lifecycle.StatusPending {
		t.Fatalf("expected pending, got %s", got.Status)
	}
}

func TestCreateLifecycleDuplicate(t *testing.T) {
	s := newTestStore(t, []taskDef{
		{ID: "task-1", Category: "feat", Priority: 1},
	})

	ctx := context.Background()
	lc := lifecycle.NewLifecycle("task-1", "run-1", "main")

	if err := s.CreateLifecycle(ctx, lc); err != nil {
		t.Fatal(err)
	}

	lc2 := lifecycle.NewLifecycle("task-1", "run-2", "main")
	err := s.CreateLifecycle(ctx, lc2)
	if err != store.ErrLifecycleExists {
		t.Fatalf("expected ErrLifecycleExists, got %v", err)
	}
}

func TestUpdateLifecycle(t *testing.T) {
	s := newTestStore(t, []taskDef{
		{ID: "task-1", Category: "feat", Priority: 1},
	})

	ctx := context.Background()
	lc := lifecycle.NewLifecycle("task-1", "run-1", "main")
	if err := s.CreateLifecycle(ctx, lc); err != nil {
		t.Fatal(err)
	}

	// Read back from disk.
	got, err := s.GetLifecycle(ctx, "task-1")
	if err != nil {
		t.Fatal(err)
	}

	got.BaseRef = "develop"
	if err := s.UpdateLifecycle(ctx, got); err != nil {
		t.Fatal(err)
	}

	updated, err := s.GetLifecycle(ctx, "task-1")
	if err != nil {
		t.Fatal(err)
	}
	if updated.Version != 1 {
		t.Fatalf("expected version 1, got %d", updated.Version)
	}
	if updated.BaseRef != "develop" {
		t.Fatalf("expected base_ref develop, got %s", updated.BaseRef)
	}

	// Stale write: use the old version.
	got.Version = 0 // stale
	err = s.UpdateLifecycle(ctx, got)
	if err != store.ErrStaleWrite {
		t.Fatalf("expected ErrStaleWrite, got %v", err)
	}
}

func TestClaimNextReady(t *testing.T) {
	s := newTestStore(t, []taskDef{
		{ID: "task-low", Category: "feat", Priority: 10},
		{ID: "task-high", Category: "feat", Priority: 1},
		{ID: "task-mid", Category: "fix", Priority: 5},
	})

	ctx := context.Background()

	createReadyLifecycle(t, s, "task-low")
	createReadyLifecycle(t, s, "task-high")
	createReadyLifecycle(t, s, "task-mid")

	claimed, err := s.ClaimNextReady(ctx, "worker-1")
	if err != nil {
		t.Fatal(err)
	}

	if claimed.TaskID != "task-high" {
		t.Fatalf("expected task-high (priority 1), got %s", claimed.TaskID)
	}
	if claimed.WorkerID != "worker-1" {
		t.Fatalf("expected worker-1, got %s", claimed.WorkerID)
	}
	if claimed.Status != lifecycle.StatusRunning {
		t.Fatalf("expected running, got %s", claimed.Status)
	}
}

func TestClaimNextReadyNoReady(t *testing.T) {
	s := newTestStore(t, []taskDef{
		{ID: "task-1", Category: "feat", Priority: 1},
	})

	ctx := context.Background()

	// Create a pending lifecycle but don't transition to ready.
	lc := lifecycle.NewLifecycle("task-1", "run-1", "main")
	if err := s.CreateLifecycle(ctx, lc); err != nil {
		t.Fatal(err)
	}

	_, err := s.ClaimNextReady(ctx, "worker-1")
	if err != store.ErrNoReadyTasks {
		t.Fatalf("expected ErrNoReadyTasks, got %v", err)
	}
}

func TestConcurrentClaimNextReady(t *testing.T) {
	s := newTestStore(t, []taskDef{
		{ID: "task-a", Category: "feat", Priority: 1},
		{ID: "task-b", Category: "feat", Priority: 2},
		{ID: "task-c", Category: "feat", Priority: 3},
	})

	ctx := context.Background()

	createReadyLifecycle(t, s, "task-a")
	createReadyLifecycle(t, s, "task-b")
	createReadyLifecycle(t, s, "task-c")

	var wg sync.WaitGroup
	var mu sync.Mutex
	claimed := make(map[string]string) // taskID -> workerID

	for i := 0; i < 3; i++ {
		wg.Add(1)
		workerID := "worker-" + string(rune('1'+i))
		go func(wid string) {
			defer wg.Done()
			lc, err := s.ClaimNextReady(ctx, wid)
			if err != nil {
				t.Errorf("worker %s: %v", wid, err)
				return
			}
			mu.Lock()
			claimed[lc.TaskID] = wid
			mu.Unlock()
		}(workerID)
	}

	wg.Wait()

	if len(claimed) != 3 {
		t.Fatalf("expected 3 claims, got %d: %v", len(claimed), claimed)
	}

	// Verify all three tasks were claimed.
	var taskIDs []string
	for id := range claimed {
		taskIDs = append(taskIDs, id)
	}
	sort.Strings(taskIDs)
	expected := []string{"task-a", "task-b", "task-c"}
	for i, id := range taskIDs {
		if id != expected[i] {
			t.Fatalf("expected %v, got %v", expected, taskIDs)
		}
	}
}
