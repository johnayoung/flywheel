package store

import "errors"

var (
	ErrTaskNotFound    = errors.New("task not found")
	ErrNoReadyTasks    = errors.New("no ready tasks available")
	ErrLifecycleExists = errors.New("lifecycle already exists for task")
	ErrStaleWrite      = errors.New("lifecycle was modified by another process")
)
