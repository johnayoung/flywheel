package task

import (
	"encoding/json"
	"testing"
)

func TestDeriveSlug_ScopedCommit(t *testing.T) {
	slug, err := DeriveSlug("feat(api): redesign checkout")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if slug != "redesign-checkout" {
		t.Errorf("got %q, want %q", slug, "redesign-checkout")
	}
}

func TestDeriveSlug_UnscopedCommit(t *testing.T) {
	slug, err := DeriveSlug("feat: redesign checkout")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if slug != "redesign-checkout" {
		t.Errorf("got %q, want %q", slug, "redesign-checkout")
	}
}

func TestDeriveSlug_NonConventional(t *testing.T) {
	slug, err := DeriveSlug("some random commit message")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if slug != "some-random-commit-message" {
		t.Errorf("got %q, want %q", slug, "some-random-commit-message")
	}
}

func TestDeriveSlug_EmptySlugError(t *testing.T) {
	_, err := DeriveSlug("!!!")
	if err == nil {
		t.Error("expected error for empty slug commit, got nil")
	}
}

func TestValidate_EmptyAcceptanceCriteria(t *testing.T) {
	tk := validTask()
	tk.AcceptanceCriteria = nil
	if err := tk.Validate(); err == nil {
		t.Error("expected error for empty acceptance_criteria")
	}
}

func TestValidate_EmptySlugCommitRejected(t *testing.T) {
	tk := validTask()
	tk.Commit = "!!!"
	if err := tk.Validate(); err == nil {
		t.Error("expected error for commit that slugifies to empty")
	}
}

func TestValidate_MergeValidValues(t *testing.T) {
	for _, v := range []string{"auto", "manual", ""} {
		tk := validTask()
		tk.Merge = v
		if err := tk.Validate(); err != nil {
			t.Errorf("unexpected error for merge=%q: %v", v, err)
		}
	}
}

func TestValidate_MergeInvalidValue(t *testing.T) {
	tk := validTask()
	tk.Merge = "squash"
	if err := tk.Validate(); err == nil {
		t.Error("expected error for invalid merge value")
	}
}

func TestMergeFieldJSONRoundTrip(t *testing.T) {
	original := validTask()
	original.Merge = "auto"

	data, err := json.Marshal(original)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var got Task
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	if got.Merge != "auto" {
		t.Errorf("Merge: got %q, want %q", got.Merge, "auto")
	}
}

func TestMergeFieldOmittedWhenEmpty(t *testing.T) {
	tk := validTask()
	tk.Merge = ""

	data, err := json.Marshal(tk)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}

	var m map[string]interface{}
	if err := json.Unmarshal(data, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	if _, ok := m["merge"]; ok {
		t.Error("merge key should be omitted when empty")
	}
}
