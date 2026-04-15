package engine

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/johnayoung/flywheel/internal/agent"
	"github.com/johnayoung/flywheel/internal/config"
	"github.com/johnayoung/flywheel/internal/conflict"
	"github.com/johnayoung/flywheel/internal/events"
	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/merge"
	"github.com/johnayoung/flywheel/internal/review"
	"github.com/johnayoung/flywheel/internal/store"
	"github.com/johnayoung/flywheel/internal/task"
	"github.com/johnayoung/flywheel/internal/validate"
	"github.com/johnayoung/flywheel/internal/worktree"
)

// ---------------------------------------------------------------------------
// testStore: in-memory TaskStore with real claim semantics
// ---------------------------------------------------------------------------

type testStore struct {
	mu         sync.Mutex
	tasks      []task.Task
	lifecycles map[string]*lifecycle.Lifecycle
}

func newTestStore(tasks []task.Task) *testStore {
	return &testStore{
		tasks:      tasks,
		lifecycles: make(map[string]*lifecycle.Lifecycle),
	}
}

func (s *testStore) ListTasks(_ context.Context, _ store.TaskFilter) ([]task.Task, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]task.Task, len(s.tasks))
	copy(out, s.tasks)
	return out, nil
}

func (s *testStore) GetTask(_ context.Context, id string) (*task.Task, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	for i := range s.tasks {
		if s.tasks[i].ID == id {
			t := s.tasks[i]
			return &t, nil
		}
	}
	return nil, store.ErrTaskNotFound
}

func (s *testStore) CreateLifecycle(_ context.Context, lc *lifecycle.Lifecycle) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, exists := s.lifecycles[lc.TaskID]; exists {
		return store.ErrLifecycleExists
	}
	cp := *lc
	s.lifecycles[lc.TaskID] = &cp
	return nil
}

func (s *testStore) GetLifecycle(_ context.Context, taskID string) (*lifecycle.Lifecycle, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	lc, ok := s.lifecycles[taskID]
	if !ok {
		return nil, store.ErrTaskNotFound
	}
	cp := *lc
	return &cp, nil
}

func (s *testStore) UpdateLifecycle(_ context.Context, lc *lifecycle.Lifecycle) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	existing, ok := s.lifecycles[lc.TaskID]
	if !ok {
		return store.ErrTaskNotFound
	}
	// Optimistic concurrency: version must match or be newer.
	if lc.Version < existing.Version {
		return store.ErrStaleWrite
	}
	cp := *lc
	s.lifecycles[lc.TaskID] = &cp
	return nil
}

func (s *testStore) ListLifecycles(_ context.Context, filter store.LifecycleFilter) ([]lifecycle.Lifecycle, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []lifecycle.Lifecycle
	for _, lc := range s.lifecycles {
		if len(filter.Statuses) > 0 {
			match := false
			for _, st := range filter.Statuses {
				if lc.Status == st {
					match = true
					break
				}
			}
			if !match {
				continue
			}
		}
		if filter.WorkerID != "" && lc.WorkerID != filter.WorkerID {
			continue
		}
		if filter.RunID != "" && lc.RunID != filter.RunID {
			continue
		}
		cp := *lc
		out = append(out, cp)
	}
	return out, nil
}

func (s *testStore) ClaimNextReady(_ context.Context, workerID string) (*lifecycle.Lifecycle, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	// Find a ready lifecycle with no worker assigned, sorted by task ID for determinism.
	var readyIDs []string
	for id, lc := range s.lifecycles {
		if lc.Status == lifecycle.StatusReady {
			readyIDs = append(readyIDs, id)
		}
	}
	sort.Strings(readyIDs)
	if len(readyIDs) == 0 {
		return nil, store.ErrNoReadyTasks
	}
	lc := s.lifecycles[readyIDs[0]]
	lc.WorkerID = workerID
	lc.Status = lifecycle.StatusRunning
	lc.Version++
	now := time.Now()
	lc.Timestamps.StartedAt = &now
	cp := *lc
	return &cp, nil
}

// ---------------------------------------------------------------------------
// testAgent: agent.Agent that creates a commit in the worktree
// ---------------------------------------------------------------------------

type testAgent struct {
	failTaskIDs  map[string]bool
	callCounts   map[string]*atomic.Int32
	mu           sync.Mutex
	executionLog []string // records task IDs in execution order
	executionCh  chan string
}

func newTestAgent() *testAgent {
	return &testAgent{
		failTaskIDs: make(map[string]bool),
		callCounts:  make(map[string]*atomic.Int32),
		executionCh: make(chan string, 100),
	}
}

