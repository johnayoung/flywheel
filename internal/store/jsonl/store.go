package jsonl

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"

	"github.com/gofrs/flock"
	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/store"
	"github.com/johnayoung/flywheel/internal/task"
)

// Store is a file-based TaskStore implementation that persists lifecycle state
// as individual JSON files and loads tasks from a directory of JSON files.
type Store struct {
	tasks         []task.Task
	taskIndex     map[string]*task.Task
	lifecyclePath string
	mu            sync.RWMutex
}

var _ store.TaskStore = (*Store)(nil)

// New creates a Store by loading tasks from tasksPath (a directory of .json files)
// and using lifecyclePath as the directory for lifecycle JSON files.
func New(tasksPath, lifecyclePath string) (*Store, error) {
	tasks, err := task.ParseDir(tasksPath)
	if err != nil {
		return nil, fmt.Errorf("loading tasks: %w", err)
	}

	if err := os.MkdirAll(lifecyclePath, 0o755); err != nil {
		return nil, fmt.Errorf("creating lifecycle directory: %w", err)
	}

	s := &Store{
		tasks:         tasks,
		taskIndex:     make(map[string]*task.Task, len(tasks)),
		lifecyclePath: lifecyclePath,
	}
	for i := range s.tasks {
		s.taskIndex[s.tasks[i].ID] = &s.tasks[i]
	}

	return s, nil
}

func (s *Store) lifecycleFilePath(taskID string) string {
	return filepath.Join(s.lifecyclePath, taskID+".json")
}

// ListTasks returns tasks matching the filter criteria.
func (s *Store) ListTasks(ctx context.Context, filter store.TaskFilter) ([]task.Task, error) {
	idSet := toSet(filter.IDs)
	catSet := toSet(filter.Categories)

	// If HasStatus is specified, load lifecycle data to filter by status.
	var statusSet map[string]bool
	var lcByTaskID map[string]*lifecycle.Lifecycle
	if len(filter.HasStatus) > 0 {
		statusSet = make(map[string]bool, len(filter.HasStatus))
		for _, st := range filter.HasStatus {
			statusSet[string(st)] = true
		}
		var err error
		lcByTaskID, err = s.readAllLifecycles()
		if err != nil {
			return nil, err
		}
	}

	var result []task.Task
	for _, t := range s.tasks {
		if len(idSet) > 0 && !idSet[t.ID] {
			continue
		}
		if len(catSet) > 0 && !catSet[t.Category] {
			continue
		}
		if statusSet != nil {
			lc, ok := lcByTaskID[t.ID]
			if !ok || !statusSet[string(lc.Status)] {
				continue
			}
		}
		result = append(result, t)
	}

	return result, nil
}

// GetTask returns a single task by ID.
func (s *Store) GetTask(_ context.Context, id string) (*task.Task, error) {
	t, ok := s.taskIndex[id]
	if !ok {
		return nil, store.ErrTaskNotFound
	}
	cp := *t
	return &cp, nil
}

// CreateLifecycle persists a new lifecycle. Returns ErrLifecycleExists if one already exists.
func (s *Store) CreateLifecycle(_ context.Context, lc *lifecycle.Lifecycle) error {
	path := s.lifecycleFilePath(lc.TaskID)

	if _, err := os.Stat(path); err == nil {
		return store.ErrLifecycleExists
	}

	return s.writeLifecycle(path, lc)
}

// GetLifecycle reads a lifecycle from disk by task ID.
func (s *Store) GetLifecycle(_ context.Context, taskID string) (*lifecycle.Lifecycle, error) {
	path := s.lifecycleFilePath(taskID)
	lc, err := s.readLifecycle(path)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, store.ErrTaskNotFound
		}
		return nil, err
	}
	return lc, nil
}

// UpdateLifecycle performs an optimistic-concurrency update with file locking.
func (s *Store) UpdateLifecycle(_ context.Context, lc *lifecycle.Lifecycle) error {
	path := s.lifecycleFilePath(lc.TaskID)
	fl := flock.New(path + ".lock")

	if err := fl.Lock(); err != nil {
		return fmt.Errorf("acquiring lock: %w", err)
	}
	defer fl.Unlock()

	current, err := s.readLifecycle(path)
	if err != nil {
		return fmt.Errorf("reading current lifecycle: %w", err)
	}

	if current.Version != lc.Version {
		return store.ErrStaleWrite
	}

	lc.Version++
	return s.writeLifecycle(path, lc)
}

