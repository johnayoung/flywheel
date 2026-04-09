package dag

import (
	"fmt"
	"sort"
	"strings"

	"github.com/johnayoung/flywheel/internal/task"
)

// DAG represents a directed acyclic graph of task dependencies.
type DAG struct {
	nodes map[string]*Node
	edges map[string][]string // task ID -> dependent task IDs (forward)
	deps  map[string][]string // task ID -> prerequisite task IDs (reverse)
}

// Node represents a task within the DAG.
type Node struct {
	TaskID   string
	Priority int
}

// Build constructs a DAG from the given tasks, validating that all prerequisites
// exist and that no cycles are present.
func Build(tasks []task.Task) (*DAG, error) {
	d := &DAG{
		nodes: make(map[string]*Node, len(tasks)),
		edges: make(map[string][]string),
		deps:  make(map[string][]string),
	}

	// Register all nodes first.
	for i := range tasks {
		t := &tasks[i]
		if _, exists := d.nodes[t.ID]; exists {
			return nil, fmt.Errorf("duplicate task ID %q", t.ID)
		}
		d.nodes[t.ID] = &Node{
			TaskID:   t.ID,
			Priority: t.Priority,
		}
	}

	// Build edges, validating prerequisites exist.
	for i := range tasks {
		t := &tasks[i]
		for _, prereq := range t.Prerequisites {
			if _, exists := d.nodes[prereq]; !exists {
				return nil, fmt.Errorf("task %q has prerequisite %q which does not exist", t.ID, prereq)
			}
			d.edges[prereq] = append(d.edges[prereq], t.ID)
			d.deps[t.ID] = append(d.deps[t.ID], prereq)
		}
	}

	// Detect cycles using DFS with path tracking.
	if err := d.detectCycle(); err != nil {
		return nil, err
	}

	return d, nil
}

// detectCycle uses iterative DFS to find cycles and returns a descriptive error
// including the cycle path.
func (d *DAG) detectCycle() error {
	const (
		white = 0 // unvisited
		gray  = 1 // in current path
		black = 2 // fully processed
	)

	color := make(map[string]int, len(d.nodes))
	parent := make(map[string]string)

	for id := range d.nodes {
		if color[id] != white {
			continue
		}

		stack := []string{id}
		for len(stack) > 0 {
			node := stack[len(stack)-1]

			if color[node] == white {
				color[node] = gray
				for _, dep := range d.edges[node] {
					if color[dep] == gray {
						// Found a cycle -- reconstruct the path.
						cycle := []string{dep, node}
						cur := node
						for cur != dep {
							cur = parent[cur]
							cycle = append(cycle, cur)
						}
						// Reverse to get the cycle in forward order.
						for i, j := 0, len(cycle)-1; i < j; i, j = i+1, j-1 {
							cycle[i], cycle[j] = cycle[j], cycle[i]
						}
						return fmt.Errorf("cycle detected: %s", strings.Join(cycle, " -> "))
					}
					if color[dep] == white {
						parent[dep] = node
						stack = append(stack, dep)
					}
				}
			} else {
				stack = stack[:len(stack)-1]
				color[node] = black
			}
		}
	}

	return nil
}

// Prerequisites returns the direct prerequisite task IDs for the given task.
func (d *DAG) Prerequisites(taskID string) []string {
	prereqs := d.deps[taskID]
	if prereqs == nil {
		return nil
	}
	out := make([]string, len(prereqs))
	copy(out, prereqs)
	return out
}

// Dependents returns the task IDs that directly depend on the given task.
func (d *DAG) Dependents(taskID string) []string {
	deps := d.edges[taskID]
	if deps == nil {
		return nil
	}
	out := make([]string, len(deps))
	copy(out, deps)
	return out
}

// Roots returns task IDs that have no prerequisites.
func (d *DAG) Roots() []string {
	var roots []string
	for id := range d.nodes {
		if len(d.deps[id]) == 0 {
			roots = append(roots, id)
		}
	}
	sort.Slice(roots, func(i, j int) bool {
		pi := d.nodes[roots[i]].Priority
		pj := d.nodes[roots[j]].Priority
		if pi != pj {
			return pi < pj
		}
		return roots[i] < roots[j]
	})
	return roots
}

// NodeCount returns the number of nodes in the DAG.
func (d *DAG) NodeCount() int {
	return len(d.nodes)
}

// Node returns the node for the given task ID, or nil if not found.
func (d *DAG) Node(taskID string) *Node {
	return d.nodes[taskID]
}
