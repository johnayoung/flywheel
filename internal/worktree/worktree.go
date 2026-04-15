package worktree

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
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
// If a worktree directory or branch with the same name already exists (e.g. left
// over from a crashed prior run), it is cleaned up first: the directory is
// removed and the branch is archived (if it has commits ahead of baseRef) or
// deleted (if empty). This makes Create resilient to leftover state without
// silently discarding committed work.
func (m *Manager) Create(ctx context.Context, taskID string, baseRef string) (*Worktree, error) {
	branch := m.branchPrefix + taskID
	path := filepath.Join(m.worktreeBase, taskID)

	if err := m.cleanPreexisting(ctx, taskID, branch, path, baseRef); err != nil {
		return nil, fmt.Errorf("cleaning pre-existing state for %s: %w", taskID, err)
	}

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

// Remove deletes the worktree directory for the given task. If the branch has
// commits ahead of baseRef, it is renamed to flywheel/archive/<task>-<unix>
// so committed work is preserved across retries; otherwise the branch is
// deleted outright. An empty baseRef disables archival and falls back to
// unconditional deletion (legacy behavior).
func (m *Manager) Remove(ctx context.Context, taskID, baseRef string) error {
	path := filepath.Join(m.worktreeBase, taskID)
	branch := m.branchPrefix + taskID

	cmd := exec.CommandContext(ctx, "git", "worktree", "remove", path, "--force")
	cmd.Dir = m.repoRoot
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("git worktree remove: %w: %s", err, string(out))
	}

	return m.archiveOrDelete(ctx, taskID, branch, baseRef)
}

// archiveOrDelete removes branch, renaming it to flywheel/archive/<task>-<unix>
// if it has commits ahead of baseRef (preserving work) or deleting outright
// otherwise. A missing branch is a no-op. An empty baseRef forces unconditional
// deletion.
func (m *Manager) archiveOrDelete(ctx context.Context, taskID, branch, baseRef string) error {
	if !m.branchExists(ctx, branch) {
		return nil
	}
	if baseRef != "" {
		ahead, err := m.commitsAhead(ctx, baseRef, branch)
		if err == nil && ahead > 0 {
			archiveName := fmt.Sprintf("flywheel/archive/%s-%d", taskID, time.Now().Unix())
			cmd := exec.CommandContext(ctx, "git", "branch", "-m", branch, archiveName)
			cmd.Dir = m.repoRoot
			if out, err := cmd.CombinedOutput(); err != nil {
				return fmt.Errorf("git branch -m %s %s: %w: %s", branch, archiveName, err, string(out))
			}
			return nil
		}
	}
	cmd := exec.CommandContext(ctx, "git", "branch", "-D", branch)
	cmd.Dir = m.repoRoot
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("git branch -D %s: %w: %s", branch, err, string(out))
	}
	return nil
}

// branchExists reports whether a local branch ref exists.
func (m *Manager) branchExists(ctx context.Context, branch string) bool {
	cmd := exec.CommandContext(ctx, "git", "show-ref", "--verify", "--quiet", "refs/heads/"+branch)
	cmd.Dir = m.repoRoot
	return cmd.Run() == nil
}

// cleanPreexisting clears any stale worktree dir or branch that would collide
// with a fresh Create for this task. Committed work on a stale branch is
// archived rather than discarded.
func (m *Manager) cleanPreexisting(ctx context.Context, taskID, branch, path, baseRef string) error {
	// If the worktree directory exists, try to detach it via git first so git's
	// internal admin state stays consistent. Fall back to rm -rf if git refuses
	// (e.g. the admin metadata is already gone) and prune afterwards.
	if _, err := os.Stat(path); err == nil {
		cmd := exec.CommandContext(ctx, "git", "worktree", "remove", "--force", path)
		cmd.Dir = m.repoRoot
		if out, rmErr := cmd.CombinedOutput(); rmErr != nil {
			_ = out
			if rmErr := os.RemoveAll(path); rmErr != nil {
				return fmt.Errorf("removing stale worktree dir %s: %w", path, rmErr)
			}
		}
	}
	// Prune dangling admin metadata (safe no-op if nothing to prune).
	cmd := exec.CommandContext(ctx, "git", "worktree", "prune")
	cmd.Dir = m.repoRoot
	_ = cmd.Run()

	return m.archiveOrDelete(ctx, taskID, branch, baseRef)
}

