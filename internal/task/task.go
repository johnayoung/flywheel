package task

import (
	"fmt"
	"regexp"
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
	Merge              string   `json:"merge,omitempty"`
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

var validMergeValues = map[string]bool{
	"auto":   true,
	"manual": true,
}

var conventionalCommitRe = regexp.MustCompile(`^(feat|fix|refactor|test|docs|chore)(\([^)]+\))?:\s*(.+)$`)

// DeriveSlug extracts the subject from a conventional commit message and slugifies it.
// For a scoped commit like "feat(api): redesign checkout", it returns "redesign-checkout".
// For a non-conventional commit, it slugifies the full string.
// Returns an error if the resulting slug is empty.
func DeriveSlug(commit string) (string, error) {
	subject := commit
	if m := conventionalCommitRe.FindStringSubmatch(commit); m != nil {
		subject = m[3]
	}
	slug := slugify(subject)
	if slug == "" {
		return "", fmt.Errorf("commit %q produces an empty slug", commit)
	}
	return slug, nil
}

func slugify(s string) string {
	s = strings.ToLower(s)
	var b strings.Builder
	prevDash := false
	for _, r := range s {
		if (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') {
			b.WriteRune(r)
			prevDash = false
		} else {
			if !prevDash && b.Len() > 0 {
				b.WriteByte('-')
				prevDash = true
			}
		}
	}
	return strings.TrimRight(b.String(), "-")
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

	if _, err := DeriveSlug(t.Commit); err != nil {
		return fmt.Errorf("task %q: %w", t.ID, err)
	}

	if len(t.Steps) == 0 {
		return fmt.Errorf("task %q: steps must have at least one entry", t.ID)
	}

	if len(t.AcceptanceCriteria) == 0 {
		return fmt.Errorf("task %q: acceptance_criteria must have at least one entry", t.ID)
	}

	if t.Review != "" && !validReviewValues[t.Review] {
		return fmt.Errorf("task %q: review value %q is invalid; must be \"agent\", \"human\", or \"none\"", t.ID, t.Review)
	}

	if t.Merge != "" && !validMergeValues[t.Merge] {
		return fmt.Errorf("task %q: merge value %q is invalid; must be \"auto\" or \"manual\"", t.ID, t.Merge)
	}

	for _, prereq := range t.Prerequisites {
		if prereq == t.ID {
			return fmt.Errorf("task %q: must not list itself as a prerequisite", t.ID)
		}
	}

	return nil
}
