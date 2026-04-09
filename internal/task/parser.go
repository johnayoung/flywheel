package task

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// ParseFile reads a single JSON file, decodes it into a Task, and validates it.
func ParseFile(path string) (*Task, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("reading task file %s: %w", path, err)
	}

	var t Task
	if err := json.Unmarshal(data, &t); err != nil {
		return nil, fmt.Errorf("decoding task file %s: %w", path, err)
	}

	if err := t.Validate(); err != nil {
		return nil, fmt.Errorf("validating task in %s: %w", path, err)
	}

	return &t, nil
}

// ParseDir reads all .json files in the given directory, parses each as a Task, and validates them.
func ParseDir(dir string) ([]Task, error) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil, fmt.Errorf("reading task directory %s: %w", dir, err)
	}

	var tasks []Task
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}

		path := filepath.Join(dir, entry.Name())
		t, err := ParseFile(path)
		if err != nil {
			return nil, err
		}
		tasks = append(tasks, *t)
	}

	return tasks, nil
}

// ParseJSONL reads a JSONL file (one JSON object per line), decodes each line as a Task, and validates them.
func ParseJSONL(path string) ([]Task, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("opening JSONL file %s: %w", path, err)
	}
	defer f.Close()

	var tasks []Task
	scanner := bufio.NewScanner(f)
	lineNum := 0
	for scanner.Scan() {
		lineNum++
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}

		var t Task
		if err := json.Unmarshal([]byte(line), &t); err != nil {
			return nil, fmt.Errorf("decoding task at line %d in %s: %w", lineNum, path, err)
		}

		if err := t.Validate(); err != nil {
			return nil, fmt.Errorf("validating task %q at line %d in %s: %w", t.ID, lineNum, path, err)
		}

		tasks = append(tasks, t)
	}

	if err := scanner.Err(); err != nil {
		return nil, fmt.Errorf("reading JSONL file %s: %w", path, err)
	}

	return tasks, nil
}
