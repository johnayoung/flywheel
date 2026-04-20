package task

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func validTask() Task {
	return Task{
		ID:                 "task-001",
		Description:        "Implement user login",
		Category:           "feat",
		Priority:           1,
		Prerequisites:      []string{"task-000"},
		Steps:              []string{"Create handler", "Add route"},
		AcceptanceCriteria: AcceptanceCriteria{Conditions: []string{"Returns 200 on success"}},
	}
}

func writeJSON(t *testing.T, path string, v any) {
	t.Helper()
	data, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestParseFile(t *testing.T) {
	dir := t.TempDir()
	task := validTask()
	path := filepath.Join(dir, "task.json")
	writeJSON(t, path, task)

	got, err := ParseFile(path)
	if err != nil {
		t.Fatalf("ParseFile returned error: %v", err)
	}
	if got.ID != task.ID {
		t.Errorf("got ID %q, want %q", got.ID, task.ID)
	}
	if got.Description != task.Description {
		t.Errorf("got Description %q, want %q", got.Description, task.Description)
	}
	if got.Category != task.Category {
		t.Errorf("got Category %q, want %q", got.Category, task.Category)
	}
	if len(got.Steps) != len(task.Steps) {
		t.Errorf("got %d steps, want %d", len(got.Steps), len(task.Steps))
	}
}

func TestParseDir(t *testing.T) {
	dir := t.TempDir()
	t1 := validTask()
	t2 := validTask()
	t2.ID = "task-002"
	t2.Description = "Add logout"

	writeJSON(t, filepath.Join(dir, "a.json"), t1)
	writeJSON(t, filepath.Join(dir, "b.json"), t2)
	if err := os.WriteFile(filepath.Join(dir, "readme.txt"), []byte("ignore"), 0o644); err != nil {
		t.Fatal(err)
	}

	tasks, err := ParseDir(dir)
	if err != nil {
		t.Fatalf("ParseDir returned error: %v", err)
	}
	if len(tasks) != 2 {
		t.Fatalf("got %d tasks, want 2", len(tasks))
	}
}

func TestParseJSONL(t *testing.T) {
	dir := t.TempDir()
	t1 := validTask()
	t2 := validTask()
	t2.ID = "task-002"
	t2.Description = "Add logout"

	d1, _ := json.Marshal(t1)
	d2, _ := json.Marshal(t2)
	content := string(d1) + "\n" + string(d2) + "\n"

	path := filepath.Join(dir, "tasks.jsonl")
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}

	tasks, err := ParseJSONL(path)
	if err != nil {
		t.Fatalf("ParseJSONL returned error: %v", err)
	}
	if len(tasks) != 2 {
		t.Fatalf("got %d tasks, want 2", len(tasks))
	}
	if tasks[0].ID != "task-001" {
		t.Errorf("first task ID = %q, want %q", tasks[0].ID, "task-001")
	}
	if tasks[1].ID != "task-002" {
		t.Errorf("second task ID = %q, want %q", tasks[1].ID, "task-002")
	}
}

func TestValidate_EmptyID(t *testing.T) {
	tk := validTask()
	tk.ID = ""
	if err := tk.Validate(); err == nil {
		t.Error("expected error for empty ID")
	}
}

func TestValidate_WhitespaceInID(t *testing.T) {
	tk := validTask()
	tk.ID = "task 001"
	if err := tk.Validate(); err == nil {
		t.Error("expected error for whitespace in ID")
	}
}

func TestValidate_EmptyDescription(t *testing.T) {
	tk := validTask()
	tk.Description = ""
	if err := tk.Validate(); err == nil {
		t.Error("expected error for empty description")
	}
}

func TestValidate_InvalidCategory(t *testing.T) {
	tk := validTask()
	tk.Category = "feature"
	if err := tk.Validate(); err == nil {
		t.Error("expected error for invalid category")
	}
}


func TestValidate_EmptySteps(t *testing.T) {
	tk := validTask()
	tk.Steps = nil
	if err := tk.Validate(); err == nil {
		t.Error("expected error for empty steps")
	}
}

func TestValidate_SelfReferencingPrerequisite(t *testing.T) {
	tk := validTask()
	tk.Prerequisites = []string{"task-001"}
	if err := tk.Validate(); err == nil {
		t.Error("expected error for self-referencing prerequisite")
	}
}

func TestRoundTrip(t *testing.T) {
	original := Task{
		ID:            "complex-task-99",
		Description:   "A complex task with all fields populated",
		Category:      "refactor",
		Priority:      5,
		Prerequisites: []string{"task-a", "task-b", "task-c"},
		Steps:         []string{"Step one", "Step two", "Step three"},
		AcceptanceCriteria: AcceptanceCriteria{
			Commands:   []string{"go test ./..."},
			Conditions: []string{"Criterion A", "Criterion B"},
		},
	}

	data, err := json.Marshal(original)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	dir := t.TempDir()
	path := filepath.Join(dir, "round.json")
	if err := os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}

	got, err := ParseFile(path)
	if err != nil {
		t.Fatalf("ParseFile: %v", err)
	}

	if got.ID != original.ID {
		t.Errorf("ID: got %q, want %q", got.ID, original.ID)
	}
	if got.Category != original.Category {
		t.Errorf("Category: got %q, want %q", got.Category, original.Category)
	}
	if got.Priority != original.Priority {
		t.Errorf("Priority: got %d, want %d", got.Priority, original.Priority)
	}
	if len(got.Prerequisites) != len(original.Prerequisites) {
		t.Errorf("Prerequisites: got %d, want %d", len(got.Prerequisites), len(original.Prerequisites))
	}
	if len(got.Steps) != len(original.Steps) {
		t.Errorf("Steps: got %d, want %d", len(got.Steps), len(original.Steps))
	}
	if len(got.AcceptanceCriteria.Commands) != len(original.AcceptanceCriteria.Commands) {
		t.Errorf("AcceptanceCriteria.Commands: got %d, want %d", len(got.AcceptanceCriteria.Commands), len(original.AcceptanceCriteria.Commands))
	}
	if len(got.AcceptanceCriteria.Conditions) != len(original.AcceptanceCriteria.Conditions) {
		t.Errorf("AcceptanceCriteria.Conditions: got %d, want %d", len(got.AcceptanceCriteria.Conditions), len(original.AcceptanceCriteria.Conditions))
	}
}