func (a *testAgent) Execute(ctx context.Context, req agent.ExecutionRequest) (*agent.ExecutionResult, error) {
	// Track call count.
	a.mu.Lock()
	if _, ok := a.callCounts[req.TaskID]; !ok {
		a.callCounts[req.TaskID] = &atomic.Int32{}
	}
	counter := a.callCounts[req.TaskID]
	a.mu.Unlock()
	callNum := counter.Add(1)

	// Record execution order.
	a.mu.Lock()
	a.executionLog = append(a.executionLog, req.TaskID)
	a.mu.Unlock()
	select {
	case a.executionCh <- req.TaskID:
	default:
	}

	if a.failTaskIDs[req.TaskID] {
		return &agent.ExecutionResult{
			Success: false,
			Error:   "agent failure",
		}, nil
	}

	// Create a real file and commit in the worktree.
	filePath := filepath.Join(req.WorktreePath, req.TaskID+".txt")
	content := fmt.Sprintf("implemented %s (call %d)\n", req.TaskID, callNum)
	if err := os.WriteFile(filePath, []byte(content), 0644); err != nil {
		return nil, fmt.Errorf("writing file: %w", err)
	}

	// Determine commit prefix from task description or use a default.
	commitPrefix := req.TaskID
	if len(req.Steps) > 0 {
		commitPrefix = req.Steps[0]
	}

	cmd := exec.CommandContext(ctx, "git", "-C", req.WorktreePath, "add", ".")
	if out, err := cmd.CombinedOutput(); err != nil {
		return nil, fmt.Errorf("git add: %s: %w", string(out), err)
	}

	cmd = exec.CommandContext(ctx, "git", "-C", req.WorktreePath, "commit", "-m", commitPrefix+": implement "+req.TaskID)
	if out, err := cmd.CombinedOutput(); err != nil {
		return nil, fmt.Errorf("git commit: %s: %w", string(out), err)
	}

	return &agent.ExecutionResult{
		Success:             true,
		StepsCompleted:      len(req.Steps),
		Output:              "done",
		ImplementationNotes: "implemented " + req.TaskID,
	}, nil
}

// ---------------------------------------------------------------------------
// testReviewer: always approves
// ---------------------------------------------------------------------------

type testReviewer struct{}

func (r *testReviewer) Review(_ context.Context, _ review.ReviewRequest) (*review.ReviewResult, error) {
	return &review.ReviewResult{Approved: true}, nil
}

// ---------------------------------------------------------------------------
// testMerger: performs a real git merge
// ---------------------------------------------------------------------------

type testMerger struct {
	mu             sync.Mutex
	mergedBranches []string
	repoRoot       string
}

func (m *testMerger) Merge(ctx context.Context, req merge.MergeRequest) (*merge.MergeResult, error) {
	m.mu.Lock()
	defer m.mu.Unlock()

	// Checkout target in the main repo.
	cmd := exec.CommandContext(ctx, "git", "-C", m.repoRoot, "checkout", req.TargetRef)
	if out, err := cmd.CombinedOutput(); err != nil {
		return nil, fmt.Errorf("checkout %s: %s: %w", req.TargetRef, string(out), err)
	}

	// Merge the branch.
	cmd = exec.CommandContext(ctx, "git", "-C", m.repoRoot, "merge", req.Branch, "--no-ff", "-m", "merge "+req.Branch)
	out, err := cmd.CombinedOutput()
	if err != nil {
		outStr := string(out)
		if strings.Contains(outStr, "CONFLICT") {
			return &merge.MergeResult{HasConflicts: true, ConflictFiles: []string{"conflict.txt"}}, nil
		}
		return nil, fmt.Errorf("merge %s: %s: %w", req.Branch, outStr, err)
	}

	// Get the merge commit SHA.
	cmd = exec.CommandContext(ctx, "git", "-C", m.repoRoot, "rev-parse", "HEAD")
	shaOut, err := cmd.CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("rev-parse HEAD: %s: %w", string(shaOut), err)
	}
	sha := strings.TrimSpace(string(shaOut))

	m.mergedBranches = append(m.mergedBranches, req.Branch)
	return &merge.MergeResult{
		Success:        true,
		MergeCommitSHA: sha,
	}, nil
}

// ---------------------------------------------------------------------------
// Git test helpers
// ---------------------------------------------------------------------------

func initTestRepo(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()

	cmds := [][]string{
		{"git", "init", "--initial-branch=main"},
		{"git", "config", "user.email", "test@test.com"},
		{"git", "config", "user.name", "Test"},
	}
	for _, args := range cmds {
		cmd := exec.Command(args[0], args[1:]...)
		cmd.Dir = dir
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("git init cmd %v: %s: %v", args, string(out), err)
		}
	}

	// Create initial commit so main exists.
	initFile := filepath.Join(dir, "init.txt")
	if err := os.WriteFile(initFile, []byte("init\n"), 0644); err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command("git", "-C", dir, "add", ".")
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("git add: %s: %v", string(out), err)
	}
	cmd = exec.Command("git", "-C", dir, "commit", "-m", "initial commit")
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("git commit: %s: %v", string(out), err)
	}

	return dir
}

