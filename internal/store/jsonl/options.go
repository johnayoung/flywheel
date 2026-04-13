package jsonl

import "path/filepath"

// Options configures the JSONL file-based store backend. Root is the directory
// under which both task definitions (Root/tasks) and lifecycle state
// (Root/lifecycle) are kept. An empty Root resolves to ".flywheel".
type Options struct {
	Root string `json:"root"`
}

// DefaultRoot is used when Options.Root is empty.
const DefaultRoot = ".flywheel"

// TasksDir returns the directory holding task JSON definitions.
func (o Options) TasksDir() string {
	return filepath.Join(o.root(), "tasks")
}

// LifecycleDir returns the directory holding lifecycle JSON state.
func (o Options) LifecycleDir() string {
	return filepath.Join(o.root(), "lifecycle")
}

func (o Options) root() string {
	if o.Root == "" {
		return DefaultRoot
	}
	return o.Root
}
