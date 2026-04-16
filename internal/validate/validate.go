package validate

import (
	"bytes"
	"context"
	"fmt"
	"os"
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
//
// The commit message type is auto-fixed rather than rejected: if the agent
// used the wrong conventional-commit type, the first commit is amended to
// match the task definition. This avoids burning retries on a mechanical
// issue the engine can correct.
func (v *Validator) Validate(ctx context.Context, t task.Task, lc lifecycle.Lifecycle) (*ValidationResult, error) {
	var checks []CheckResult

	checks = append(checks, v.checkCleanWorktree(ctx, lc))
	checks = append(checks, v.checkCommitExists(ctx, lc))

	fixResult, err := v.fixCommitMessage(ctx, t, lc)
	if err != nil {
		return nil, fmt.Errorf("fix commit message: %w", err)
	}
	checks = append(checks, fixResult)

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

// fixCommitMessage checks the first commit's conventional-commit type and, if
// it doesn't match the task definition, amends it to use the correct type.
// This always passes (unless there are no commits or git fails), since any
// mismatch is corrected in-place rather than rejected.
func (v *Validator) fixCommitMessage(ctx context.Context, t task.Task, lc lifecycle.Lifecycle) (CheckResult, error) {
	cmd := exec.CommandContext(ctx, "git", "-C", lc.WorktreePath, "log", "--reverse", "--format=%s", lc.BaseSHA+"..HEAD")
	out, err := cmd.Output()
	if err != nil {
		return CheckResult{Name: "commit_message", Passed: false, Detail: fmt.Sprintf("git log failed: %v", err)}, nil
	}
	lines := strings.TrimSpace(string(out))
	if lines == "" {
		return CheckResult{Name: "commit_message", Passed: false, Detail: "no commits to check"}, nil
	}
	firstLine := strings.SplitN(lines, "\n", 2)[0]

	wantType := extractTaskCommitType(t.Commit)
	if wantType == "" {
		return CheckResult{Name: "commit_message", Passed: true, Detail: fmt.Sprintf("task.commit %q has no conventional-commit type; skipping", t.Commit)}, nil
	}

	gotType := extractCommitSubjectType(firstLine)

	if gotType == wantType {
		return CheckResult{Name: "commit_message", Passed: true, Detail: fmt.Sprintf("commit type %q matches task", gotType)}, nil
	}

	// Need to rewrite. Build the corrected subject.
	newSubject := rewriteSubject(firstLine, t.Commit, gotType)

	// Count commits to decide between amend (single) and rebase (multiple).
	commitLines := strings.Split(lines, "\n")
	if len(commitLines) == 1 {
		// Single commit: simple amend.
		amend := exec.CommandContext(ctx, "git", "-C", lc.WorktreePath, "commit", "--amend", "-m", newSubject)
		if out, err := amend.CombinedOutput(); err != nil {
			return CheckResult{}, fmt.Errorf("git commit --amend: %w: %s", err, string(out))
		}
	} else {
		// Multiple commits: rewrite the first one via rebase.
		if err := rebaseRewriteFirst(ctx, lc.WorktreePath, lc.BaseSHA, newSubject); err != nil {
			return CheckResult{}, fmt.Errorf("rebase rewrite first commit: %w", err)
		}
	}

	detail := fmt.Sprintf("rewrote commit type %q -> %q (subject: %q)", gotType, wantType, newSubject)
	if gotType == "" {
		detail = fmt.Sprintf("rewrote non-conventional commit to %q (subject: %q)", wantType, newSubject)
	}
	return CheckResult{Name: "commit_message", Passed: true, Detail: detail}, nil
}

// rewriteSubject builds a corrected commit subject. If the original had a
// conventional-commit type, it is replaced. If it didn't, the task's commit
// prefix is prepended.
func rewriteSubject(original, taskCommit, gotType string) string {
	if gotType != "" {
		// Replace existing type (and optional scope/bang) with task commit prefix.
		loc := commitSubjectType.FindStringIndex(original)
		if loc != nil {
			return taskCommit + ": " + strings.TrimSpace(original[loc[1]:])
		}
	}
	// Not a conventional commit at all; prepend the task prefix.
	return taskCommit + ": " + original
}

// rebaseRewriteFirst rewrites only the first commit's message after baseSHA
// using a non-interactive rebase with GIT_SEQUENCE_EDITOR to mark the first
// commit for reword, and GIT_EDITOR to supply the new message.
func rebaseRewriteFirst(ctx context.Context, worktreePath, baseSHA, newSubject string) error {
	// Write the new message to a temp file so the editor script can read it
	// without shell-escaping issues.
	msgFile, err := os.CreateTemp("", "flywheel-commit-msg-*")
	if err != nil {
		return fmt.Errorf("create temp file: %w", err)
	}
	defer os.Remove(msgFile.Name())
	if _, err := msgFile.WriteString(newSubject); err != nil {
		msgFile.Close()
		return fmt.Errorf("write temp file: %w", err)
	}
	msgFile.Close()

	seqEditor := `sed -i '1s/^pick /reword /' "$1"`
	// The editor script copies the prepared message into the commit message file.
	// Must be a sh -c wrapper so $1 is the file argument from git, not from the
	// parent shell context.
	editor := fmt.Sprintf(`sh -c 'cp "%s" "$1"' --`, msgFile.Name())

	cmd := exec.CommandContext(ctx, "git", "-C", worktreePath, "rebase", "-i", baseSHA)
	cmd.Env = append(cmd.Environ(),
		"GIT_SEQUENCE_EDITOR="+seqEditor,
		"GIT_EDITOR="+editor,
	)
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("%w: %s", err, string(out))
	}
	return nil
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