func makeTasks(defs ...taskDef) []task.Task {
	var tasks []task.Task
	for _, d := range defs {
		tasks = append(tasks, task.Task{
			ID:            d.id,
			Description:   "implement " + d.id,
			Category:      "feat",
			Priority:      d.priority,
			Prerequisites: d.deps,
			Commit:        d.id,
			Steps:         []string{d.id},
		})
	}
	return tasks
}

type taskDef struct {
	id       string
	priority int
	deps     []string
}

func buildEngine(t *testing.T, tasks []task.Task, ag *testAgent, maxParallel int) (*Engine, *testStore, *testMerger) {
	t.Helper()
	repoRoot := initTestRepo(t)
	worktreeBase := filepath.Join(repoRoot, ".worktrees")
	if err := os.MkdirAll(worktreeBase, 0755); err != nil {
		t.Fatal(err)
	}

	wm := worktree.NewManager(repoRoot, worktreeBase, "flywheel/")
	st := newTestStore(tasks)
	tm := &testMerger{repoRoot: repoRoot}
	v := validate.New("") // no build command -- all checks that need git will pass

	cfg := config.Config{
		BaseRef:            "main",
		BranchPrefix:       "flywheel/",
		MaxParallel:        maxParallel,
		MaxRetries:         2,
		MaxResolveAttempts: 2,
	}

	e := New(
		cfg,
		st,
		wm,
		func() agent.Agent { return ag },
		v,
		func(_ task.Task) review.Reviewer { return &testReviewer{} },
		tm,
		conflict.New(ag),
	)

	return e, st, tm
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

func TestIndependentTasksParallel(t *testing.T) {
	tasks := makeTasks(
		taskDef{id: "task-a", priority: 1},
		taskDef{id: "task-b", priority: 2},
		taskDef{id: "task-c", priority: 3},
	)

	ag := newTestAgent()
	e, st, tm := buildEngine(t, tasks, ag, 3)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	summary, err := e.Run(ctx)
	if err != nil {
		t.Fatalf("Run() error: %v", err)
	}

	if summary.TotalTasks != 3 {
		t.Errorf("TotalTasks = %d, want 3", summary.TotalTasks)
	}
	if summary.Merged != 3 {
		t.Errorf("Merged = %d, want 3", summary.Merged)
	}
	if summary.Failed != 0 {
		t.Errorf("Failed = %d, want 0", summary.Failed)
	}

	// Verify all lifecycles are merged.
	for _, taskID := range []string{"task-a", "task-b", "task-c"} {
		lc, err := st.GetLifecycle(ctx, taskID)
		if err != nil {
			t.Fatalf("GetLifecycle(%s): %v", taskID, err)
		}
		if lc.Status != lifecycle.StatusMerged {
			t.Errorf("task %s status = %s, want merged", taskID, lc.Status)
		}
	}

	// Verify merger was called for all 3.
	tm.mu.Lock()
	if len(tm.mergedBranches) != 3 {
		t.Errorf("merged branches = %d, want 3", len(tm.mergedBranches))
	}
	tm.mu.Unlock()
}

func TestDependencyChain(t *testing.T) {
	tasks := makeTasks(
		taskDef{id: "task-a", priority: 1},
		taskDef{id: "task-b", priority: 2, deps: []string{"task-a"}},
		taskDef{id: "task-c", priority: 3, deps: []string{"task-b"}},
	)

	ag := newTestAgent()
	e, _, _ := buildEngine(t, tasks, ag, 3)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	summary, err := e.Run(ctx)
	if err != nil {
		t.Fatalf("Run() error: %v", err)
	}

	if summary.Merged != 3 {
		t.Errorf("Merged = %d, want 3", summary.Merged)
	}

	// Verify execution order: A before B before C.
	ag.mu.Lock()
	log := make([]string, len(ag.executionLog))
	copy(log, ag.executionLog)
	ag.mu.Unlock()

	idxA, idxB, idxC := -1, -1, -1
	for i, id := range log {
		switch id {
		case "task-a":
			if idxA == -1 {
				idxA = i
			}
		case "task-b":
			if idxB == -1 {
				idxB = i
			}
		case "task-c":
			if idxC == -1 {
				idxC = i
			}
		}
	}

	if idxA >= idxB || idxB >= idxC {
		t.Errorf("execution order wrong: A=%d B=%d C=%d (want A < B < C)", idxA, idxB, idxC)
	}
}

func TestDiamondDependency(t *testing.T) {
	tasks := makeTasks(
		taskDef{id: "task-a", priority: 1},
		taskDef{id: "task-b", priority: 1},
		taskDef{id: "task-c", priority: 2, deps: []string{"task-a", "task-b"}},
	)

	ag := newTestAgent()
	e, _, _ := buildEngine(t, tasks, ag, 3)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	summary, err := e.Run(ctx)
	if err != nil {
		t.Fatalf("Run() error: %v", err)
	}

	if summary.Merged != 3 {
		t.Errorf("Merged = %d, want 3", summary.Merged)
	}

	// Verify C ran after both A and B.
	ag.mu.Lock()
	log := make([]string, len(ag.executionLog))
	copy(log, ag.executionLog)
	ag.mu.Unlock()

	idxC := -1
	maxAB := -1
	for i, id := range log {
		switch id {
		case "task-a", "task-b":
			if i > maxAB {
				maxAB = i
			}
		case "task-c":
			if idxC == -1 {
				idxC = i
			}
		}
	}

	if idxC <= maxAB {
		t.Errorf("task-c (idx=%d) should run after both A and B (max idx=%d)", idxC, maxAB)
	}
}

func TestAgentFailure(t *testing.T) {
	tasks := makeTasks(
		taskDef{id: "task-fail", priority: 1},
		taskDef{id: "task-ok", priority: 2},
	)

	ag := newTestAgent()
	ag.failTaskIDs["task-fail"] = true

	e, st, _ := buildEngine(t, tasks, ag, 2)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	summary, err := e.Run(ctx)
	if err != nil {
		t.Fatalf("Run() error: %v", err)
	}

	if summary.Failed != 1 {
		t.Errorf("Failed = %d, want 1", summary.Failed)
	}
	if summary.Merged != 1 {
		t.Errorf("Merged = %d, want 1", summary.Merged)
	}

	lc, _ := st.GetLifecycle(ctx, "task-fail")
	if lc.Status != lifecycle.StatusFailed {
		t.Errorf("task-fail status = %s, want failed", lc.Status)
	}
	if !strings.Contains(lc.Error, "agent") {
		t.Errorf("task-fail error = %q, want it to mention agent", lc.Error)
	}
}

func TestValidationFailureRetry(t *testing.T) {
	// Use a single task. The agent creates a commit, but validation will fail
	// the first time (commit message check fails because the first commit is
	// not a conventional commit). On retry, the agent creates a correct commit.
	tasks := []task.Task{{
		ID:          "task-retry",
		Description: "implement task-retry",
		Category:    "feat",
		Priority:    1,
		Commit:      "feat: implement task-retry",
		Steps:       []string{"task-retry"},
	}}

	repoRoot := initTestRepo(t)
	worktreeBase := filepath.Join(repoRoot, ".worktrees")
	if err := os.MkdirAll(worktreeBase, 0755); err != nil {
		t.Fatal(err)
	}

	wm := worktree.NewManager(repoRoot, worktreeBase, "flywheel/")
	st := newTestStore(tasks)
	tm := &testMerger{repoRoot: repoRoot}

	v := validate.New("")

	cfg := config.Config{
		BaseRef:            "main",
		BranchPrefix:       "flywheel/",
		MaxParallel:        1,
		MaxRetries:         2,
		MaxResolveAttempts: 2,
	}

	// First call: agent makes commit with a non-conventional subject -> validation fails.
	// Second call: agent makes commit with a "feat:" subject -> validation passes.
	badAgent := &retryTestAgent{
		callCounts: make(map[string]*atomic.Int32),
	}

	e := New(
		cfg,
		st,
		wm,
		func() agent.Agent { return badAgent },
		v,
		func(_ task.Task) review.Reviewer { return &testReviewer{} },
		tm,
		conflict.New(badAgent),
	)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	summary, err := e.Run(ctx)
	if err != nil {
		t.Fatalf("Run() error: %v", err)
	}

	if summary.Merged != 1 {
		t.Errorf("Merged = %d, want 1", summary.Merged)
	}

	lc, _ := st.GetLifecycle(ctx, "task-retry")
	if lc.Status != lifecycle.StatusMerged {
		t.Errorf("status = %s, want merged", lc.Status)
	}
	if lc.Retries < 1 {
		t.Errorf("retries = %d, want >= 1", lc.Retries)
	}
}

// retryTestAgent produces a bad commit message on the first call and a good one
// on subsequent calls, triggering a validation failure and retry.
type retryTestAgent struct {
	mu         sync.Mutex
	callCounts map[string]*atomic.Int32
}

func (a *retryTestAgent) Execute(ctx context.Context, req agent.ExecutionRequest) (*agent.ExecutionResult, error) {
	a.mu.Lock()
	if _, ok := a.callCounts[req.TaskID]; !ok {
		a.callCounts[req.TaskID] = &atomic.Int32{}
	}
	counter := a.callCounts[req.TaskID]
	a.mu.Unlock()

	callNum := counter.Add(1)

	filePath := filepath.Join(req.WorktreePath, req.TaskID+".txt")
	content := fmt.Sprintf("implemented %s (call %d)\n", req.TaskID, callNum)
	if err := os.WriteFile(filePath, []byte(content), 0644); err != nil {
		return nil, err
	}

	cmd := exec.CommandContext(ctx, "git", "-C", req.WorktreePath, "add", ".")
	if out, err := cmd.CombinedOutput(); err != nil {
		return nil, fmt.Errorf("git add: %s: %w", string(out), err)
	}

	// First call: non-conventional subject (no "<type>:" prefix) -> validation fails.
	// Subsequent calls: valid conventional "feat:" subject -> validation passes.
	subject := "feat: implement " + req.TaskID
	if callNum == 1 {
		subject = "implement " + req.TaskID + " (no conventional prefix)"
	}

	cmd = exec.CommandContext(ctx, "git", "-C", req.WorktreePath, "commit", "-m", subject)
	if out, err := cmd.CombinedOutput(); err != nil {
		return nil, fmt.Errorf("git commit: %s: %w", string(out), err)
	}

	return &agent.ExecutionResult{
		Success:             true,
		StepsCompleted:      len(req.Steps),
		Output:              "done",
		ImplementationNotes: "implemented " + req.TaskID,
	}, nil
}

func TestContextCancellation(t *testing.T) {
	// Create tasks with dependencies so workers will block waiting.
	tasks := makeTasks(
		taskDef{id: "task-a", priority: 1},
		taskDef{id: "task-b", priority: 2, deps: []string{"task-a"}},
	)

	// Use a slow agent that blocks until context is cancelled.
	slowAg := &slowAgent{
		started: make(chan struct{}),
	}

	repoRoot := initTestRepo(t)
	worktreeBase := filepath.Join(repoRoot, ".worktrees")
	if err := os.MkdirAll(worktreeBase, 0755); err != nil {
		t.Fatal(err)
	}

	wm := worktree.NewManager(repoRoot, worktreeBase, "flywheel/")
	st := newTestStore(tasks)
	tm := &testMerger{repoRoot: repoRoot}
	v := validate.New("")

	cfg := config.Config{
		BaseRef:            "main",
		BranchPrefix:       "flywheel/",
		MaxParallel:        2,
		MaxRetries:         2,
		MaxResolveAttempts: 2,
	}

	e := New(cfg, st, wm,
		func() agent.Agent { return slowAg },
		v,
		func(_ task.Task) review.Reviewer { return &testReviewer{} },
		tm,
		conflict.New(slowAg),
	)

	ctx, cancel := context.WithCancel(context.Background())

	done := make(chan struct{})
	var summary *RunSummary
	var runErr error
	go func() {
		summary, runErr = e.Run(ctx)
		close(done)
	}()

	// Wait for the agent to start executing, then cancel.
	select {
	case <-slowAg.started:
	case <-time.After(10 * time.Second):
		t.Fatal("timed out waiting for agent to start")
	}
	cancel()

	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("Run did not return after context cancellation")
	}

	// Run should return without error (or with context error).
	if runErr != nil && !errors.Is(runErr, context.Canceled) {
		t.Errorf("Run() error = %v, want nil or context.Canceled", runErr)
	}
	_ = summary
}

