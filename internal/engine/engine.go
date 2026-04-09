package engine

import (
	"context"
	"errors"
	"fmt"
	"math/rand/v2"
	"os/exec"
	"strings"
	"sync"
	"time"

	"github.com/johnayoung/flywheel/internal/agent"
	"github.com/johnayoung/flywheel/internal/config"
	"github.com/johnayoung/flywheel/internal/conflict"
	"github.com/johnayoung/flywheel/internal/dag"
	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/merge"
	"github.com/johnayoung/flywheel/internal/review"
	"github.com/johnayoung/flywheel/internal/store"
	"github.com/johnayoung/flywheel/internal/task"
	"github.com/johnayoung/flywheel/internal/validate"
	"github.com/johnayoung/flywheel/internal/worktree"
)

// Engine orchestrates the full lifecycle of tasks: scheduling via DAG, agent
// execution, validation, review, merge, and conflict resolution.
type Engine struct {
	cfg         config.Config
	store       store.TaskStore
	dag         *dag.DAG
	checker     *dag.ReadinessChecker
	worktrees   *worktree.Manager
	agentFn     func() agent.Agent
	validator   *validate.Validator
	reviewerFn  func(task.Task) review.Reviewer
	merger      merge.Merger
	resolver    *conflict.Resolver
	maxParallel int

	mu   sync.Mutex
	cond *sync.Cond
}

// RunSummary captures the outcome of a full engine run.
type RunSummary struct {
	RunID      string
	TotalTasks int
	Merged     int
	Failed     int
	Pending    int
	Duration   time.Duration
}

// New creates an Engine wired to the provided dependencies.
func New(
	cfg config.Config,
	st store.TaskStore,
	wm *worktree.Manager,
	agentFn func() agent.Agent,
	validator *validate.Validator,
	reviewerFn func(task.Task) review.Reviewer,
	merger merge.Merger,
	resolver *conflict.Resolver,
) *Engine {
	e := &Engine{
		cfg:         cfg,
		store:       st,
		worktrees:   wm,
		agentFn:     agentFn,
		validator:   validator,
		reviewerFn:  reviewerFn,
		merger:      merger,
		resolver:    resolver,
		maxParallel: cfg.MaxParallel,
	}
	e.cond = sync.NewCond(&e.mu)
	return e
}

