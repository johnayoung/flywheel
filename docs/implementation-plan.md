# Flywheel — Implementation Plan

## Context

Flywheel is a worktree-based parallel agent orchestrator with dependency resolution, merge conflict handling, and a review pipeline. It coordinates AI coding agents (Claude Code for MVP) working in isolated git worktrees, scheduled by a DAG-aware engine.

**Read `docs/design.md` first** — it contains the full architecture, data models, state machine, and design decisions. This document is the step-by-step build plan.

**Language:** Go 1.26+
**CLI framework:** Cobra
**Module path:** `github.com/johnayoung/flywheel`

---

## Principles

- Each milestone is a self-contained, testable unit. Commit after each one.
- Interfaces first, implementations second. Every major component has an interface so backends/agents are swappable.
- Tests are not optional. Every milestone includes what to test.
- No dummy implementations. Every function does real work.
- Minimize dependencies. Standard library preferred. Only pull in third-party packages when justified.

---

## Directory Structure

```
flywheel/
├── cmd/
│   └── flywheel/
│       └── main.go
├── internal/
│   ├── config/
│   │   ├── config.go          # Run configuration model + loader
│   │   └── config_test.go
│   ├── task/
│   │   ├── task.go            # Task model (the immutable work definition)
│   │   ├── parser.go          # Parse tasks from JSON, JSONL, directory of JSON files
│   │   └── parser_test.go
│   ├── lifecycle/
│   │   ├── lifecycle.go       # Lifecycle model + status constants
│   │   ├── machine.go         # State machine: valid transitions, transition logic
│   │   └── machine_test.go
│   ├── store/
│   │   ├── store.go           # TaskStore interface
│   │   ├── errors.go          # Sentinel errors (ErrNoReadyTasks, ErrTaskLocked, etc.)
│   │   └── jsonl/
│   │       ├── store.go       # JSONL/file-based implementation
│   │       └── store_test.go
│   ├── dag/
│   │   ├── dag.go             # DAG construction from tasks
│   │   ├── scheduler.go       # Topo sort, readiness evaluation, wave computation
│   │   └── dag_test.go
│   ├── worktree/
│   │   ├── worktree.go        # Git worktree lifecycle (create, remove, list)
│   │   └── worktree_test.go
│   ├── agent/
│   │   ├── agent.go           # Agent interface
│   │   └── claudecode/
│   │       ├── claude.go      # Claude Code implementation (subprocess)
│   │       └── claude_test.go
│   ├── validate/
│   │   ├── validate.go        # Post-agent validation pipeline
│   │   └── validate_test.go
│   ├── review/
│   │   ├── review.go          # Review interface + agent reviewer
│   │   ├── hitl.go            # Human-in-the-loop reviewer (blocks for input)
│   │   └── review_test.go
│   ├── merge/
│   │   ├── merge.go           # Merge interface + strategies
│   │   ├── sequential.go      # Sequential merge implementation
│   │   └── merge_test.go
│   ├── conflict/
│   │   ├── resolver.go        # Conflict detection + agent-based resolution
│   │   └── resolver_test.go
│   └── engine/
│       ├── engine.go          # Main orchestration loop (ties everything together)
│       └── engine_test.go
├── flywheel.json              # Example run configuration
├── tasks/                     # Example task files
│   └── example-task.json
├── go.mod
└── go.sum
```

---

## Milestone 1: Project Scaffold + Core Models + Config

**Goal:** Go module initialized, core data types defined, run configuration loading, basic serialization working.

### 1a. Initialize the project

```bash
go mod init github.com/johnayoung/flywheel
```

Create the directory structure above (empty files are fine for directories that come later).

### 1b. Task model — `internal/task/task.go`

Define the `Task` struct. This is the immutable work definition authored by humans.

```go
type Task struct {
    ID                 string   `json:"id"`
    Description        string   `json:"description"`
    Category           string   `json:"category"`            // feat, fix, refactor, test, docs, chore
    Priority           int      `json:"priority"`             // lower = higher priority
    Prerequisites      []string `json:"prerequisites"`

    Commit             string   `json:"commit"`               // commit message template
    Steps              []string `json:"steps"`
    AcceptanceCriteria []string `json:"acceptance_criteria"`

    Review             string   `json:"review,omitempty"`     // "agent", "human", "none" — per-task override, falls back to config default
}
```

Include a `Validate() error` method that checks:
- `ID` is non-empty, no whitespace
- `Description` is non-empty
- `Category` is one of the allowed values
- `Commit` is non-empty
- `Steps` has at least one entry
- `Review`, if set, is one of `"agent"`, `"human"`, `"none"`
- No self-referencing prerequisites (ID not in its own Prerequisites)

### 1c. Task parser — `internal/task/parser.go`

Support three input formats:

1. **Single JSON file**: one task object → `ParseFile(path string) (*Task, error)`
2. **Directory of JSON files**: `ParseDir(dir string) ([]Task, error)` — reads all `.json` files
3. **JSONL file**: one task per line → `ParseJSONL(path string) ([]Task, error)`

All parsers should call `Validate()` on each parsed task and return clear errors with the task ID and file path.

### 1d. Lifecycle model — `internal/lifecycle/lifecycle.go`

```go
type Status string

const (
    StatusPending          Status = "pending"
    StatusReady            Status = "ready"
    StatusRunning          Status = "running"
    StatusValidating       Status = "validating"
    StatusFailedValidation Status = "failed_validation"
    StatusReviewing        Status = "reviewing"
    StatusRejected         Status = "rejected"
    StatusMerging          Status = "merging"
    StatusConflict         Status = "conflict"
    StatusResolving        Status = "resolving"
    StatusMerged           Status = "merged"
    StatusFailed           Status = "failed"
)

type Lifecycle struct {
    TaskID          string     `json:"task_id"`
    RunID           string     `json:"run_id"`
    WorkerID        string     `json:"worker_id,omitempty"`

    Status          Status     `json:"status"`
    Branch          string     `json:"branch,omitempty"`
    WorktreePath    string     `json:"worktree_path,omitempty"`
    BaseRef         string     `json:"base_ref"`
    BaseSHA         string     `json:"base_sha,omitempty"`

    CurrentStep     int        `json:"current_step"`
    StepsCompleted  []string   `json:"steps_completed,omitempty"`

    Timestamps      Timestamps `json:"timestamps"`

    Version           int        `json:"version"`            // optimistic concurrency — incremented on every write
    Retries           int        `json:"retries"`
    ResolveAttempts   int        `json:"resolve_attempts"`   // conflict resolution attempts, compared against config.MaxResolveAttempts
    Error           string     `json:"error,omitempty"`
    AgentOutput     string     `json:"agent_output,omitempty"`
    ImplNotes       string     `json:"implementation_notes,omitempty"`
    ReviewResult    string     `json:"review_result,omitempty"`    // JSON-serialized review.ReviewResult
    MergeCommitSHA  string     `json:"merge_commit_sha,omitempty"`
    ConflictDetails string     `json:"conflict_details,omitempty"`
}

type Timestamps struct {
    CreatedAt   *time.Time `json:"created_at,omitempty"`
    ReadyAt     *time.Time `json:"ready_at,omitempty"`
    StartedAt   *time.Time `json:"started_at,omitempty"`
    CompletedAt *time.Time `json:"completed_at,omitempty"`
    ReviewedAt  *time.Time `json:"reviewed_at,omitempty"`
    MergedAt    *time.Time `json:"merged_at,omitempty"`
    FailedAt    *time.Time `json:"failed_at,omitempty"`    // set when transitioning to failed (terminal)
}
```

Add a `NewLifecycle(taskID, runID, baseRef string) *Lifecycle` constructor that initializes with `StatusPending` and sets `CreatedAt`.

### 1e. State machine — `internal/lifecycle/machine.go`

Define the valid transitions as a map:

```go
var validTransitions = map[Status][]Status{
    StatusPending:          {StatusReady},
    StatusReady:            {StatusRunning},
    StatusRunning:          {StatusValidating, StatusFailed, StatusReady},       // StatusReady = crash recovery
    StatusValidating:       {StatusReviewing, StatusFailedValidation, StatusReady}, // StatusReady = crash recovery
    StatusFailedValidation: {StatusReady, StatusFailed},
    StatusReviewing:        {StatusMerging, StatusRejected, StatusReady},        // StatusReady = crash recovery
    StatusRejected:         {StatusReady, StatusFailed},
    StatusMerging:          {StatusMerged, StatusConflict, StatusReady},         // StatusReady = crash recovery
    StatusConflict:         {StatusResolving},
    StatusResolving:        {StatusMerging, StatusFailed, StatusReady},           // StatusFailed = unresolvable conflict, StatusReady = crash recovery
    StatusMerged:           {}, // terminal
    StatusFailed:           {}, // terminal
}
```

Implement:
- `Transition(lc *Lifecycle, to Status) error` — validates the transition is allowed, updates the status, sets the appropriate timestamp. Returns error if transition is invalid.
- `CanTransition(from, to Status) bool`
- `IsTerminal(s Status) bool`
- `CanRetry(lc *Lifecycle, maxRetries int) bool` — checks if retries < maxRetries and status is one of `failed_validation` or `rejected`. Note: `failed` is terminal and not retryable.

