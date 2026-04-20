package task

import (
	"testing"
)

func TestValidate_EmptyAcceptanceCriteria(t *testing.T) {
	tk := validTask()
	tk.AcceptanceCriteria = AcceptanceCriteria{}
	if err := tk.Validate(); err == nil {
		t.Error("expected error for empty acceptance_criteria")
	}
}

func TestValidate_CommandsOnlySufficient(t *testing.T) {
	tk := validTask()
	tk.AcceptanceCriteria = AcceptanceCriteria{Commands: []string{"go test ./..."}}
	if err := tk.Validate(); err != nil {
		t.Errorf("unexpected error: %v", err)
	}
}

func TestValidate_ConditionsOnlySufficient(t *testing.T) {
	tk := validTask()
	tk.AcceptanceCriteria = AcceptanceCriteria{Conditions: []string{"API returns 200"}}
	if err := tk.Validate(); err != nil {
		t.Errorf("unexpected error: %v", err)
	}
}
