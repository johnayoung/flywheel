package strategy

import (
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
		t.Error("expected error for empty slug, got nil")
	}
}