The `Transition` function must also handle side effects:
- When transitioning from `failed_validation` or `rejected` back to `ready`: increment `Retries` and clear the `Error` field.
- When transitioning to `failed` or `failed_validation`: require an error message to be set on the lifecycle before the transition. Set `FailedAt` timestamp.
- When transitioning from `resolving` back to `merging`: handled by the engine after successful conflict resolution (engine manages `ResolveAttempts` directly).

### 1f. Run configuration — `internal/config/config.go`

```go
type Config struct {
    Version      string      `json:"version"`
    Repo         string      `json:"repo"`           // path to git repo, "." default
    BaseRef      string      `json:"base_ref"`       // "main" default
    BranchPrefix string      `json:"branch_prefix"`  // "flywheel/" default
    MaxParallel  int         `json:"max_parallel"`    // 3 default
    BuildCommand string      `json:"build_command"`   // e.g. "go build ./..." — empty = skip
    Store        StoreConfig `json:"store"`

    MergeStrategy string     `json:"merge_strategy"` // "sequential" default
    Review        string     `json:"review"`         // "agent" default — task-level review overrides this
    Agent         string     `json:"agent"`          // "claude-code" default
    Timeout             string `json:"timeout"`               // "30m" default
    MaxRetries          int    `json:"max_retries"`           // 2 default
    MaxResolveAttempts  int    `json:"max_resolve_attempts"`  // 2 default — caps conflict resolution retries before failing
}

type StoreConfig struct {
    Backend string          `json:"backend"` // "jsonl", "sqlite", "postgres"
    Options json.RawMessage `json:"options"` // backend-specific; parsed by the backend
}
```

The only task-level override is `review` (whether a human needs to look at a specific task is a property of the task, not the system). All other operational settings are config-level only.

Implement:
- `Load(path string) (*Config, error)` — reads and parses the config file.
- `LoadWithDefaults() *Config` — returns config with all defaults set (for when no config file exists).
- `Validate(cfg *Config) error` — checks required fields, valid enum values. Rejects unsupported backends/strategies in MVP: `store.backend` must be `"jsonl"`, `merge_strategy` must be `"sequential"`. Others return a clear "not yet implemented" error.
- `ReviewModeForTask(cfg Config, t task.Task) string` — returns the task's review mode if set, otherwise falls back to config default.

### 1g. Tests

- `task/parser_test.go`: test parsing each format, test validation catches bad tasks, test a valid complex task round-trips correctly.
- `lifecycle/machine_test.go`: test every valid transition, test every invalid transition is rejected, test the full happy-path sequence (pending -> ... -> merged), test retry logic, test crash recovery transitions (running/validating/reviewing/merging/resolving -> ready).
- `config/config_test.go`: test loading from JSON, test defaults are applied, test validation rejects bad config, test `ReviewModeForTask` fallback logic.

### Commit
```
feat: add core models, state machine, and run configuration
```

---

## Milestone 2: Store Interface + JSONL Implementation

**Goal:** Pluggable storage layer with a working file-based implementation.

### 2a. Store interface — `internal/store/store.go`

```go
type TaskStore interface {
    // Task definitions (read path)
    ListTasks(ctx context.Context, filter TaskFilter) ([]task.Task, error)
    GetTask(ctx context.Context, id string) (*task.Task, error)

    // Lifecycle management (read-write, with locking)
    CreateLifecycle(ctx context.Context, lc *lifecycle.Lifecycle) error
    GetLifecycle(ctx context.Context, taskID string) (*lifecycle.Lifecycle, error)
    UpdateLifecycle(ctx context.Context, lc *lifecycle.Lifecycle) error
    ListLifecycles(ctx context.Context, filter LifecycleFilter) ([]lifecycle.Lifecycle, error)

    // Atomic claim — finds highest-priority ready task, locks it, returns lifecycle
    ClaimNextReady(ctx context.Context, workerID string) (*lifecycle.Lifecycle, error)
}

type TaskFilter struct {
    IDs        []string
    Categories []string
    HasStatus  []lifecycle.Status // join filter: tasks whose lifecycle matches these statuses
}

type LifecycleFilter struct {
    Statuses []lifecycle.Status
    WorkerID string
    RunID    string
}
```

### 2b. Sentinel errors — `internal/store/errors.go`

```go
var (
    ErrTaskNotFound   = errors.New("task not found")
    ErrNoReadyTasks   = errors.New("no ready tasks available")
    ErrLifecycleExists = errors.New("lifecycle already exists for task")
    ErrStaleWrite     = errors.New("lifecycle was modified by another process")
)
```

### 2c. JSONL file store — `internal/store/jsonl/store.go`

This is a file-based implementation (named `"jsonl"` in config because tasks can be authored as JSONL, though lifecycle storage uses individual JSON files):
- Tasks: read from a configurable path (directory of `.json` files, single `.jsonl` file, etc.) — reuse the task parser from milestone 1.
- Lifecycles: one JSON file per task in a lifecycle directory (e.g., `.flywheel/lifecycle/{task_id}.json`).

