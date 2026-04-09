package conflict

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"strings"

	"github.com/johnayoung/flywheel/internal/agent"
)

// Resolver uses an AI agent to resolve git merge conflicts.
type Resolver struct {
	agent agent.Agent
}

// New creates a Resolver backed by the given agent.
func New(ag agent.Agent) *Resolver {
	return &Resolver{agent: ag}
}

// ResolveRequest describes a merge conflict to resolve.
type ResolveRequest struct {
	MergeWorkspace string   // path to the merge worktree (where conflicts exist)
	Branch         string   // the branch being merged
	TargetRef      string   // e.g., "main"
	TaskDesc       string   // description of the task being merged
	TargetDesc     string   // description of what's on the target (context)
	ConflictFiles  []string // list of files with conflicts
}

// ResolveResult captures the outcome of conflict resolution.
type ResolveResult struct {
	Success       bool
	ResolvedFiles []string
	Error         string
}

// Resolve attempts to resolve merge conflicts by delegating to the agent.
func (r *Resolver) Resolve(ctx context.Context, req ResolveRequest) (*ResolveResult, error) {
	// 1. Read conflict file contents.
	conflictContents := make(map[string]string, len(req.ConflictFiles))
	for _, f := range req.ConflictFiles {
		path := req.MergeWorkspace + "/" + f
		data, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("reading conflict file %s: %w", f, err)
		}
		conflictContents[f] = string(data)
	}

	// 2. Build prompt.
	prompt := buildConflictPrompt(req, conflictContents)

	// 3. Call agent.
	result, err := r.agent.Execute(ctx, agent.ExecutionRequest{
		WorktreePath: req.MergeWorkspace,
		TaskID:       "conflict-resolution",
		Description:  prompt,
	})
	if err != nil {
		return nil, fmt.Errorf("agent execution failed: %w", err)
	}
	if !result.Success {
		return &ResolveResult{Success: false, Error: result.Error}, nil
	}

	// 4. Parse resolved files from agent output and write them.
	resolved := parseResolvedFiles(result.Output)
	var resolvedFiles []string
	for _, f := range req.ConflictFiles {
		content, ok := resolved[f]
		if !ok {
			return &ResolveResult{
				Success: false,
				Error:   fmt.Sprintf("agent did not produce resolved content for %s", f),
			}, nil
		}
		path := req.MergeWorkspace + "/" + f
		if err := os.WriteFile(path, []byte(content), 0644); err != nil {
			return nil, fmt.Errorf("writing resolved file %s: %w", f, err)
		}
		resolvedFiles = append(resolvedFiles, f)
	}

	// 5. Stage and commit.
	for _, f := range resolvedFiles {
		cmd := exec.CommandContext(ctx, "git", "-C", req.MergeWorkspace, "add", f)
		if out, err := cmd.CombinedOutput(); err != nil {
			return nil, fmt.Errorf("git add %s: %s: %w", f, string(out), err)
		}
	}
	commitCmd := exec.CommandContext(ctx, "git", "-C", req.MergeWorkspace, "commit", "--no-edit")
	if out, err := commitCmd.CombinedOutput(); err != nil {
		return nil, fmt.Errorf("git commit: %s: %w", string(out), err)
	}

	// 6. Verify no remaining conflicts.
	checkCmd := exec.CommandContext(ctx, "git", "-C", req.MergeWorkspace, "diff", "--name-only", "--diff-filter=U")
	checkOut, err := checkCmd.Output()
	if err != nil {
		return nil, fmt.Errorf("checking remaining conflicts: %w", err)
	}
	if remaining := strings.TrimSpace(string(checkOut)); remaining != "" {
		return &ResolveResult{
			Success: false,
			Error:   "conflicts remain after resolution",
		}, nil
	}

	return &ResolveResult{
		Success:       true,
		ResolvedFiles: resolvedFiles,
	}, nil
}

// buildConflictPrompt constructs the prompt sent to the agent for conflict resolution.
func buildConflictPrompt(req ResolveRequest, conflictContents map[string]string) string {
	var b strings.Builder

	b.WriteString("Resolve the following git merge conflicts.\n\n")
	b.WriteString(fmt.Sprintf("Branch being merged: %s\n", req.Branch))
	b.WriteString(fmt.Sprintf("Target ref: %s\n", req.TargetRef))
	b.WriteString(fmt.Sprintf("Task description (incoming branch): %s\n", req.TaskDesc))
	b.WriteString(fmt.Sprintf("Target description: %s\n\n", req.TargetDesc))

	b.WriteString("Each file below contains conflict markers (<<<<<<< / ======= / >>>>>>>).\n")
	b.WriteString("Resolve every conflict and output the complete resolved file contents.\n\n")

	b.WriteString("Format your output exactly as follows for each file:\n")
	b.WriteString("--- FILE: {path} ---\n")
	b.WriteString("{resolved file contents}\n")
	b.WriteString("--- END FILE ---\n\n")

	for _, f := range req.ConflictFiles {
		content, ok := conflictContents[f]
		if !ok {
			continue
		}
		b.WriteString(fmt.Sprintf("=== CONFLICT FILE: %s ===\n", f))
		b.WriteString(content)
		if !strings.HasSuffix(content, "\n") {
			b.WriteByte('\n')
		}
		b.WriteString(fmt.Sprintf("=== END CONFLICT FILE: %s ===\n\n", f))
	}

	return b.String()
}

// parseResolvedFiles extracts file contents from the agent's output delimited by
// --- FILE: {path} --- and --- END FILE ---.
func parseResolvedFiles(output string) map[string]string {
	result := make(map[string]string)
	const filePrefix = "--- FILE: "
	const fileSuffix = " ---"
	const endMarker = "--- END FILE ---"

	remaining := output
	for {
		startIdx := strings.Index(remaining, filePrefix)
		if startIdx == -1 {
			break
		}
		headerStart := startIdx + len(filePrefix)
		headerEnd := strings.Index(remaining[headerStart:], fileSuffix)
		if headerEnd == -1 {
			break
		}
		filePath := remaining[headerStart : headerStart+headerEnd]

		contentStart := headerStart + headerEnd + len(fileSuffix)
		// Skip the newline after the header line.
		if contentStart < len(remaining) && remaining[contentStart] == '\n' {
			contentStart++
		}

		endIdx := strings.Index(remaining[contentStart:], endMarker)
		if endIdx == -1 {
			break
		}
		content := remaining[contentStart : contentStart+endIdx]
		// Trim a single trailing newline that's part of formatting, not content.
		content = strings.TrimSuffix(content, "\n")

		result[filePath] = content
		remaining = remaining[contentStart+endIdx+len(endMarker):]
	}

	return result
}
