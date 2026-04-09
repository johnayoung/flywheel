package store

import (
	"context"

	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/task"
)

// TaskStore defines the persistence interface for tasks and their lifecycles.
type TaskStore interface {
	ListTasks(ctx context.Context, filter TaskFilter) ([]task.Task, error)
	GetTask(ctx context.Context, id string) (*task.Task, error)
	CreateLifecycle(ctx context.Context, lc *lifecycle.Lifecycle) error
	GetLifecycle(ctx context.Context, taskID string) (*lifecycle.Lifecycle, error)
	UpdateLifecycle(ctx context.Context, lc *lifecycle.Lifecycle) error
	ListLifecycles(ctx context.Context, filter LifecycleFilter) ([]lifecycle.Lifecycle, error)
	ClaimNextReady(ctx context.Context, workerID string) (*lifecycle.Lifecycle, error)
}

// TaskFilter constrains which tasks are returned by ListTasks.
type TaskFilter struct {
	IDs        []string
	Categories []string
	HasStatus  []lifecycle.Status
}

// LifecycleFilter constrains which lifecycles are returned by ListLifecycles.
type LifecycleFilter struct {
	Statuses []lifecycle.Status
	WorkerID string
	RunID    string
}
