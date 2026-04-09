package dag

import (
	"sort"

	"github.com/johnayoung/flywheel/internal/lifecycle"
)

// ReadinessChecker determines which tasks are ready to execute based on
// the DAG structure and current task statuses.
type ReadinessChecker struct {
	dag *DAG
}

// NewReadinessChecker creates a ReadinessChecker for the given DAG.
func NewReadinessChecker(dag *DAG) *ReadinessChecker {
	return &ReadinessChecker{dag: dag}
}

// IsReady returns true if all prerequisites of the given task have status merged.
func (rc *ReadinessChecker) IsReady(taskID string, statuses map[string]lifecycle.Status) bool {
	for _, prereq := range rc.dag.deps[taskID] {
		if statuses[prereq] != lifecycle.StatusMerged {
			return false
		}
	}
	return true
}

// ReadyTasks returns task IDs that are pending and have all prerequisites merged.
// Results are sorted by priority (lower number = higher priority), with ties
// broken by task ID lexicographic order.
func (rc *ReadinessChecker) ReadyTasks(statuses map[string]lifecycle.Status) []string {
	var ready []string
	for id := range rc.dag.nodes {
		if statuses[id] != lifecycle.StatusPending {
			continue
		}
		if rc.IsReady(id, statuses) {
			ready = append(ready, id)
		}
	}
	rc.sortByPriority(ready)
	return ready
}

// ComputeWaves returns theoretical execution waves. Wave 1 contains tasks whose
// prerequisites are all already merged (or have no prerequisites). Wave 2 contains
// tasks whose prerequisites are all in wave 1 or already merged, and so on.
// Each wave is sorted by priority.
func (rc *ReadinessChecker) ComputeWaves(statuses map[string]lifecycle.Status) [][]string {
	// Track which tasks are "done" -- either already merged or assigned to a wave.
	done := make(map[string]bool, len(rc.dag.nodes))
	for id := range rc.dag.nodes {
		if statuses[id] == lifecycle.StatusMerged {
			done[id] = true
		}
	}

	// Collect tasks that still need scheduling (not yet merged).
	remaining := make(map[string]bool)
	for id := range rc.dag.nodes {
		if !done[id] {
			remaining[id] = true
		}
	}

	var waves [][]string
	for len(remaining) > 0 {
		var wave []string
		for id := range remaining {
			allDone := true
			for _, prereq := range rc.dag.deps[id] {
				if !done[prereq] {
					allDone = false
					break
				}
			}
			if allDone {
				wave = append(wave, id)
			}
		}

		if len(wave) == 0 {
			// All remaining tasks have unsatisfied dependencies that can't be
			// resolved (shouldn't happen in a valid DAG, but guard against it).
			break
		}

		rc.sortByPriority(wave)
		waves = append(waves, wave)

		for _, id := range wave {
			done[id] = true
			delete(remaining, id)
		}
	}

	return waves
}

// sortByPriority sorts task IDs by their node priority (ascending), breaking
// ties with lexicographic order on the task ID.
func (rc *ReadinessChecker) sortByPriority(ids []string) {
	sort.Slice(ids, func(i, j int) bool {
		pi := rc.dag.nodes[ids[i]].Priority
		pj := rc.dag.nodes[ids[j]].Priority
		if pi != pj {
			return pi < pj
		}
		return ids[i] < ids[j]
	})
}