Key implementation details:

**Task loading**: Tasks are loaded once at initialization (or on demand with caching). They are read-only.

**Lifecycle locking**: Use file locking (via `github.com/gofrs/flock` for cross-platform support) on each lifecycle file for atomic reads and writes. For `ClaimNextReady`:
1. List all lifecycle files.
2. Sort by priority (join with task data).
3. For each `ready` lifecycle, attempt to acquire an exclusive flock.
4. If acquired: read → verify still `ready` → update to `running` with workerID → write → release lock → return.
5. If lock fails (another worker got it): skip, try next.

**File format for lifecycle files**: Pretty-printed JSON for human readability and easy `jq` queries.

**Stale write protection**: Use the `Version` field (integer) on the `Lifecycle` struct. Increment on every write. On update, verify the on-disk version matches the in-memory version before writing. If it doesn't, return `ErrStaleWrite`.

### 2d. Tests — `internal/store/jsonl/store_test.go`

- Test `ListTasks` with a directory of test fixture JSON files.
- Test `CreateLifecycle` + `GetLifecycle` round-trip.
- Test `UpdateLifecycle` with stale write detection.
- Test `ClaimNextReady` returns highest-priority ready task.
- Test `ClaimNextReady` with no ready tasks returns `ErrNoReadyTasks`.
- Test concurrent `ClaimNextReady` calls (use goroutines) — verify no task is double-claimed.

### Commit
```
feat: add TaskStore interface and JSONL file-based implementation
```

---

## Milestone 3: DAG Construction + Scheduling Logic

**Goal:** Build the dependency graph from tasks, detect cycles, compute readiness, and determine execution order.

### 3a. DAG — `internal/dag/dag.go`

```go
type DAG struct {
    nodes map[string]*Node
    edges map[string][]string // task ID → list of dependent task IDs (forward edges)
    deps  map[string][]string // task ID → list of prerequisite task IDs (reverse edges)
}

type Node struct {
    TaskID   string
    Priority int
}
```