// slowAgent blocks on Execute until the context is cancelled.
type slowAgent struct {
	started chan struct{}
	once    sync.Once
}

func (a *slowAgent) Execute(ctx context.Context, req agent.ExecutionRequest) (*agent.ExecutionResult, error) {
	a.once.Do(func() { close(a.started) })
	<-ctx.Done()
	return &agent.ExecutionResult{
		Success: false,
		Error:   "context cancelled",
	}, nil
}

// TestContextCancellation_LeavesTaskInterruptedNotFailed asserts that when
// the operator interrupts mid-execution, the in-flight task lands in
// StatusInterrupted (resumable) rather than StatusFailed, and the retry
// budget is preserved. Regression test for the false-failure scenario
// where ctx.Canceled was treated as a terminal agent error.
func TestContextCancellation_LeavesTaskInterruptedNotFailed(t *testing.T) {
	tasks := makeTasks(taskDef{id: "task-a", priority: 1})

	slowAg := &slowAgent{started: make(chan struct{})}

	repoRoot := initTestRepo(t)
	worktreeBase := filepath.Join(repoRoot, ".worktrees")
	if err := os.MkdirAll(worktreeBase, 0755); err != nil {
		t.Fatal(err)
	}

	wm := worktree.NewManager(repoRoot, worktreeBase, "flywheel/")
	st := newTestStore(tasks)
	tm := &testMerger{repoRoot: repoRoot}
	v := validate.New("")

	cfg := config.Config{
		BaseRef:            "main",
		BranchPrefix:       "flywheel/",
		MaxParallel:        1,
		MaxRetries:         2,
		MaxResolveAttempts: 2,
	}

	e := New(cfg, st, wm,
		func() agent.Agent { return slowAg },
		v,
		func(_ task.Task) review.Reviewer { return &testReviewer{} },
		tm,
		conflict.New(slowAg),
	)

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		_, _ = e.Run(ctx)
		close(done)
	}()

	select {
	case <-slowAg.started:
	case <-time.After(10 * time.Second):
		t.Fatal("agent did not start")
	}
	cancel()

	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("engine did not return after cancel")
	}

	lc, err := st.GetLifecycle(context.Background(), "task-a")
	if err != nil {
		t.Fatalf("GetLifecycle: %v", err)
	}
	if lc.Status != lifecycle.StatusInterrupted {
		t.Errorf("status = %s, want interrupted", lc.Status)
	}
	if lc.Retries != 0 {
		t.Errorf("Retries = %d after cancel, want 0 (cancellation must not consume budget)", lc.Retries)
	}
	if len(lc.Attempts) != 1 {
		t.Fatalf("Attempts = %d, want 1", len(lc.Attempts))
	}
	att := lc.Attempts[0]
	if att.Outcome != lifecycle.OutcomeCancelled {
		t.Errorf("attempt outcome = %q, want %q", att.Outcome, lifecycle.OutcomeCancelled)
	}
	if att.EndedAt == nil {
		t.Error("attempt EndedAt should be set")
	}
}

