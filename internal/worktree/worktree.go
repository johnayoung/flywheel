package worktree

import (
	"bufio"
	"context"
	"fmt"
	"os/exec"
	"path/filepath"
	"strings"
)

// Manager handles git worktree lifecycle operations.
type Manager struct {
	repoRoot     string
	worktreeBase string
	branchPrefix string
}

// Worktree represents a single git worktree tied to a task.
type Worktree struct {
	TaskID  string
	Path    string
	Branch  string
	BaseSHA string
}

// Commit represents a single git commit.
type Commit struct {
	SHA     string
	Message string
}

// NewManager creates a Manager for the given repository.
func NewManager(repoRoot, worktreeBase, branchPrefix string) *Manager {
	return &Manager{
		repoRoot:     repoRoot,
		worktreeBase: worktreeBase,
		branchPrefix: branchPrefix,
	}
}

// Create adds a new git worktree for the given task, branching from baseRef.
func (m *Manager) Create(ctx context.Context, taskID string, baseRef string) (*Worktree, error) {
	branch := m.branchPrefix + taskID
	path := filepath.Join(m.worktreeBase, taskID)

	// git worktree add -b {branch} {path} {baseRef}
	cmd := exec.CommandContext(ctx, "git", "worktree", "add", "-b", branch, path, baseRef)
	cmd.Dir = m.repoRoot
	if out, err := cmd.CombinedOutput(); err != nil {
		return nil, fmt.Errorf("git worktree add: %w: %s", err, string(out))
	}

	// git rev-parse {baseRef}
	cmd = exec.CommandContext(ctx, "git", "rev-parse", baseRef)
	cmd.Dir = m.repoRoot
	out, err := cmd.CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("git rev-parse %s: %w: %s", baseRef, err, string(out))
	}

	return &Worktree{
		TaskID:  taskID,
		Path:    path,
		Branch:  branch,
		BaseSHA: strings.TrimSpace(string(out)),
	}, nil
}

// Remove deletes the worktree directory and its branch for the given task.
func (m *Manager) Remove(ctx context.Context, taskID string) error {
	path := filepath.Join(m.worktreeBase, taskID)
	branch := m.branchPrefix + taskID

	cmd := exec.CommandContext(ctx, "git", "worktree", "remove", path, "--force")
	cmd.Dir = m.repoRoot
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("git worktree remove: %w: %s", err, string(out))
	}

	cmd = exec.CommandContext(ctx, "git", "branch", "-D", branch)
	cmd.Dir = m.repoRoot
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("git branch -D %s: %w: %s", branch, err, string(out))
	}

	return nil
}

// List returns all worktrees whose branch matches the configured prefix.
func (m *Manager) List(ctx context.Context) ([]Worktree, error) {
	cmd := exec.CommandContext(ctx, "git", "worktree", "list", "--porcelain")
	cmd.Dir = m.repoRoot
	out, err := cmd.CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("git worktree list: %w: %s", err, string(out))
	}

	var worktrees []Worktree
	scanner := bufio.NewScanner(strings.NewReader(string(out)))

	var wtPath, head, branch string
	for scanner.Scan() {
		line := scanner.Text()
		switch {
		case strings.HasPrefix(line, "worktree "):
			wtPath = strings.TrimPrefix(line, "worktree ")
		case strings.HasPrefix(line, "HEAD "):
			head = strings.TrimPrefix(line, "HEAD ")
		case strings.HasPrefix(line, "branch "):
			branch = strings.TrimPrefix(line, "branch refs/heads/")
		case line == "":
			if branch != "" && strings.HasPrefix(branch, m.branchPrefix) {
				taskID := strings.TrimPrefix(branch, m.branchPrefix)
				worktrees = append(worktrees, Worktree{
					TaskID:  taskID,
					Path:    wtPath,
					Branch:  branch,
					BaseSHA: head,
				})
			}
			wtPath, head, branch = "", "", ""
		}
	}
	// Handle last block if output doesn't end with blank line.
	if branch != "" && strings.HasPrefix(branch, m.branchPrefix) {
		taskID := strings.TrimPrefix(branch, m.branchPrefix)
		worktrees = append(worktrees, Worktree{
			TaskID:  taskID,
			Path:    wtPath,
			Branch:  branch,
			BaseSHA: head,
		})
	}

	return worktrees, nil
}

// IsClean reports whether the worktree at path has no uncommitted changes.
func (m *Manager) IsClean(ctx context.Context, path string) (bool, error) {
	cmd := exec.CommandContext(ctx, "git", "-C", path, "status", "--porcelain")
	out, err := cmd.CombinedOutput()
	if err != nil {
		return false, fmt.Errorf("git status: %w: %s", err, string(out))
	}
	return strings.TrimSpace(string(out)) == "", nil
}

// GetCommits returns commits in the worktree since baseSHA.
func (m *Manager) GetCommits(ctx context.Context, path string, baseSHA string) ([]Commit, error) {
	cmd := exec.CommandContext(ctx, "git", "-C", path, "log", "--oneline", baseSHA+"..HEAD")
	out, err := cmd.CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("git log: %w: %s", err, string(out))
	}

	var commits []Commit
	for _, line := range strings.Split(strings.TrimSpace(string(out)), "\n") {
		if line == "" {
			continue
		}
		parts := strings.SplitN(line, " ", 2)
		msg := ""
		if len(parts) == 2 {
			msg = parts[1]
		}
		commits = append(commits, Commit{
			SHA:     parts[0],
			Message: msg,
		})
	}
	return commits, nil
}
