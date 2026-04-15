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
	"github.com/johnayoung/flywheel/internal/events"
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
	bus         events.Publisher
	runID       string

	mu                sync.Mutex
	cond              *sync.Cond
	workerState       map[string]workerSnapshot
	lastTransitionAt  time.Time
	heartbeatInterval time.Duration
}

// workerSnapshot is the observable state of a worker for heartbeat reporting.
type workerSnapshot struct {
	TaskID string
	Phase  string
}

// ErrDeadlocked is returned from Run when every non-terminal task is
// transitively blocked by a failed prerequisite and no worker can make
// progress. Operators resolve this with `flywheel retry --all-failed --force`.
var ErrDeadlocked = errors.New("run deadlocked: all non-terminal tasks have failed prerequisites")

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
	opts ...Option,
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
		bus:         events.NopPublisher{},
		workerState: make(map[string]workerSnapshot),
	}
	for _, opt := range opts {
		opt(e)
	}
	// Parse heartbeat interval; fall back to a sane default on empty/invalid.
	if cfg.HeartbeatInterval != "" {
		if d, err := time.ParseDuration(cfg.HeartbeatInterval); err == nil {
			e.heartbeatInterval = d
		}
	}
	if e.heartbeatInterval == 0 {
		e.heartbeatInterval = 30 * time.Second
	}
	e.cond = sync.NewCond(&e.mu)
	return e
}

// Option configures an Engine at construction time.
type Option func(*Engine)

// WithEventBus wires the engine to publish progress events to the given
// publisher. Defaults to events.NopPublisher{} when not set.
func WithEventBus(p events.Publisher) Option {
	return func(e *Engine) {
		if p != nil {
			e.bus = p
		}
	}
}