// Run executes the full orchestration loop: build DAG, initialize lifecycles,
// recover from crashes, evaluate initial readiness, and dispatch workers.
func (e *Engine) Run(ctx context.Context) (*RunSummary, error) {
	start := time.Now()

	// 1. Load tasks.
	tasks, err := e.store.ListTasks(ctx, store.TaskFilter{})
	if err != nil {
		return nil, fmt.Errorf("listing tasks: %w", err)
	}
	if len(tasks) == 0 {
		return &RunSummary{Duration: time.Since(start)}, nil
	}

	// 2. Build DAG.
	d, err := dag.Build(tasks)
	if err != nil {
		return nil, fmt.Errorf("building DAG: %w", err)
	}
	e.dag = d
	e.checker = dag.NewReadinessChecker(d)

	// 3. Generate run ID.
	runID := fmt.Sprintf("run-%s-%04x", time.Now().Format("20060102-150405"), rand.IntN(0x10000))

	// 4. Initialize lifecycles.
	for _, t := range tasks {
		lc := lifecycle.NewLifecycle(t.ID, runID, e.cfg.BaseRef)
		if err := e.store.CreateLifecycle(ctx, lc); err != nil {
			if errors.Is(err, store.ErrLifecycleExists) {
				continue // resume support
			}
			return nil, fmt.Errorf("creating lifecycle for task %s: %w", t.ID, err)
		}
	}

	// 5. Crash recovery: reset any in-flight states back to ready.
	inFlightStatuses := []lifecycle.Status{
		lifecycle.StatusRunning,
		lifecycle.StatusValidating,
		lifecycle.StatusReviewing,
		lifecycle.StatusMerging,
		lifecycle.StatusResolving,
	}
	allLCs, err := e.store.ListLifecycles(ctx, store.LifecycleFilter{})
	if err != nil {
		return nil, fmt.Errorf("listing lifecycles for crash recovery: %w", err)
	}
	for _, lc := range allLCs {
		needsRecovery := false
		for _, s := range inFlightStatuses {
			if lc.Status == s {
				needsRecovery = true
				break
			}
		}
		if !needsRecovery {
			continue
		}
		// Try to remove stale worktree.
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		if err := lifecycle.Transition(&lc, lifecycle.StatusReady); err != nil {
			return nil, fmt.Errorf("crash recovery transition for %s: %w", lc.TaskID, err)
		}
		if err := e.store.UpdateLifecycle(ctx, &lc); err != nil {
			return nil, fmt.Errorf("crash recovery update for %s: %w", lc.TaskID, err)
		}
	}

	// 6. Evaluate initial readiness.
	if err := e.evaluateReadiness(ctx); err != nil {
		return nil, fmt.Errorf("evaluating initial readiness: %w", err)
	}

	// 7. Start workers.
	var wg sync.WaitGroup
	for i := 0; i < e.maxParallel; i++ {
		wg.Add(1)
		workerID := fmt.Sprintf("worker-%d", i)
		go func() {
			defer wg.Done()
			e.worker(ctx, workerID)
		}()
	}

	wg.Wait()

	// 8. Build summary.
	finalLCs, err := e.store.ListLifecycles(ctx, store.LifecycleFilter{})
	if err != nil {
		return nil, fmt.Errorf("listing lifecycles for summary: %w", err)
	}
	summary := &RunSummary{
		RunID:      runID,
		TotalTasks: len(tasks),
		Duration:   time.Since(start),
	}
	for _, lc := range finalLCs {
		switch lc.Status {
		case lifecycle.StatusMerged:
			summary.Merged++
		case lifecycle.StatusFailed:
			summary.Failed++
		default:
			summary.Pending++
		}
	}
	return summary, nil
}

// evaluateReadiness transitions all pending tasks whose prerequisites are met
// from pending to ready.
func (e *Engine) evaluateReadiness(ctx context.Context) error {
	allLCs, err := e.store.ListLifecycles(ctx, store.LifecycleFilter{})
	if err != nil {
		return err
	}
	statuses := make(map[string]lifecycle.Status, len(allLCs))
	for _, lc := range allLCs {
		statuses[lc.TaskID] = lc.Status
	}
	for _, lc := range allLCs {
		if lc.Status != lifecycle.StatusPending {
			continue
		}
		if e.checker.IsReady(lc.TaskID, statuses) {
			lcCopy := lc
			if err := lifecycle.Transition(&lcCopy, lifecycle.StatusReady); err != nil {
				return fmt.Errorf("readiness transition for %s: %w", lc.TaskID, err)
			}
			if err := e.store.UpdateLifecycle(ctx, &lcCopy); err != nil {
				return fmt.Errorf("readiness update for %s: %w", lc.TaskID, err)
			}
		}
	}
	return nil
}

// worker is the main loop for a single worker goroutine. It claims ready tasks,
// executes the agent pipeline, and loops until all tasks are terminal or the
// context is cancelled.
func (e *Engine) worker(ctx context.Context, workerID string) {
	for {
		if ctx.Err() != nil {
			return
		}

		lc, err := e.store.ClaimNextReady(ctx, workerID)
		if err != nil {
			if errors.Is(err, store.ErrNoReadyTasks) {
				if e.allTerminal(ctx) {
					return
				}
				if err := e.waitForReady(ctx); err != nil {
					return // context cancelled
				}
				continue
			}
			// Unexpected store error; back off briefly then retry.
			select {
			case <-ctx.Done():
				return
			case <-time.After(100 * time.Millisecond):
				continue
			}
		}

		e.processTask(ctx, workerID, lc)
	}
}

// allTerminal returns true if every lifecycle is in a terminal state.
func (e *Engine) allTerminal(ctx context.Context) bool {
	lcs, err := e.store.ListLifecycles(ctx, store.LifecycleFilter{})
	if err != nil {
		return false
	}
	for _, lc := range lcs {
		if !lifecycle.IsTerminal(lc.Status) {
			return false
		}
	}
	return true
}