// alwaysFailAgent returns Success=false on every call (no commit, no error).
type alwaysFailAgent struct {
	mu    sync.Mutex
	calls int
}

func (a *alwaysFailAgent) Execute(_ context.Context, _ agent.ExecutionRequest) (*agent.ExecutionResult, error) {
	a.mu.Lock()
	a.calls++
	a.mu.Unlock()
	return &agent.ExecutionResult{Success: false, Error: "always fails"}, nil
}

// TestAgentError_RetriesUpToMaxThenFails asserts that a non-cancellation
// agent error consumes the retry budget (instead of being instant-terminal)
// and only lands in StatusFailed after retries are exhausted.
func TestAgentError_RetriesUpToMaxThenFails(t *testing.T) {
	tasks := makeTasks(taskDef{id: "task-fail", priority: 1})

	ag := &alwaysFailAgent{}
	repoRoot := initTestRepo(t)
	worktreeBase := filepath.Join(repoRoot, ".worktrees")
	if err := os.MkdirAll(worktreeBase, 0755); err != nil {
		t.Fatal(err)
	}
	wm := worktree.NewManager(repoRoot, worktreeBase, "flywheel/")
	st := newTestStore(tasks)
	tm := &testMerger{repoRoot: repoRoot}
	v := validate.New("")

	cfg := config.Config{
		BaseRef:            "main",
		BranchPrefix:       "flywheel/",
		MaxParallel:        1,
		MaxRetries:         2,
		MaxResolveAttempts: 2,
	}

	e := New(cfg, st, wm,
		func() agent.Agent { return ag },
		v,
		func(_ task.Task) review.Reviewer { return &testReviewer{} },
		tm,
		conflict.New(ag),
	)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if _, err := e.Run(ctx); err != nil {
		t.Fatalf("Run: %v", err)
	}

	lc, err := st.GetLifecycle(ctx, "task-fail")
	if err != nil {
		t.Fatalf("GetLifecycle: %v", err)
	}
	if lc.Status != lifecycle.StatusFailed {
		t.Errorf("status = %s, want failed", lc.Status)
	}
	// MaxRetries=2 means the initial attempt + 2 retries = 3 attempts total.
	if lc.Retries != cfg.MaxRetries {
		t.Errorf("Retries = %d, want %d", lc.Retries, cfg.MaxRetries)
	}
	if len(lc.Attempts) != cfg.MaxRetries+1 {
		t.Errorf("Attempts = %d, want %d", len(lc.Attempts), cfg.MaxRetries+1)
	}
	for i, att := range lc.Attempts {
		if att.Outcome != lifecycle.OutcomeAgentError {
			t.Errorf("attempt %d outcome = %q, want %q", i, att.Outcome, lifecycle.OutcomeAgentError)
		}
	}
}