// publish is a tiny helper that stamps shared fields onto an Event and
// dispatches it through the engine's publisher.
func (e *Engine) publish(ev events.Event) {
	if ev.Timestamp.IsZero() {
		ev.Timestamp = time.Now()
	}
	if ev.RunID == "" {
		ev.RunID = e.runID
	}
	e.bus.Publish(ev)
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
	e.runID = runID

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

	// 4a. Prune stale worktrees and branches left by a crashed prior run so
	// Create doesn't collide with leftover state. Committed work on orphan
	// branches is archived under flywheel/archive/* rather than discarded.
	active := make(map[string]bool, len(tasks))
	for _, t := range tasks {
		active[t.ID] = true
	}
	if err := e.worktrees.PruneStale(ctx, active, e.cfg.BaseRef); err != nil {
		return nil, fmt.Errorf("pruning stale worktrees: %w", err)
	}

	// 4b. Auto-reset: revive tasks left in StatusFailed by prior runs, up to
	// the consecutive-run failure cap. This dissolves stale blockers without
	// operator intervention.
	if err := e.autoResetPriorRunFailures(ctx); err != nil {
		return nil, fmt.Errorf("auto-reset prior run failures: %w", err)
	}

	// 5. Crash recovery: reset any in-flight or interrupted states back to ready.
	inFlightStatuses := []lifecycle.Status{
		lifecycle.StatusRunning,
		lifecycle.StatusValidating,
		lifecycle.StatusReviewing,
		lifecycle.StatusMerging,
		lifecycle.StatusResolving,
		lifecycle.StatusInterrupted,
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
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
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

	// 6a. Emit run_started + plan_summary so operators see the shape of the run.
	e.publish(events.Event{Type: events.TypeRunStarted, Data: map[string]any{"tasks": len(tasks)}})
	e.emitPlanSummary(ctx)

	// 6b. Heartbeat: prove liveness while workers are active.
	hbCtx, hbCancel := context.WithCancel(ctx)
	defer hbCancel()
	go e.heartbeatLoop(hbCtx)

	// 7. Start workers.
	var wg sync.WaitGroup
	var deadlockOnce sync.Once
	var deadlocked bool
	for i := 0; i < e.maxParallel; i++ {
		wg.Add(1)
		workerID := fmt.Sprintf("worker-%d", i)
		go func() {
			defer wg.Done()
			if e.worker(ctx, workerID) {
				deadlockOnce.Do(func() { deadlocked = true })
			}
		}()
	}

	wg.Wait()
	hbCancel()

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
	e.publish(events.Event{
		Type: events.TypeRunCompleted,
		Data: map[string]any{
			"merged":  summary.Merged,
			"failed":  summary.Failed,
			"pending": summary.Pending,
		},
	})
	if deadlocked {
		return summary, ErrDeadlocked
	}
	return summary, nil
}

// autoResetPriorRunFailures scans for lifecycles in StatusFailed whose
// failures all came from prior runs (distinct RunIDs) and revives them when
// the cross-run failure count is below the configured cap. This is what
// turns "ran flywheel run again" into a working recovery path for 90% of
// stalled states without operator intervention.
func (e *Engine) autoResetPriorRunFailures(ctx context.Context) error {
	cap := e.cfg.ConsecutiveFailureCap
	if cap <= 0 {
		return nil
	}
	lcs, err := e.store.ListLifecycles(ctx, store.LifecycleFilter{
		Statuses: []lifecycle.Status{lifecycle.StatusFailed},
	})
	if err != nil {
		return fmt.Errorf("listing failed lifecycles: %w", err)
	}
	for _, lc := range lcs {
		priorFailures := lifecycle.ConsecutiveFailedRuns(&lc)
		if priorFailures >= cap {
			continue
		}
		// Failed is terminal; rebuild to StatusPending so normal readiness
		// evaluation picks it up (and dependents unblock as it advances).
		lcCopy := lc
		lifecycle.ResetForRetry(&lcCopy)
		// Bypass the transition table (failed is terminal by design) by
		// writing the status directly; this is an engine-level recovery action.
		lcCopy.Status = lifecycle.StatusPending
		lcCopy.Timestamps.FailedAt = nil
		if err := e.store.UpdateLifecycle(ctx, &lcCopy); err != nil {
			return fmt.Errorf("auto-reset update for %s: %w", lc.TaskID, err)
		}
		e.publish(events.Event{
			Type:   events.TypeTaskAutoReset,
			TaskID: lc.TaskID,
			Data: map[string]any{
				"reason":             "prior_run_failure",
				"prior_run_failures": priorFailures,
			},
		})
	}
	return nil
}

// emitPlanSummary publishes a plan_summary event summarizing current status.
func (e *Engine) emitPlanSummary(ctx context.Context) {
	lcs, err := e.store.ListLifecycles(ctx, store.LifecycleFilter{})
	if err != nil {
		return
	}
	statuses := make(map[string]lifecycle.Status, len(lcs))
	for _, lc := range lcs {
		statuses[lc.TaskID] = lc.Status
	}
	sum := e.checker.PlanSummary(statuses)
	e.publish(events.Event{
		Type: events.TypePlanSummary,
		Data: map[string]any{
			"total":   sum.Total,
			"ready":   sum.Ready,
			"blocked": sum.Blocked,
			"failed":  sum.Failed,
			"merged":  sum.Merged,
		},
	})
}

// heartbeatLoop emits run_heartbeat events while the engine is running.
// Suppressed when a phase transition occurred within the last 2 seconds so
// rapid-progress bursts don't drown the log.
func (e *Engine) heartbeatLoop(ctx context.Context) {
	t := time.NewTicker(e.heartbeatInterval)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			e.mu.Lock()
			recent := time.Since(e.lastTransitionAt) < 2*time.Second
			workers := make([]map[string]any, 0, len(e.workerState))
			for wid, snap := range e.workerState {
				workers = append(workers, map[string]any{
					"worker_id": wid,
					"task_id":   snap.TaskID,
					"phase":     snap.Phase,
				})
			}
			e.mu.Unlock()
			if recent {
				continue
			}
			e.publish(events.Event{
				Type: events.TypeRunHeartbeat,
				Data: map[string]any{"workers": workers},
			})
		}
	}
}

// setWorkerState records what a worker is currently doing (for heartbeats).
func (e *Engine) setWorkerState(workerID, taskID, phase string) {
	e.mu.Lock()
	e.workerState[workerID] = workerSnapshot{TaskID: taskID, Phase: phase}
	e.lastTransitionAt = time.Now()
	e.mu.Unlock()
}

