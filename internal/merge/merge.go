package merge

import "context"

// Merger defines the interface for merging branches.
type Merger interface {
	Merge(ctx context.Context, req MergeRequest) (*MergeResult, error)
}

// MergeRequest describes a merge operation.
type MergeRequest struct {
	WorktreePath string // NOT used for merging -- the merge happens in a dedicated workspace
	Branch       string
	TargetRef    string // e.g., "main"
	TaskID       string
}

// MergeResult describes the outcome of a merge operation.
type MergeResult struct {
	Success        bool
	MergeCommitSHA string
	HasConflicts   bool
	ConflictFiles  []string
}