// captureBus records every event for assertions.
type captureBus struct {
	mu     sync.Mutex
	events []events.Event
}

func (c *captureBus) Publish(e events.Event) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.events = append(c.events, e)
}

func (c *captureBus) types() []string {
	c.mu.Lock()
	defer c.mu.Unlock()
	out := make([]string, len(c.events))
	for i, e := range c.events {
		out[i] = e.Type
	}
	return out
}

// TestEngine_EmitsEvents asserts the engine publishes the expected event
// sequence for a happy-path single-task run.
func TestEngine_EmitsEvents(t *testing.T) {
	tasks := makeTasks(taskDef{id: "task-x", priority: 1})

	ag := newTestAgent()
	e, _, _ := buildEngine(t, tasks, ag, 1)

	bus := &captureBus{}
	e.bus = bus

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	if _, err := e.Run(ctx); err != nil {
		t.Fatalf("Run: %v", err)
	}

	got := bus.types()
	// Must contain at least these in order.
	required := []string{
		events.TypeRunStarted,
		events.TypeTaskClaimed,
		events.TypeTaskAttemptStarted,
		events.TypeTaskPhaseChanged, // running -> validating
		events.TypeTaskPhaseChanged, // validating -> reviewing
		events.TypeTaskPhaseChanged, // reviewing -> merging
		events.TypeTaskAttemptEnded,
		events.TypeRunCompleted,
	}

	if !containsSubsequence(got, required) {
		t.Errorf("event sequence missing required subsequence.\n got: %v\nwant subsequence: %v", got, required)
	}
}