// clearWorkerState removes a worker's entry (on task completion or exit).
func (e *Engine) clearWorkerState(workerID string) {
	e.mu.Lock()
	delete(e.workerState, workerID)
	e.mu.Unlock()
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
// context is cancelled. Returns true if the worker exited because the run is
// deadlocked (all non-terminal tasks blocked by failed prereqs).
func (e *Engine) worker(ctx context.Context, workerID string) bool {
	for {
		if ctx.Err() != nil {
			return false
		}

		lc, err := e.store.ClaimNextReady(ctx, workerID)
		if err != nil {
			if errors.Is(err, store.ErrNoReadyTasks) {
				if e.allTerminal(ctx) {
					return false
				}
				// No ready tasks + not all terminal: check for deadlock
				// before blocking. If every non-terminal task is blocked
				// by a failed prereq AND no worker is mid-task, we'll
				// never make progress.
				if e.isDeadlocked(ctx) {
					e.emitDeadlockedEvent(ctx)
					// Wake any siblings blocked in waitForReady so they can exit.
					e.mu.Lock()
					e.cond.Broadcast()
					e.mu.Unlock()
					return true
				}
				if err := e.waitForReady(ctx); err != nil {
					return false // context cancelled
				}
				continue
			}
			// Unexpected store error; back off briefly then retry.
			select {
			case <-ctx.Done():
				return false
			case <-time.After(100 * time.Millisecond):
				continue
			}
		}

		e.publish(events.Event{
			Type:     events.TypeTaskClaimed,
			TaskID:   lc.TaskID,
			WorkerID: workerID,
		})
		e.processTask(ctx, workerID, lc)
	}
}

// isDeadlocked returns true when every non-terminal task is transitively
// blocked by a StatusFailed prerequisite AND no worker is currently holding a
// task. This is the fallback the auto-reset pass couldn't cover (e.g., when
// a task has hit the consecutive-failure cap).
func (e *Engine) isDeadlocked(ctx context.Context) bool {
	e.mu.Lock()
	workersBusy := len(e.workerState) > 0
	e.mu.Unlock()
	if workersBusy {
		return false
	}
	lcs, err := e.store.ListLifecycles(ctx, store.LifecycleFilter{})
	if err != nil {
		return false
	}
	statuses := make(map[string]lifecycle.Status, len(lcs))
	hasNonTerminal := false
	for _, lc := range lcs {
		statuses[lc.TaskID] = lc.Status
		if !lifecycle.IsTerminal(lc.Status) {
			hasNonTerminal = true
		}
	}
	if !hasNonTerminal {
		return false
	}
	for _, lc := range lcs {
		if lifecycle.IsTerminal(lc.Status) {
			continue
		}
		if !e.checker.BlockedByFailed(lc.TaskID, statuses) {
			return false
		}
	}
	return true
}

// emitDeadlockedEvent publishes a run_deadlocked event listing the blocked
// tasks and their failing ancestors.
func (e *Engine) emitDeadlockedEvent(ctx context.Context) {
	lcs, err := e.store.ListLifecycles(ctx, store.LifecycleFilter{})
	if err != nil {
		return
	}
	var blocked, failed []string
	for _, lc := range lcs {
		switch lc.Status {
		case lifecycle.StatusFailed:
			failed = append(failed, lc.TaskID)
		default:
			if !lifecycle.IsTerminal(lc.Status) {
				blocked = append(blocked, lc.TaskID)
			}
		}
	}
	e.publish(events.Event{
		Type: events.TypeRunDeadlocked,
		Data: map[string]any{
			"blocked_tasks": blocked,
			"failed_tasks":  failed,
		},
	})
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

// phase transitions the lifecycle to `to`, persists, and emits a
// task_phase_changed event. Returns any transition error.
func (e *Engine) phase(ctx context.Context, lc *lifecycle.Lifecycle, workerID string, to lifecycle.Status) error {
	from := lc.Status
	if err := lifecycle.Transition(lc, to); err != nil {
		return err
	}
	_ = e.store.UpdateLifecycle(ctx, lc)
	e.setWorkerState(workerID, lc.TaskID, string(to))
	e.publish(events.Event{
		Type:     events.TypeTaskPhaseChanged,
		TaskID:   lc.TaskID,
		WorkerID: workerID,
		Attempt:  attemptNumber(lc),
		Data:     map[string]any{"from": string(from), "to": string(to)},
	})
	return nil
}

// startAttempt appends a new Attempt to the lifecycle and emits
// task_attempt_started.
func (e *Engine) startAttempt(ctx context.Context, lc *lifecycle.Lifecycle, workerID string) {
	att := lifecycle.Attempt{
		Number:    len(lc.Attempts) + 1,
		StartedAt: time.Now(),
		RunID:     e.runID,
	}
	lc.Attempts = append(lc.Attempts, att)
	_ = e.store.UpdateLifecycle(ctx, lc)
	e.publish(events.Event{
		Type:     events.TypeTaskAttemptStarted,
		TaskID:   lc.TaskID,
		WorkerID: workerID,
		Attempt:  att.Number,
	})
}

// endAttempt fills in the EndedAt + outcome for the latest Attempt and
// emits task_attempt_ended. Mirrors selected fields onto the top-level
// Lifecycle for back-compat with existing status/review commands.
func (e *Engine) endAttempt(ctx context.Context, lc *lifecycle.Lifecycle, workerID, outcome string, mutate func(*lifecycle.Attempt)) {
	if len(lc.Attempts) == 0 {
		return
	}
	now := time.Now()
	att := &lc.Attempts[len(lc.Attempts)-1]
	if att.EndedAt != nil {
		// Already ended; do not double-emit.
		return
	}
	att.EndedAt = &now
	att.Outcome = outcome
	if mutate != nil {
		mutate(att)
	}
	_ = e.store.UpdateLifecycle(ctx, lc)
	e.publish(events.Event{
		Type:     events.TypeTaskAttemptEnded,
		TaskID:   lc.TaskID,
		WorkerID: workerID,
		Attempt:  att.Number,
		Data:     map[string]any{"outcome": outcome},
	})
}

// attemptNumber returns the current attempt number (or 0 if none yet).
func attemptNumber(lc *lifecycle.Lifecycle) int {
	if len(lc.Attempts) == 0 {
		return 0
	}
	return lc.Attempts[len(lc.Attempts)-1].Number
}

// processTask executes the full pipeline for a single claimed task: agent
// execution, validation, review, merge, conflict resolution.
func (e *Engine) processTask(ctx context.Context, workerID string, lc *lifecycle.Lifecycle) {
	e.setWorkerState(workerID, lc.TaskID, string(lc.Status))
	defer e.clearWorkerState(workerID)
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

	// Open a new attempt record.
	e.startAttempt(ctx, lc, workerID)

	// Execute agent.
	req := agent.ExecutionRequest{
		WorktreePath: wt.Path,
		TaskID:       t.ID,
		Description:  t.Description,
		Steps:        t.Steps,
		ResumeFrom:   lc.CurrentStep,
	}
	result, err := e.agentFn().Execute(ctx, req)

	// Distinguish context cancellation (operator interrupt / shutdown) from
	// genuine agent failure. Cancellation must NOT consume the retry budget
	// and must NOT poison successful work.
	if ctx.Err() != nil {
		lc.Error = fmt.Sprintf("agent execution interrupted: %v", ctx.Err())
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeCancelled, func(a *lifecycle.Attempt) {
			a.Error = lc.Error
			if result != nil {
				a.AgentOutput = result.Output
			}
		})
		_ = lifecycle.Transition(lc, lifecycle.StatusInterrupted)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(context.Background(), lc.TaskID, e.cfg.BaseRef)
		return
	}

	if err != nil || !result.Success {
		errMsg := ""
		if err != nil {
			errMsg = err.Error()
		} else {
			errMsg = result.Error
		}
		lc.Error = fmt.Sprintf("agent execution: %s", errMsg)
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeAgentError, func(a *lifecycle.Attempt) {
			a.Error = lc.Error
			if result != nil {
				a.AgentOutput = result.Output
			}
		})
		// Treat agent errors as retryable: route through StatusFailedValidation
		// (the only retryable terminal-ish state on the agent side) so that
		// CanRetry's existing semantics apply.
		_ = lifecycle.Transition(lc, lifecycle.StatusFailedValidation)
		_ = e.store.UpdateLifecycle(ctx, lc)
		if lifecycle.CanRetry(lc, e.cfg.MaxRetries) {
			_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
			_ = lifecycle.Transition(lc, lifecycle.StatusReady)
			_ = e.store.UpdateLifecycle(ctx, lc)
			e.cond.Broadcast()
			return
		}
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
		return
	}
	lc.AgentOutput = result.Output
	lc.ImplNotes = result.ImplementationNotes
	if n := len(lc.Attempts); n > 0 {
		lc.Attempts[n-1].AgentOutput = result.Output
	}
	_ = e.store.UpdateLifecycle(ctx, lc)

	// Validate.
	if err := e.phase(ctx, lc, workerID, lifecycle.StatusValidating); err != nil {
		lc.Error = fmt.Sprintf("transition to validating: %v", err)
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeInternalError, func(a *lifecycle.Attempt) { a.Error = lc.Error })
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
		return
	}

	valResult, err := e.validator.Validate(ctx, *t, *lc)
	if err != nil {
		lc.Error = fmt.Sprintf("validation error: %v", err)
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeInternalError, func(a *lifecycle.Attempt) { a.Error = lc.Error })
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
		return
	}
	if !valResult.Passed {
		details := formatCheckResults(valResult.Checks)
		lc.Error = fmt.Sprintf("validation failed: %s", details)
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeValidationFailed, func(a *lifecycle.Attempt) {
			a.Error = lc.Error
			a.ValidationFailures = details
		})
		_ = lifecycle.Transition(lc, lifecycle.StatusFailedValidation)
		_ = e.store.UpdateLifecycle(ctx, lc)
		if lifecycle.CanRetry(lc, e.cfg.MaxRetries) {
			_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
			_ = lifecycle.Transition(lc, lifecycle.StatusReady)
			_ = e.store.UpdateLifecycle(ctx, lc)
			e.cond.Broadcast()
			return
		}
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
		return
	}

	// Review.
	if err := e.phase(ctx, lc, workerID, lifecycle.StatusReviewing); err != nil {
		lc.Error = fmt.Sprintf("transition to reviewing: %v", err)
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeInternalError, func(a *lifecycle.Attempt) { a.Error = lc.Error })
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
		return
	}

	diff, err := getDiff(ctx, lc.WorktreePath, lc.BaseSHA)
	if err != nil {
		lc.Error = fmt.Sprintf("getting diff: %v", err)
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeInternalError, func(a *lifecycle.Attempt) { a.Error = lc.Error })
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
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
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeInternalError, func(a *lifecycle.Attempt) { a.Error = lc.Error })
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
		return
	}
	if !revResult.Approved {
		lc.Error = revResult.Feedback
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeRejected, func(a *lifecycle.Attempt) {
			a.ReviewFeedback = revResult.Feedback
			a.Error = revResult.Feedback
		})
		_ = lifecycle.Transition(lc, lifecycle.StatusRejected)
		_ = e.store.UpdateLifecycle(ctx, lc)
		if lifecycle.CanRetry(lc, e.cfg.MaxRetries) {
			_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
			_ = lifecycle.Transition(lc, lifecycle.StatusReady)
			_ = e.store.UpdateLifecycle(ctx, lc)
			e.cond.Broadcast()
			return
		}
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
		return
	}

	// Merge (with conflict resolution loop).
	if err := e.phase(ctx, lc, workerID, lifecycle.StatusMerging); err != nil {
		lc.Error = fmt.Sprintf("transition to merging: %v", err)
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeInternalError, func(a *lifecycle.Attempt) { a.Error = lc.Error })
		_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
		return
	}

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
			e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeMergeError, func(a *lifecycle.Attempt) { a.Error = lc.Error })
			_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
			_ = e.store.UpdateLifecycle(ctx, lc)
			_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
			return
		}

		if mergeResult.HasConflicts {
			_ = e.phase(ctx, lc, workerID, lifecycle.StatusConflict)
			_ = e.phase(ctx, lc, workerID, lifecycle.StatusResolving)

			if lc.ResolveAttempts >= e.cfg.MaxResolveAttempts {
				lc.Error = "max resolve attempts reached"
				e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeMergeConflictFail, func(a *lifecycle.Attempt) { a.Error = lc.Error })
				_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
				_ = e.store.UpdateLifecycle(ctx, lc)
				_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
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
				e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeMergeConflictFail, func(a *lifecycle.Attempt) { a.Error = lc.Error })
				_ = lifecycle.Transition(lc, lifecycle.StatusFailed)
				_ = e.store.UpdateLifecycle(ctx, lc)
				_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
				return
			}
			// Transition back to merging and retry.
			_ = e.phase(ctx, lc, workerID, lifecycle.StatusMerging)
			continue // retry merge
		}

		// Merge succeeded.
		lc.MergeCommitSHA = mergeResult.MergeCommitSHA
		e.endAttempt(ctx, lc, workerID, lifecycle.OutcomeSucceeded, func(a *lifecycle.Attempt) {})
		_ = lifecycle.Transition(lc, lifecycle.StatusMerged)
		_ = e.store.UpdateLifecycle(ctx, lc)
		_ = e.worktrees.Remove(ctx, lc.TaskID, e.cfg.BaseRef)
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
