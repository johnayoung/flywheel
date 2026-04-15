package validate

import (
	"bytes"
	"context"
	"fmt"
	"os/exec"
	"regexp"
	"strings"

	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/task"
)

// Validator runs post-agent mechanical checks against a worktree.
//
// Validation is intentionally a cheap, deterministic gate: clean worktree, a
// commit exists, its conventional-commit type matches the task, and the build
// passes. Correctness and acceptance-criteria judgements belong to the review
// phase (internal/review), which runs a separate agent over the diff.
type Validator struct {
	buildCommand string
}

// New creates a Validator. buildCommand may be empty (skips build check).
func New(buildCommand string) *Validator {
	return &Validator{buildCommand: buildCommand}
}

// ValidationResult is the aggregate outcome of all validation checks.
type ValidationResult struct {
	Passed bool
	Checks []CheckResult
	Error  string
}

// CheckResult is the outcome of a single validation check.
type CheckResult struct {
	Name   string
	Passed bool
	Detail string
}

// Validate runs all post-agent mechanical checks in order against the given
// task and lifecycle. All checks run regardless of earlier failures. Passed
// is true only if every check passes.
func (v *Validator) Validate(ctx context.Context, t task.Task, lc lifecycle.Lifecycle) (*ValidationResult, error) {
	var checks []CheckResult

	checks = append(checks, v.checkCleanWorktree(ctx, lc))
	checks = append(checks, v.checkCommitExists(ctx, lc))
	checks = append(checks, v.checkCommitMessage(ctx, t, lc))
	checks = append(checks, v.checkBuild(ctx, lc)...)

	passed := true
	for _, c := range checks {
		if !c.Passed {
			passed = false
			break
		}
	}

	return &ValidationResult{Passed: passed, Checks: checks}, nil
}

func (v *Validator) checkCleanWorktree(ctx context.Context, lc lifecycle.Lifecycle) CheckResult {
	cmd := exec.CommandContext(ctx, "git", "-C", lc.WorktreePath, "status", "--porcelain")
	out, err := cmd.Output()
	if err != nil {
		return CheckResult{Name: "clean_worktree", Passed: false, Detail: fmt.Sprintf("git status failed: %v", err)}
	}
	if len(bytes.TrimSpace(out)) > 0 {
		return CheckResult{Name: "clean_worktree", Passed: false, Detail: fmt.Sprintf("worktree has uncommitted changes:\n%s", string(out))}
	}
	return CheckResult{Name: "clean_worktree", Passed: true, Detail: "worktree is clean"}
}

func (v *Validator) checkCommitExists(ctx context.Context, lc lifecycle.Lifecycle) CheckResult {
	cmd := exec.CommandContext(ctx, "git", "-C", lc.WorktreePath, "log", "--oneline", lc.BaseSHA+"..HEAD")
	out, err := cmd.Output()
	if err != nil {
		return CheckResult{Name: "commit_exists", Passed: false, Detail: fmt.Sprintf("git log failed: %v", err)}
	}
	lines := strings.TrimSpace(string(out))
	if lines == "" {
		return CheckResult{Name: "commit_exists", Passed: false, Detail: "no commits found after base SHA"}
	}
	count := len(strings.Split(lines, "\n"))
	return CheckResult{Name: "commit_exists", Passed: true, Detail: fmt.Sprintf("%d commit(s) found", count)}
}

// commitSubjectType matches the leading type of a conventional-commit subject,
// which REQUIRES a trailing colon: "feat:", "fix(scope):", "refactor!:" etc.
// Used on actual commit subjects produced by the agent.
var commitSubjectType = regexp.MustCompile(`^([a-zA-Z]+)(?:\([^)]*\))?!?:`)

// taskCommitType matches the leading type declared in task.Commit. Task
// specs sometimes give a bare prefix like "feat(scope)" without a subject, so
// the trailing colon is optional and we also accept end-of-string.
var taskCommitType = regexp.MustCompile(`^([a-zA-Z]+)(?:\([^)]*\))?!?(?::|$)`)

// extractCommitSubjectType returns the conventional-commit type (lowercased)
// from an actual commit subject, or "" if the subject is not a conventional
// commit (missing "<type>:" prefix).
func extractCommitSubjectType(subject string) string {
	m := commitSubjectType.FindStringSubmatch(subject)
	if len(m) < 2 {
		return ""
	}
	return strings.ToLower(m[1])
}

// extractTaskCommitType returns the conventional-commit type (lowercased)
// declared in a task.Commit field, or "" if none is present.
func extractTaskCommitType(s string) string {
	m := taskCommitType.FindStringSubmatch(s)
	if len(m) < 2 {
		return ""
	}
	return strings.ToLower(m[1])
}

func (v *Validator) checkCommitMessage(ctx context.Context, t task.Task, lc lifecycle.Lifecycle) CheckResult {
	cmd := exec.CommandContext(ctx, "git", "-C", lc.WorktreePath, "log", "--reverse", "--format=%s", lc.BaseSHA+"..HEAD")
	out, err := cmd.Output()
	if err != nil {
		return CheckResult{Name: "commit_message", Passed: false, Detail: fmt.Sprintf("git log failed: %v", err)}
	}
	lines := strings.TrimSpace(string(out))
	if lines == "" {
		return CheckResult{Name: "commit_message", Passed: false, Detail: "no commits to check"}
	}
	firstLine := strings.SplitN(lines, "\n", 2)[0]

	// Match conventional-commit type only; the agent can paraphrase the
	// subject freely as long as the category (feat/fix/refactor/...) agrees
	// with the task definition.
	wantType := extractTaskCommitType(t.Commit)
	gotType := extractCommitSubjectType(firstLine)

	if wantType == "" {
		// Task didn't specify a conventional-commit type; accept whatever the
		// agent produced as long as *something* was committed.
		return CheckResult{Name: "commit_message", Passed: true, Detail: fmt.Sprintf("task.commit %q has no conventional-commit type; skipping type check", t.Commit)}
	}
	if gotType == "" {
		return CheckResult{Name: "commit_message", Passed: false, Detail: fmt.Sprintf("first commit %q is not a conventional commit (expected type %q)", firstLine, wantType)}
	}
	if gotType != wantType {
		return CheckResult{Name: "commit_message", Passed: false, Detail: fmt.Sprintf("first commit type %q does not match task type %q (subject: %q)", gotType, wantType, firstLine)}
	}
	return CheckResult{Name: "commit_message", Passed: true, Detail: fmt.Sprintf("first commit type %q matches task", gotType)}
}

func (v *Validator) checkBuild(ctx context.Context, lc lifecycle.Lifecycle) []CheckResult {
	if v.buildCommand == "" {
		return nil
	}
	cmd := exec.CommandContext(ctx, "sh", "-c", v.buildCommand)
	cmd.Dir = lc.WorktreePath
	out, err := cmd.CombinedOutput()
	if err != nil {
		return []CheckResult{{Name: "build", Passed: false, Detail: fmt.Sprintf("build failed: %v\n%s", err, string(out))}}
	}
	return []CheckResult{{Name: "build", Passed: true, Detail: "build succeeded"}}
}
