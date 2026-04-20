package task

import (
	"fmt"
	"strings"
)

// AcceptanceCriteria defines what "done" means for a task.
// Commands are deterministic checks the harness executes (pass/fail).
// Conditions are semantic criteria requiring judgment (LLM or human).
type AcceptanceCriteria struct {
	Commands   []string `json:"commands,omitempty"`
	Conditions []string `json:"conditions"`
}

// Task represents a unit of work in the Flywheel system.
type Task struct {
	ID                 string             `json:"id"`
	Description        string             `json:"description"`
	Category           string             `json:"category"`
	Priority           int                `json:"priority"`
	Prerequisites      []string           `json:"prerequisites"`
	Steps              []string           `json:"steps"`
	AcceptanceCriteria AcceptanceCriteria `json:"acceptance_criteria"`
}

var validCategories = map[string]bool{
	"feat":     true,
	"fix":      true,
	"refactor": true,
	"test":     true,
	"docs":     true,
	"chore":    true,
}

// Validate checks that the task fields satisfy all constraints.
func (t *Task) Validate() error {
	if t.ID == "" {
		return fmt.Errorf("task ID must not be empty")
	}
	if strings.ContainsAny(t.ID, " \t\n\r") {
		return fmt.Errorf("task ID %q must not contain whitespace", t.ID)
	}

	if t.Description == "" {
		return fmt.Errorf("task %q: description must not be empty", t.ID)
	}

	if !validCategories[t.Category] {
		return fmt.Errorf("task %q: category %q is invalid; must be one of: feat, fix, refactor, test, docs, chore", t.ID, t.Category)
	}

	if len(t.Steps) == 0 {
		return fmt.Errorf("task %q: steps must have at least one entry", t.ID)
	}

	if len(t.AcceptanceCriteria.Commands) == 0 && len(t.AcceptanceCriteria.Conditions) == 0 {
		return fmt.Errorf("task %q: acceptance_criteria must have at least one command or condition", t.ID)
	}

	for _, prereq := range t.Prerequisites {
		if prereq == t.ID {
			return fmt.Errorf("task %q: must not list itself as a prerequisite", t.ID)
		}
	}

	return nil
}
