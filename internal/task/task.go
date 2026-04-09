package task

import (
	"fmt"
	"strings"
)

// Task represents a unit of work in the Flywheel system.
type Task struct {
	ID                 string   `json:"id"`
	Description        string   `json:"description"`
	Category           string   `json:"category"`
	Priority           int      `json:"priority"`
	Prerequisites      []string `json:"prerequisites"`
	Commit             string   `json:"commit"`
	Steps              []string `json:"steps"`
	AcceptanceCriteria []string `json:"acceptance_criteria"`
	Review             string   `json:"review,omitempty"`
}

var validCategories = map[string]bool{
	"feat":     true,
	"fix":      true,
	"refactor": true,
	"test":     true,
	"docs":     true,
	"chore":    true,
}

var validReviewValues = map[string]bool{
	"agent": true,
	"human": true,
	"none":  true,
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

	if t.Commit == "" {
		return fmt.Errorf("task %q: commit must not be empty", t.ID)
	}

	if len(t.Steps) == 0 {
		return fmt.Errorf("task %q: steps must have at least one entry", t.ID)
	}

	if t.Review != "" && !validReviewValues[t.Review] {
		return fmt.Errorf("task %q: review value %q is invalid; must be \"agent\", \"human\", or \"none\"", t.ID, t.Review)
	}

	for _, prereq := range t.Prerequisites {
		if prereq == t.ID {
			return fmt.Errorf("task %q: must not list itself as a prerequisite", t.ID)
		}
	}

	return nil
}