// ListLifecycles returns all lifecycles matching the filter.
func (s *Store) ListLifecycles(_ context.Context, filter store.LifecycleFilter) ([]lifecycle.Lifecycle, error) {
	all, err := s.readAllLifecycles()
	if err != nil {
		return nil, err
	}

	statusSet := make(map[string]bool, len(filter.Statuses))
	for _, st := range filter.Statuses {
		statusSet[string(st)] = true
	}

	var result []lifecycle.Lifecycle
	for _, lc := range all {
		if len(statusSet) > 0 && !statusSet[string(lc.Status)] {
			continue
		}
		if filter.WorkerID != "" && lc.WorkerID != filter.WorkerID {
			continue
		}
		if filter.RunID != "" && lc.RunID != filter.RunID {
			continue
		}
		result = append(result, *lc)
	}

	return result, nil
}

// ClaimNextReady atomically claims the highest-priority ready task for the given worker.
func (s *Store) ClaimNextReady(_ context.Context, workerID string) (*lifecycle.Lifecycle, error) {
	all, err := s.readAllLifecycles()
	if err != nil {
		return nil, err
	}

	// Collect ready lifecycles.
	var ready []*lifecycle.Lifecycle
	for _, lc := range all {
		if lc.Status == lifecycle.StatusReady {
			ready = append(ready, lc)
		}
	}

	// Sort by task priority (lower number = higher priority).
	sort.Slice(ready, func(i, j int) bool {
		pi := s.taskPriority(ready[i].TaskID)
		pj := s.taskPriority(ready[j].TaskID)
		return pi < pj
	})

	for _, candidate := range ready {
		path := s.lifecycleFilePath(candidate.TaskID)
		fl := flock.New(path + ".lock")

		if err := fl.Lock(); err != nil {
			continue
		}

		lc, err := s.readLifecycle(path)
		if err != nil {
			fl.Unlock()
			continue
		}

		if lc.Status != lifecycle.StatusReady {
			fl.Unlock()
			continue
		}

		if err := lifecycle.Transition(lc, lifecycle.StatusRunning); err != nil {
			fl.Unlock()
			continue
		}

		lc.WorkerID = workerID
		if err := s.writeLifecycle(path, lc); err != nil {
			fl.Unlock()
			continue
		}

		fl.Unlock()
		return lc, nil
	}

	return nil, store.ErrNoReadyTasks
}

func (s *Store) taskPriority(taskID string) int {
	if t, ok := s.taskIndex[taskID]; ok {
		return t.Priority
	}
	return 1<<31 - 1
}

func (s *Store) readLifecycle(path string) (*lifecycle.Lifecycle, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var lc lifecycle.Lifecycle
	if err := json.Unmarshal(data, &lc); err != nil {
		return nil, fmt.Errorf("decoding lifecycle %s: %w", path, err)
	}
	return &lc, nil
}

func (s *Store) writeLifecycle(path string, lc *lifecycle.Lifecycle) error {
	data, err := json.MarshalIndent(lc, "", "  ")
	if err != nil {
		return fmt.Errorf("encoding lifecycle: %w", err)
	}
	if err := os.WriteFile(path, data, 0o644); err != nil {
		return fmt.Errorf("writing lifecycle %s: %w", path, err)
	}
	return nil
}

func (s *Store) readAllLifecycles() (map[string]*lifecycle.Lifecycle, error) {
	entries, err := os.ReadDir(s.lifecyclePath)
	if err != nil {
		return nil, fmt.Errorf("reading lifecycle directory: %w", err)
	}

	result := make(map[string]*lifecycle.Lifecycle)
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}
		path := filepath.Join(s.lifecyclePath, entry.Name())
		lc, err := s.readLifecycle(path)
		if err != nil {
			return nil, err
		}
		result[lc.TaskID] = lc
	}

	return result, nil
}

func toSet(items []string) map[string]bool {
	if len(items) == 0 {
		return nil
	}
	m := make(map[string]bool, len(items))
	for _, item := range items {
		m[item] = true
	}
	return m
}