Implement:
- `Build(tasks []task.Task) (*DAG, error)` — constructs the graph from task prerequisites. Returns error if:
  - A prerequisite references a task ID that doesn't exist.
  - A cycle is detected (use Kahn's algorithm or DFS-based cycle detection).
- `Prerequisites(taskID string) []string` — returns direct prerequisites.
- `Dependents(taskID string) []string` — returns tasks that depend on this one.
- `Roots() []string` — returns tasks with no prerequisites (wave 1 candidates).

### 3b. Scheduler logic — `internal/dag/scheduler.go`

```go
type ReadinessChecker struct {
    dag *DAG
}
```

Implement:
- `IsReady(taskID string, statuses map[string]lifecycle.Status) bool` — returns true if ALL prerequisites of `taskID` have status `merged`. This is the DAG-aware readiness check.
- `ReadyTasks(statuses map[string]lifecycle.Status) []string` — returns all task IDs that are currently `pending` and whose prerequisites are all `merged`. Sorted by priority.
- `ComputeWaves(statuses map[string]lifecycle.Status) [][]string` — computes theoretical execution waves (for `flywheel init` preview). Wave 1 = roots, wave 2 = tasks whose prereqs are all in wave 1, etc. This is a preview — actual execution is dynamic, not wave-locked.

### 3c. Tests — `internal/dag/dag_test.go`

- Build a valid DAG, verify structure.
- Detect a cycle (A → B → C → A), verify error.
- Detect a missing prerequisite, verify error.
- Test `IsReady` with various status maps.
- Test `ReadyTasks` returns correct tasks in priority order.
- Test `ComputeWaves` with a known graph, verify wave assignment.

### Commit
```
feat: add DAG construction with cycle detection and readiness scheduling
```

---

## Milestone 4: Git Worktree Management

**Goal:** Create, list, and clean up git worktrees. This is the isolation primitive.

### 4a. Worktree manager — `internal/worktree/worktree.go`

```go
type Manager struct {
    repoRoot     string // path to the git repository
    worktreeBase string // base directory for worktrees (e.g., .flywheel/worktrees/)
    branchPrefix string // e.g., "flywheel/"
}
```

Implement (all functions shell out to `git` via `os/exec`):

- `Create(ctx context.Context, taskID string, baseRef string) (*Worktree, error)`:
  1. Branch name: `{branchPrefix}{taskID}` (e.g., `flywheel/INF-0252`).
  2. Worktree path: `{worktreeBase}/{taskID}`.
  3. Run `git worktree add -b {branch} {path} {baseRef}`.
  4. Return the worktree info (path, branch, base SHA from `git rev-parse {baseRef}`).

- `Remove(ctx context.Context, taskID string) error`:
  1. Run `git worktree remove {path} --force`.
  2. Run `git branch -D {branch}` (clean up the branch).

- `List(ctx context.Context) ([]Worktree, error)`:
  1. Run `git worktree list --porcelain`.
  2. Parse output into structured data.

- `IsClean(ctx context.Context, path string) (bool, error)`:
  1. Run `git -C {path} status --porcelain`.
  2. Return true if output is empty.

- `GetCommits(ctx context.Context, path string, baseRef string) ([]Commit, error)`:
  1. Run `git -C {path} log --oneline {baseRef}..HEAD`.
  2. Parse into structured commit data.

```go
type Worktree struct {
    TaskID  string
    Path    string
    Branch  string
    BaseSHA string
}

type Commit struct {
    SHA     string
    Message string
}
```

All `git` commands should capture stderr for error reporting. Use `exec.CommandContext` for timeout support.

### 4b. Tests — `internal/worktree/worktree_test.go`

Tests need a real git repo. Use `t.TempDir()` to create a temp directory, `git init`, make an initial commit, then test:

- Create a worktree, verify directory exists and branch exists.
- Create two worktrees from the same base, verify they're independent.
- Remove a worktree, verify directory and branch are gone.
- List worktrees, verify count.
- IsClean on a clean worktree returns true.
- IsClean after creating an untracked file returns false.
- GetCommits returns commits made in the worktree.

### Commit
```
feat: add git worktree lifecycle management
```

---

## Milestone 5: Agent Interface + Claude Code Implementation

**Goal:** Define the agent abstraction and implement the Claude Code backend.

### 5a. Agent interface — `internal/agent/agent.go`

```go
type Agent interface {
    Execute(ctx context.Context, req ExecutionRequest) (*ExecutionResult, error)
}

type ExecutionRequest struct {
    WorktreePath string
    TaskID       string
    Description  string
    Steps        []string
    ResumeFrom   int      // 0 = start from beginning, N = resume from step N (used on retry, not crash recovery)
}

type ExecutionResult struct {
    Success             bool
    StepsCompleted      int
    Output              string   // full agent output/logs
    ImplementationNotes string   // agent's summary of what it did
    Error               string   // error message if failed
}
```

### 5b. Claude Code implementation — `internal/agent/claudecode/claude.go`

Claude Code is invoked as a CLI subprocess: `claude --dangerously-skip-permissions -p "..." --output-format json`.

**Note:** `--dangerously-skip-permissions` is required because agents run non-interactively in worktrees and must edit files / run commands without interactive approval prompts. This is safe because each agent operates in an isolated worktree with no access to the main working tree.

Key implementation:

- **Prompt construction**: Build the prompt from the `ExecutionRequest`. Include:
  - The task description
  - The steps (with "resume from step N" context if resuming)
  - Instruction to commit work with the expected commit message
  - Instruction to not modify files outside the scope

- **Subprocess management**:
  - Set `WorkDir` to the worktree path.
  - Capture stdout and stderr.
  - Respect context cancellation (kill the subprocess on ctx.Done()).
  - Parse the JSON output to extract the result.

- **Output parsing**: Claude Code's `--output-format json` returns structured output. Parse this to determine success/failure and extract the response text.

- **Timeout handling**: The context should carry the task's timeout. If the subprocess doesn't complete in time, kill it and return an error.

**Important**: Look up the current Claude Code CLI interface before implementing. The exact flags and output format may have changed. Use `claude --help` or check the docs.

### 5c. Tests — `internal/agent/claudecode/claude_test.go`

- Test prompt construction produces correct output for various inputs.
- Test with a mock executable (a shell script that echoes JSON) to verify subprocess management.
- Test context cancellation kills the subprocess.
- Test timeout handling.

### Commit
```
feat: add agent interface and Claude Code subprocess implementation
```

---

## Milestone 6: Validation Pipeline

**Goal:** Post-agent checks that verify the work is actually correct before it enters review.

### 6a. Validator — `internal/validate/validate.go`

```go
type Validator struct {
    worktreeMgr  *worktree.Manager
    agent        agent.Agent // for acceptance criteria checking (optional)
    buildCommand string      // from config.BuildCommand — empty = skip build check
}

type ValidationResult struct {
    Passed   bool
    Checks   []CheckResult
    Error    string
}

type CheckResult struct {
    Name    string // e.g., "clean_worktree", "commit_message", "build", "acceptance_criteria"
    Passed  bool
    Detail  string
}
```

Implement `Validate(ctx context.Context, t task.Task, lc lifecycle.Lifecycle) (*ValidationResult, error)`:

1. **Clean worktree check**: Run `git -C {worktree} status --porcelain`. Must be empty.
2. **Commit exists check**: Run `git -C {worktree} log --oneline {baseSHA}..HEAD`. Must have >= 1 commit.
3. **Commit message check**: The earliest commit on the branch (first in `git log --reverse {baseSHA}..HEAD`) must start with `t.Commit`. Subsequent commits are unconstrained — agents may make multiple commits during implementation.
4. **Build check** (if `buildCommand` is non-empty): Run the build command (e.g., `go build ./...`) in the worktree. Must exit 0.
5. **Acceptance criteria check** (if agent available and criteria exist): Construct a prompt with the git diff (`git -C {worktree} diff {baseSHA}..HEAD`) and the acceptance criteria. Ask the agent to evaluate each criterion as pass/fail with reasoning. Parse the response.

Each check runs in order. If any check fails, the remaining checks still run (collect all failures), but the overall result is `Passed: false`.

### 6b. Tests — `internal/validate/validate_test.go`

Use a temp git repo with worktrees:
- Test all checks pass on a valid worktree with correct commit.
- Test failure on dirty worktree.
- Test failure on missing commit.
- Test failure on wrong commit message.
- Test build failure (use a command like `false` to simulate).

### Commit
```
feat: add post-agent validation pipeline
```

---

## Milestone 7: Review Pipeline

**Goal:** Agent-based review and human-in-the-loop approval gates.

### 7a. Reviewer interface — `internal/review/review.go`

```go
type Reviewer interface {
    Review(ctx context.Context, req ReviewRequest) (*ReviewResult, error)
}

type ReviewRequest struct {
    Task         task.Task
    Lifecycle    lifecycle.Lifecycle
    Diff         string // git diff output
    WorktreePath string
}

type ReviewResult struct {
    Approved    bool
    Feedback    string   // reviewer's comments
    Suggestions []string // non-blocking suggestions
}
```

### 7b. Agent reviewer — `internal/review/review.go` (or separate file)

Implement `AgentReviewer` that uses the `agent.Agent` interface:
- Constructs a review prompt with the diff, task description, acceptance criteria, and instructions to review for code quality, correctness, and adherence to the task.
- Parses the agent's response to extract approved/rejected + feedback.
- Non-blocking suggestions are captured but don't affect the approval decision.

### 7c. HITL reviewer — `internal/review/hitl.go`

Implement `HITLReviewer`:
- Prints the diff summary to stdout.
- Prints a URL if PR mode is enabled.
- Blocks waiting for user input on stdin: `[a]pprove / [r]eject / [d]iff / [s]kip`
  - `a`: approved
  - `r`: rejected (prompt for reason)
  - `d`: show full diff (page through with less)
  - `s`: skip (leave in reviewing state for later)
- **Concurrency:** Multiple workers may reach HITL review simultaneously. Serialize stdin prompts with a mutex to prevent garbled I/O. Workers block on the mutex until it's their turn to prompt.

### 7d. Tests

- Agent reviewer: mock the agent, verify prompt construction and response parsing.
- HITL reviewer: test with a bytes.Buffer as stdin/stdout to simulate user input.

### Commit
```
feat: add review pipeline with agent and HITL reviewers
```

---

## Milestone 8: Merge Engine + Conflict Resolution

**Goal:** Merge completed branches back to base ref with conflict detection and agent-based resolution.

### 8a. Merger interface — `internal/merge/merge.go`

```go
type Merger interface {
    Merge(ctx context.Context, req MergeRequest) (*MergeResult, error)
}

type MergeRequest struct {
    WorktreePath string
    Branch       string
    TargetRef    string // e.g., "main"
    TaskID       string
}

type MergeResult struct {
    Success        bool
    MergeCommitSHA string
    HasConflicts   bool
    ConflictFiles  []string
}
```

### 8b. Sequential merger — `internal/merge/sequential.go`

This is the default strategy. Merges happen in a dedicated temporary worktree, not the user's main working directory.

Implementation:

1. **Acquire merge lock**: Use a file lock (`.flywheel/merge.lock`) to ensure only one merge at a time.
2. **Create merge worktree**: `git worktree add {.flywheel/merge-workspace} {targetRef}`. Reuse if it already exists (just `git checkout {targetRef} && git reset --hard {targetRef}`).
3. **Attempt merge**: Run `git -C {merge-worktree} merge {branch} --no-ff -m "Merge {branch}"`.
4. **If clean**: Record the merge commit SHA. Update the real ref: `git update-ref refs/heads/{targetRef} {mergeCommitSHA}`. Return success.
5. **If conflicts**: Parse `git -C {merge-worktree} diff --name-only --diff-filter=U` for conflict files. Abort with `git -C {merge-worktree} merge --abort`. Return `HasConflicts: true` with the list.
6. **Release lock** (in defer).

This avoids touching the user's working directory entirely. The merge worktree is a long-lived workspace under `.flywheel/` that is reused across merges.

### 8c. Conflict resolver — `internal/conflict/resolver.go`

```go
type Resolver struct {
    agent agent.Agent
}
```

Implement `Resolve(ctx context.Context, req ResolveRequest) (*ResolveResult, error)`:

1. Get the conflict markers: `git -C {merge-worktree} diff` shows the conflict markers.
2. Get context: the original task descriptions for both the current branch and whatever caused the conflict on the target.
3. Construct a prompt for the agent:
   - Show the conflict markers.
   - Show the task descriptions for both sides.
   - Ask the agent to resolve the conflicts by producing the correct merged file content.
4. Apply the resolution: write the resolved content, `git -C {merge-worktree} add` the files, `git -C {merge-worktree} commit`.
5. Verify: `git -C {merge-worktree} diff --name-only --diff-filter=U` should be empty after resolution.

### 8d. Tests

- Sequential merger: test clean merge in a temp repo with two branches. Verify `base_ref` is updated and the user's working directory is untouched.
- Sequential merger: test conflict detection with conflicting changes. Verify merge is aborted cleanly in the merge worktree.
- Conflict resolver: mock the agent, verify prompt includes conflict markers and task context.
- Test merge lock prevents concurrent merges (goroutine test).
- Test merge worktree reuse across multiple sequential merges.

### Commit
```
feat: add merge engine with sequential strategy and conflict resolution
```

---

## Milestone 9: Orchestration Engine

**Goal:** The main execution loop that ties everything together.

### 9a. Engine — `internal/engine/engine.go`

```go
type Engine struct {
    config      config.Config
    store       store.TaskStore
    dag         *dag.DAG
    worktrees   *worktree.Manager
    agentFn     func() agent.Agent   // factory, one agent per worker
    validator   *validate.Validator
    reviewerFn  func(task.Task) review.Reviewer // returns agent or HITL based on task config
    merger      merge.Merger
    resolver    *conflict.Resolver

    maxParallel int
    mu          sync.Mutex
}
```

Implement `Run(ctx context.Context) error`:

1. **Load tasks** from the store.
1b. **Generate run ID**: format `run-{YYYYMMDD}-{HHmmss}-{4hex}` (e.g., `run-20260409-143022-a3f1`). The 4-character random hex suffix prevents collisions if two runs start in the same second from different terminals.
2. **Build DAG**, validate (check for cycles, missing prerequisites).
3. **Initialize lifecycles**: for each task, create a lifecycle record in `pending` status (if one doesn't already exist — for resume support).
4. **Evaluate readiness**: transition all tasks whose prerequisites are met from `pending` → `ready`.
5. **Start worker goroutines** (up to `maxParallel`). Each worker loops:
   a. `ClaimNextReady` from the store.
   b. If `ErrNoReadyTasks` and all tasks are terminal → exit.
   c. If `ErrNoReadyTasks` but non-terminal tasks exist → wait and retry (another worker's merge may unlock new tasks).
   d. Create worktree.
   e. Update lifecycle → `running`.
   f. Execute agent.
   g. Update lifecycle → `validating`. Run validation.
   h. If validation fails: update lifecycle → `failed_validation`. If can retry, transition → `ready` (back into the pool). Otherwise → `failed`.
   i. Update lifecycle → `reviewing`. Run review.
   j. If rejected: similar retry logic.
   k. Update lifecycle → `merging`. Run merge.
   l. If conflict: update → `conflict` → `resolving`. Increment `ResolveAttempts`. If `ResolveAttempts > MaxResolveAttempts`, transition → `failed`. Otherwise run resolver, then retry merge.
   m. If merge succeeds: update → `merged`. Clean up worktree. **Re-evaluate readiness for all pending tasks** (this is the DAG-reactive part — notify other workers that new tasks may be ready).
6. **Wait for all workers to exit.**
7. **Report summary**: total time, tasks merged/failed, retries, conflicts resolved.

The **readiness re-evaluation after each merge** is critical. After a task merges, the engine must check every `pending` task to see if its prerequisites are now all `merged`. Any newly-ready tasks get transitioned to `ready`, making them available for `ClaimNextReady`.

Use a `sync.Cond` or channel to wake sleeping workers when new tasks become ready.

### 9b. Engine resume support

When `Run` is called, before starting workers:
1. Load all existing lifecycles.
2. For tasks with lifecycle in `running`, `validating`, `reviewing`, `merging`, or `resolving` (stale from a crash): clean up the worktree if it exists, then transition back to `ready` via the state machine's crash recovery transitions. Clear `worker_id` and `error`. Reset `current_step` and `steps_completed` — crash recovery always starts fresh because the worktree state is unreliable after a crash.
3. For tasks already `merged` or `failed`: skip.
4. Proceed with normal execution.

The crash recovery transitions (`{running,validating,reviewing,merging,resolving}` -> `ready`) are explicitly defined in the state machine's `validTransitions` map, so this uses the normal `Transition()` path — no bypass needed.

**Retry vs crash recovery — worktree handling:**
- **Crash recovery** (stale `running`/`validating`/etc. from a previous engine run): worktree is destroyed and recreated. The agent starts from step 0. Partial work is unreliable after a crash.
- **Retry** (`failed_validation` or `rejected` → `ready`): the existing worktree is preserved. The agent receives `ResumeFrom` set to `lifecycle.CurrentStep`, so it can build on its prior work rather than starting over. The retry prompt includes the validation/review feedback in `lifecycle.Error` so the agent knows what to fix.

### 9c. Tests — `internal/engine/engine_test.go`

- Test with 3 independent tasks: all run in parallel, all merge.
- Test with a dependency chain: A → B → C. Verify execution order.
- Test with a diamond: A, B independent; C depends on both. Verify C starts only after both merge.
- Test agent failure + retry: mock agent fails first time, succeeds second.
- Test validation failure + retry.
- Test resume: create partially-completed state, run engine, verify it picks up.

### Commit
```
feat: add orchestration engine with DAG-reactive scheduling and crash recovery
```

---

## Milestone 10: CLI

**Goal:** Cobra-based CLI with commands for the full workflow.

### 10a. Commands

Use [cobra](https://github.com/spf13/cobra). Commands:

**`flywheel init`**
- Loads tasks, builds DAG, validates.
- Prints execution plan (waves, estimated parallelism).
- Creates `.flywheel/` directory if it doesn't exist.
- Does NOT start execution.

**`flywheel run`**
- Runs the engine.
- Flags: `--max-parallel N`, `--verbose`, `--dry-run` (print what would happen without executing).
- Streams progress to stdout with the panel-style output (task status boxes).
- Handles SIGINT gracefully: cancel running agents, clean up worktrees, save state.

**`flywheel status`**
- Prints a table of all tasks with their current lifecycle status.
- `flywheel status {task_id}` prints detailed lifecycle for one task.
- `flywheel status {task_id} --lifecycle` prints the full timeline with timestamps.

**`flywheel review`**
- `flywheel review list` — lists tasks awaiting human review.
- `flywheel review approve {task_id}` — approves a task.
- `flywheel review reject {task_id} --reason "..."` — rejects with reason.

**`flywheel clean`**
- Removes all worktrees, deletes flywheel branches, cleans `.flywheel/` state.
- `flywheel clean --worktrees-only` — just worktrees, keep state.

**`flywheel validate`**
- `flywheel validate tasks` — loads and validates all task definitions without running.
- `flywheel validate dag` — builds DAG, checks for cycles, shows warnings.

### 10b. Configuration loading

The CLI looks for `flywheel.json` in the current directory. Allow override with `--config path/to/config`.

### 10c. Signal handling

Register handlers for SIGINT and SIGTERM:
1. Cancel the engine's context.
2. Wait for workers to finish current operations (with a timeout).
3. Save state.
4. Clean up worktrees for incomplete tasks (or leave them for resume — configurable).
5. Print summary of what was completed and what's still pending.

### Commit
```
feat: add Cobra CLI with init, run, status, review, clean, and validate commands
```

---

## Post-MVP Milestones (Future)

These are not part of the initial build but are captured for planning:

### Milestone 11: Web Dashboard
- Embedded HTTP server (`flywheel dashboard --port 4040`).
- Server-sent events for live status updates.
- DAG visualization with color-coded nodes.
- HITL review approval from the browser.
- Task timeline view with full audit trail.

### Milestone 12: SQLite Store Backend
- Implement `TaskStore` with SQLite.
- `ClaimNextReady` uses `BEGIN IMMEDIATE` transactions.
- Enables better querying and reporting.

### Milestone 13: PR Merge Strategy
- GitHub/GitLab API integration.
- Create PRs from completed branches.
- Poll for approval status.
- Merge via API.

### Milestone 14: `flywheel plan` Command
- Takes a high-level goal + codebase context.
- Uses an agent to decompose into a task graph.
- Outputs task files for review before execution.

### Milestone 15: Postgres Store Backend
- Implement `TaskStore` with Postgres.
- `ClaimNextReady` uses `SELECT ... FOR UPDATE SKIP LOCKED`.
- Enables multi-machine worker coordination.

### Milestone 16: Accumulate Merge Strategy
- Hold completed branches until a group/wave finishes.
- Merge as a batch.
- Review the full changeset holistically.

### Milestone 17: External Integrations / Webhooks
- Post-merge hooks (GitHub Projects, Linear, Slack).
- Configurable webhook URLs per lifecycle transition.
- Optional task-level external reference mapping (e.g., GitHub item IDs, Linear issue IDs) for bidirectional status sync.