// waitForReady blocks until cond is broadcast or the context is cancelled.
func (e *Engine) waitForReady(ctx context.Context) error {
	done := make(chan struct{})
	go func() {
		e.mu.Lock()
		e.cond.Wait()
		e.mu.Unlock()
		close(done)
	}()
	select {
	case <-done:
		return nil
	case <-ctx.Done():
		e.cond.Broadcast() // wake the waiting goroutine so it exits
		return ctx.Err()
	}
}

// processTask executes the full pipeline for a single claimed task: agent
// execution, validation, review, merge, conflict resolution.
func (e *Engine) processTask(ctx context.Context, workerID string, lc *lifecycle.Lifecycle) {
	// Fetch the task.
	t, err := e.store.GetTask(ctx, lc.TaskID)
	if err != nil {
		lc.Error = fmt.Sprintf("fetching task: %v", err)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		return
	}

	// Create worktree.
	wt, err := e.worktrees.Create(ctx, lc.TaskID, e.cfg.BaseRef)
	if err != nil {
		lc.Error = fmt.Sprintf("creating worktree: %v", err)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		return
	}
	lc.Branch = wt.Branch
	lc.WorktreePath = wt.Path
	lc.BaseSHA = wt.BaseSHA
	_ = e.store.UpdateLifecycle(ctx, lc)

	// Execute agent.
	req := agent.ExecutionRequest{
		WorktreePath: wt.Path,
		TaskID:       t.ID,
		Description:  t.Description,
		Steps:        t.Steps,
		ResumeFrom:   lc.CurrentStep,
	}
	result, err := e.agentFn().Execute(ctx, req)
	if err != nil || !result.Success {
		errMsg := ""
		if err != nil {
			errMsg = err.Error()
		} else {
			errMsg = result.Error
		}
		lc.Error = fmt.Sprintf("agent execution: %s", errMsg)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		return
	}
	lc.AgentOutput = result.Output
	lc.ImplNotes = result.ImplementationNotes
	_ = e.store.UpdateLifecycle(ctx, lc)

	// Validate.
	if err := lifecycle.Transition(lc, lifecycle.StatusValidating); err != nil {
		lc.Error = fmt.Sprintf("transition to validating: %v", err)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		return
	}
	_ = e.store.UpdateLifecycle(ctx, lc)

	valResult, err := e.validator.Validate(ctx, *t, *lc)
	if err != nil {
		lc.Error = fmt.Sprintf("validation error: %v", err)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		return
	}
	if !valResult.Passed {
		details := formatCheckResults(valResult.Checks)
		lc.Error = fmt.Sprintf("validation failed: %s", details)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailedValidation)
		_ = e.store.UpdateLifecycle(ctx, lc)
		if lifecycle.CanRetry(lc, e.cfg.MaxRetries) {
			_ = e.worktrees.Remove(ctx, lc.TaskID)
			_ = lifecycle.Transition(lc, lifecycle.StatusReady)
			_ = e.store.UpdateLifecycle(ctx, lc)
			e.cond.Broadcast()
			return
		}
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		return
	}

	// Review.
	if err := lifecycle.Transition(lc, lifecycle.StatusReviewing); err != nil {
		lc.Error = fmt.Sprintf("transition to reviewing: %v", err)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		return
	}
	_ = e.store.UpdateLifecycle(ctx, lc)

	diff, err := getDiff(ctx, lc.WorktreePath, lc.BaseSHA)
	if err != nil {
		lc.Error = fmt.Sprintf("getting diff: %v", err)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		return
	}

	reviewReq := review.ReviewRequest{
		Task:         *t,
		Lifecycle:    *lc,
		Diff:         diff,
		WorktreePath: lc.WorktreePath,
	}
	revResult, err := e.reviewerFn(*t).Review(ctx, reviewReq)
	if err != nil {
		lc.Error = fmt.Sprintf("review error: %v", err)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		return
	}
	if !revResult.Approved {
		lc.Error = revResult.Feedback
		_ = lifecycle.Transition(lc, lifecycle.StatusRejected)
		_ = e.store.UpdateLifecycle(ctx, lc)
		if lifecycle.CanRetry(lc, e.cfg.MaxRetries) {
			_ = e.worktrees.Remove(ctx, lc.TaskID)
			_ = lifecycle.Transition(lc, lifecycle.StatusReady)
			_ = e.store.UpdateLifecycle(ctx, lc)
			e.cond.Broadcast()
			return
		}
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		return
	}

	// Merge (with conflict resolution loop).
	if err := lifecycle.Transition(lc, lifecycle.StatusMerging); err != nil {
		lc.Error = fmt.Sprintf("transition to merging: %v", err)
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		return
	}
	_ = e.store.UpdateLifecycle(ctx, lc)

	for {
		mergeReq := merge.MergeRequest{
			WorktreePath: lc.WorktreePath,
			Branch:       lc.Branch,
			TargetRef:    e.cfg.BaseRef,
			TaskID:       lc.TaskID,
		}
		mergeResult, err := e.merger.Merge(ctx, mergeReq)
		if err != nil {
			lc.Error = fmt.Sprintf("merge error: %v", err)
			_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
			_ = e.store.UpdateLifecycle(ctx, lc)
			_ = e.worktrees.Remove(ctx, lc.TaskID)
			return
		}

		if mergeResult.HasConflicts {
			_ = lifecycle.Transition(lc, lifecycle.StatusConflict)
			_ = e.store.UpdateLifecycle(ctx, lc)
			_ = lifecycle.Transition(lc, lifecycle.StatusResolving)
			_ = e.store.UpdateLifecycle(ctx, lc)

			if lc.ResolveAttempts >= e.cfg.MaxResolveAttempts {
				lc.Error = "max resolve attempts reached"
				_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
				_ = e.store.UpdateLifecycle(ctx, lc)
				_ = e.worktrees.Remove(ctx, lc.TaskID)
				return
			}
			lc.ResolveAttempts++
			lc.ConflictDetails = strings.Join(mergeResult.ConflictFiles, ", ")

			resolveReq := conflict.ResolveRequest{
				MergeWorkspace: lc.WorktreePath,
				Branch:         lc.Branch,
				TargetRef:      e.cfg.BaseRef,
				TaskDesc:       t.Description,
				ConflictFiles:  mergeResult.ConflictFiles,
			}
			resolveResult, err := e.resolver.Resolve(ctx, resolveReq)
			if err != nil || !resolveResult.Success {
				errMsg := ""
				if err != nil {
					errMsg = err.Error()
				} else {
					errMsg = resolveResult.Error
				}
				lc.Error = fmt.Sprintf("conflict resolution failed: %s", errMsg)
				_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
				_ = e.store.UpdateLifecycle(ctx, lc)
				_ = e.worktrees.Remove(ctx, lc.TaskID)
				return
			}
			// Transition back to merging and retry.
			_ = lifecycle.Transition(lc, lifecycle.StatusMerging)
			_ = e.store.UpdateLifecycle(ctx, lc)
			continue // retry merge
		}

		// Merge succeeded.
		lc.MergeCommitSHA = mergeResult.MergeCommitSHA
		_ = lifecycle.Transition(lc, lifecycle.StatusMerged)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID)
		break
	}

	// After merge, re-evaluate readiness and wake workers.
	e.mu.Lock()
	_ = e.evaluateReadiness(ctx)
	e.cond.Broadcast()
	e.mu.Unlock()
}

// getDiff returns the git diff between baseSHA and HEAD in the worktree.
func getDiff(ctx context.Context, worktreePath, baseSHA string) (string, error) {
	cmd := exec.CommandContext(ctx, "git", "-C", worktreePath, "diff", baseSHA+"..HEAD")
	out, err := cmd.CombinedOutput()
	if err != nil {
		return "", fmt.Errorf("git diff: %w: %s", err, string(out))
	}
	return string(out), nil
}

// formatCheckResults builds a human-readable summary of validation check results.
func formatCheckResults(checks []validate.CheckResult) string {
	var parts []string
	for _, c := range checks {
		if !c.Passed {
			parts = append(parts, fmt.Sprintf("%s: %s", c.Name, c.Detail))
		}
	}
	return strings.Join(parts, "; ")
}
