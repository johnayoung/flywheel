package validate

import (
	"bytes"
	"context"
	"fmt"
	"os/exec"
	"strings"

	"github.com/johnayoung/flywheel/internal/agent"
	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/task"
)

// Validator runs post-agent validation checks against a worktree.
type Validator struct {
	agent        agent.Agent
	buildCommand string
}

// New creates a Validator. agent may be nil (skips acceptance criteria check).
// buildCommand may be empty (skips build check).
func New(ag agent.Agent, buildCommand string) *Validator {
	return &Validator{agent: ag, buildCommand: buildCommand}
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

// Validate runs all post-agent checks in order against the given task and lifecycle.
// All checks run regardless of earlier failures. Passed is true only if every check passes.
func (v *Validator) Validate(ctx context.Context, t task.Task, lc lifecycle.Lifecycle) (*ValidationResult, error) {
	var checks []CheckResult

	checks = append(checks, v.checkCleanWorktree(ctx, lc))
	checks = append(checks, v.checkCommitExists(ctx, lc))
	checks = append(checks, v.checkCommitMessage(ctx, t, lc))
	checks = append(checks, v.checkBuild(ctx, lc)...)
	checks = append(checks, v.checkAcceptanceCriteria(ctx, t, lc)...)

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
	if !strings.HasPrefix(firstLine, t.Commit) {
		return CheckResult{Name: "commit_message", Passed: false, Detail: fmt.Sprintf("first commit %q does not start with %q", firstLine, t.Commit)}
	}
	return CheckResult{Name: "commit_message", Passed: true, Detail: fmt.Sprintf("first commit starts with %q", t.Commit)}
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

func (v *Validator) checkAcceptanceCriteria(ctx context.Context, t task.Task, lc lifecycle.Lifecycle) []CheckResult {
	if v.agent == nil || len(t.AcceptanceCriteria) == 0 {
		return nil
	}

	cmd := exec.CommandContext(ctx, "git", "-C", lc.WorktreePath, "diff", lc.BaseSHA+"..HEAD")
	diffOut, err := cmd.Output()
	if err != nil {
		return []CheckResult{{Name: "acceptance_criteria", Passed: false, Detail: fmt.Sprintf("git diff failed: %v", err)}}
	}

	var prompt strings.Builder
	prompt.WriteString("Review the following diff and determine whether ALL acceptance criteria are met.\n\n")
	prompt.WriteString("## Diff\n```\n")
	prompt.WriteString(string(diffOut))
	prompt.WriteString("\n```\n\n## Acceptance Criteria\n")
	for i, ac := range t.AcceptanceCriteria {
		fmt.Fprintf(&prompt, "%d. %s\n", i+1, ac)
	}
	prompt.WriteString("\nRespond with PASS if all criteria are met, or FAIL followed by an explanation if any are not.")

	result, err := v.agent.Execute(ctx, agent.ExecutionRequest{
		WorktreePath: lc.WorktreePath,
		TaskID:       t.ID,
		Description:  prompt.String(),
	})
	if err != nil {
		return []CheckResult{{Name: "acceptance_criteria", Passed: false, Detail: fmt.Sprintf("agent execution failed: %v", err)}}
	}

	passed := result.Success && strings.Contains(strings.ToUpper(result.Output), "PASS")
	detail := result.Output
	if detail == "" {
		detail = result.ImplementationNotes
	}
	return []CheckResult{{Name: "acceptance_criteria", Passed: passed, Detail: detail}}
}
