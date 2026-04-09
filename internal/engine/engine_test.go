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
	v := validate.New(nil, "") // no build command, no agent -- all checks that need git will pass

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
	// the first time (commit message check fails because we sabotage the commit prefix).
	// On retry, the agent creates a correct commit.
	tasks := []task.Task{{
		ID:          "task-retry",
		Description: "implement task-retry",
		Category:    "feat",
		Priority:    1,
		Commit:      "task-retry",
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

	// Create a validator that tracks calls and fails the first time.
	var validationCalls atomic.Int32
	// We use a custom agent for validation that always says PASS.
	validatorAgent := &passAgent{}
	v := validate.New(validatorAgent, "")

	cfg := config.Config{
		BaseRef:            "main",
		BranchPrefix:       "flywheel/",
		MaxParallel:        1,
		MaxRetries:         2,
		MaxResolveAttempts: 2,
	}

	// Wrap the validator to fail on first call.
	wrappedValidator := &countingValidator{
		inner:     v,
		failUntil: 1, // fail on call #1, pass on call #2+
		callCount: &validationCalls,
	}

	// We need to create the engine manually to inject the wrapped validator.
	// Since Validator is a concrete type, we'll use a different approach:
	// Make the agent produce a bad commit message on the first call.
	// The testAgent already creates commits with the task ID prefix, but we can
	// make the first call use a wrong prefix.
	badAgent := &retryTestAgent{
		callCounts: make(map[string]*atomic.Int32),
	}

	// Use the real validator (it checks commit messages).
	// First call: agent makes commit with wrong prefix -> validation fails.
	// Second call: agent makes commit with correct prefix -> validation passes.
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

	// Ignore wrappedValidator (it was a dead-end approach), use badAgent instead.
	_ = wrappedValidator

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

	// First call: use wrong commit prefix to trigger validation failure.
	// Subsequent calls: use correct prefix.
	prefix := req.TaskID
	if callNum == 1 {
		prefix = "WRONG-PREFIX"
	}

	cmd = exec.CommandContext(ctx, "git", "-C", req.WorktreePath, "commit", "-m", prefix+": implement "+req.TaskID)
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

// passAgent is a minimal agent that always returns success with PASS.
type passAgent struct{}

func (a *passAgent) Execute(_ context.Context, _ agent.ExecutionRequest) (*agent.ExecutionResult, error) {
	return &agent.ExecutionResult{
		Success: true,
		Output:  "PASS",
	}, nil
}

// countingValidator wraps a real validator and forces failure for the first N calls.
type countingValidator struct {
	inner     *validate.Validator
	failUntil int32
	callCount *atomic.Int32
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
	v := validate.New(nil, "")

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