// TestPlanSummary_EmittedAtStartup asserts the engine publishes a plan_summary
// event right after run_started so operators see the shape of the run.
func TestPlanSummary_EmittedAtStartup(t *testing.T) {
	tasks := makeTasks(
		taskDef{id: "a", priority: 1},
		taskDef{id: "b", priority: 2, deps: []string{"a"}},
	)
	ag := newTestAgent()
	e, _, _ := buildEngine(t, tasks, ag, 1)
	bus := &captureBus{}
	e.bus = bus

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if _, err := e.Run(ctx); err != nil {
		t.Fatalf("Run: %v", err)
	}

	types := bus.types()
	// plan_summary must appear and follow run_started.
	startedIdx, summaryIdx := -1, -1
	for i, ty := range types {
		if ty == events.TypeRunStarted && startedIdx == -1 {
			startedIdx = i
		}
		if ty == events.TypePlanSummary && summaryIdx == -1 {
			summaryIdx = i
		}
	}
	if startedIdx == -1 || summaryIdx == -1 || summaryIdx < startedIdx {
		t.Fatalf("expected plan_summary after run_started; got: %v", types)
	}
}

// TestAttempt_StampsRunID asserts that attempts created during a run carry
// the engine's runID so ConsecutiveFailedRuns can reason across runs.
func TestAttempt_StampsRunID(t *testing.T) {
	tasks := makeTasks(taskDef{id: "only", priority: 1})
	ag := newTestAgent()
	e, st, _ := buildEngine(t, tasks, ag, 1)

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if _, err := e.Run(ctx); err != nil {
		t.Fatalf("Run: %v", err)
	}
	lc, err := st.GetLifecycle(ctx, "only")
	if err != nil {
		t.Fatalf("GetLifecycle: %v", err)
	}
	if len(lc.Attempts) == 0 {
		t.Fatal("expected at least one attempt")
	}
	if lc.Attempts[0].RunID == "" {
		t.Error("attempt RunID is empty; engine did not stamp runID")
	}
}