// PruneStale sweeps orphaned flywheel worktrees and branches left by a crashed
// prior run. Any worktree whose taskID is not in active is removed and its
// branch archived (if it has commits) or deleted. Safe to call at startup
// before any workers begin; unsafe to call concurrently with Create/Remove.
func (m *Manager) PruneStale(ctx context.Context, active map[string]bool, baseRef string) error {
	// Prune first so git's state matches the filesystem.
	cmd := exec.CommandContext(ctx, "git", "worktree", "prune")
	cmd.Dir = m.repoRoot
	_ = cmd.Run()

	existing, err := m.List(ctx)
	if err != nil {
		return fmt.Errorf("listing worktrees: %w", err)
	}
	for _, wt := range existing {
		if active[wt.TaskID] {
			continue
		}
		if err := m.Remove(ctx, wt.TaskID, baseRef); err != nil {
			return fmt.Errorf("pruning stale worktree %s: %w", wt.TaskID, err)
		}
	}

	// Also sweep branches whose worktrees are already gone (branch-only orphans).
	orphans, err := m.listOrphanBranches(ctx)
	if err != nil {
		return fmt.Errorf("listing orphan branches: %w", err)
	}
	for _, taskID := range orphans {
		if active[taskID] {
			continue
		}
		branch := m.branchPrefix + taskID
		if err := m.archiveOrDelete(ctx, taskID, branch, baseRef); err != nil {
			return fmt.Errorf("archiving orphan branch %s: %w", branch, err)
		}
	}
	return nil
}

// listOrphanBranches returns taskIDs of branches matching the flywheel prefix
// that have no worktree checked out.
func (m *Manager) listOrphanBranches(ctx context.Context) ([]string, error) {
	cmd := exec.CommandContext(ctx, "git", "for-each-ref", "--format=%(refname:short)", "refs/heads/"+m.branchPrefix)
	cmd.Dir = m.repoRoot
	out, err := cmd.CombinedOutput()
	if err != nil {
		return nil, fmt.Errorf("git for-each-ref: %w: %s", err, string(out))
	}
	wts, err := m.List(ctx)
	if err != nil {
		return nil, err
	}
	checkedOut := map[string]bool{}
	for _, wt := range wts {
		checkedOut[wt.Branch] = true
	}
	var orphans []string
	for _, line := range strings.Split(strings.TrimSpace(string(out)), "\n") {
		branch := strings.TrimSpace(line)
		if branch == "" || checkedOut[branch] {
			continue
		}
		// Skip archive branches; only prune live flywheel/<task> branches.
		if strings.HasPrefix(branch, m.branchPrefix+"archive/") {
			continue
		}
		taskID := strings.TrimPrefix(branch, m.branchPrefix)
		if taskID == "" {
			continue
		}
		orphans = append(orphans, taskID)
	}
	return orphans, nil
}

// commitsAhead reports how many commits `branch` has beyond `baseRef`.
func (m *Manager) commitsAhead(ctx context.Context, baseRef, branch string) (int, error) {
	cmd := exec.CommandContext(ctx, "git", "rev-list", "--count", baseRef+".."+branch)
	cmd.Dir = m.repoRoot
	out, err := cmd.CombinedOutput()
	if err != nil {
		return 0, fmt.Errorf("git rev-list --count: %w: %s", err, string(out))
	}
	var n int
	if _, err := fmt.Sscanf(strings.TrimSpace(string(out)), "%d", &n); err != nil {
		return 0, fmt.Errorf("parsing rev-list count %q: %w", string(out), err)
	}
	return n, nil
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