// TestAutoReset_ResetsPriorRunFailureBelowCap asserts that a task left in
// StatusFailed by a prior run (with cross-run failure count below cap) is
// revived on the next Run() and a task_auto_reset event is emitted.
func TestAutoReset_ResetsPriorRunFailureBelowCap(t *testing.T) {
	tasks := makeTasks(taskDef{id: "stale", priority: 1})
	ag := newTestAgent()
	e, st, _ := buildEngine(t, tasks, ag, 1)
	e.cfg.ConsecutiveFailureCap = 2

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	// Seed a prior-run failure.
	prior := lifecycle.NewLifecycle("stale", "prior-run-123", "main")
	prior.Status = lifecycle.StatusFailed
	endedAt := time.Now()
	prior.Attempts = []lifecycle.Attempt{{
		Number:    1,
		StartedAt: time.Now().Add(-time.Hour),
		EndedAt:   &endedAt,
		Outcome:   lifecycle.OutcomeAgentError,
		RunID:     "prior-run-123",
	}}
	if err := st.CreateLifecycle(ctx, prior); err != nil {
		t.Fatalf("seed: %v", err)
	}

	bus := &captureBus{}
	e.bus = bus
	if _, err := e.Run(ctx); err != nil {
		t.Fatalf("Run: %v", err)
	}

	// Should see auto_reset event.
	sawAutoReset := false
	for _, ev := range bus.events {
		if ev.Type == events.TypeTaskAutoReset && ev.TaskID == "stale" {
			sawAutoReset = true
		}
	}
	if !sawAutoReset {
		t.Error("expected task_auto_reset for 'stale'")
	}
	// Task should have progressed past StatusFailed.
	lc, _ := st.GetLifecycle(ctx, "stale")
	if lc.Status == lifecycle.StatusFailed {
		t.Errorf("task still StatusFailed; auto-reset did not take effect")
	}
}

// TestAutoReset_StopsAtCap asserts tasks that have already consumed the
// consecutive-run failure cap are NOT auto-reset.
func TestAutoReset_StopsAtCap(t *testing.T) {
	tasks := makeTasks(taskDef{id: "capped", priority: 1})
	ag := newTestAgent()
	e, st, _ := buildEngine(t, tasks, ag, 1)
	e.cfg.ConsecutiveFailureCap = 2

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	// Seed 2 prior-run failures.
	prior := lifecycle.NewLifecycle("capped", "seed", "main")
	prior.Status = lifecycle.StatusFailed
	now := time.Now()
	prior.Attempts = []lifecycle.Attempt{
		{Number: 1, StartedAt: now, EndedAt: &now, Outcome: lifecycle.OutcomeAgentError, RunID: "run-1"},
		{Number: 2, StartedAt: now, EndedAt: &now, Outcome: lifecycle.OutcomeAgentError, RunID: "run-2"},
	}
	if err := st.CreateLifecycle(ctx, prior); err != nil {
		t.Fatalf("seed: %v", err)
	}

	bus := &captureBus{}
	e.bus = bus
	// Engine will deadlock (single capped failed task, no other work).
	_, err := e.Run(ctx)
	if err != nil && !errors.Is(err, ErrDeadlocked) {
		t.Fatalf("Run: unexpected error: %v", err)
	}

	// No auto_reset event for capped task.
	for _, ev := range bus.events {
		if ev.Type == events.TypeTaskAutoReset && ev.TaskID == "capped" {
			t.Error("capped task was auto-reset; expected it to stay failed")
		}
	}
	lc, _ := st.GetLifecycle(ctx, "capped")
	if lc.Status != lifecycle.StatusFailed {
		t.Errorf("capped task status=%s, want failed", lc.Status)
	}
}

// TestDeadlock_EmitsAndReturnsErr asserts that when all non-terminal tasks
// are blocked by a capped failure, the engine emits run_deadlocked and
// returns ErrDeadlocked.
func TestDeadlock_EmitsAndReturnsErr(t *testing.T) {
	tasks := makeTasks(
		taskDef{id: "root", priority: 1},
		taskDef{id: "child", priority: 2, deps: []string{"root"}},
	)
	ag := newTestAgent()
	e, st, _ := buildEngine(t, tasks, ag, 1)
	e.cfg.ConsecutiveFailureCap = 1

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	// Seed root as a capped failure.
	now := time.Now()
	root := lifecycle.NewLifecycle("root", "seed", "main")
	root.Status = lifecycle.StatusFailed
	root.Attempts = []lifecycle.Attempt{
		{Number: 1, StartedAt: now, EndedAt: &now, Outcome: lifecycle.OutcomeAgentError, RunID: "run-1"},
	}
	if err := st.CreateLifecycle(ctx, root); err != nil {
		t.Fatalf("seed root: %v", err)
	}
	// child is auto-created as StatusPending.

	bus := &captureBus{}
	e.bus = bus
	_, err := e.Run(ctx)
	if !errors.Is(err, ErrDeadlocked) {
		t.Fatalf("Run: want ErrDeadlocked, got %v", err)
	}

	sawDeadlock := false
	for _, ev := range bus.events {
		if ev.Type == events.TypeRunDeadlocked {
			sawDeadlock = true
		}
	}
	if !sawDeadlock {
		t.Error("expected run_deadlocked event")
	}
}

// containsSubsequence reports whether want occurs in seq in order (not
// necessarily contiguously).
func containsSubsequence(seq, want []string) bool {
	i := 0
	for _, s := range seq {
		if i < len(want) && s == want[i] {
			i++
		}
	}
	return i == len(want)
}
