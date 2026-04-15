package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestIsGoInstallPath_GOPATH(t *testing.T) {
	gopath := t.TempDir()
	gobin := filepath.Join(gopath, "bin")
	if err := os.MkdirAll(gobin, 0o755); err != nil {
		t.Fatalf("mkdir: %v", err)
	}

	t.Setenv("GOPATH", gopath)
	t.Setenv("GOBIN", "")

	if !isGoInstallPath(filepath.Join(gobin, "flywheel")) {
		t.Fatal("expected binary under GOPATH/bin to be detected as go-install")
	}
	if isGoInstallPath(filepath.Join(t.TempDir(), "flywheel")) {
		t.Fatal("binary outside GOPATH/bin should not be detected as go-install")
	}
}

func TestIsGoInstallPath_GOBIN(t *testing.T) {
	gobin := t.TempDir()
	t.Setenv("GOBIN", gobin)
	t.Setenv("GOPATH", t.TempDir())

	if !isGoInstallPath(filepath.Join(gobin, "flywheel")) {
		t.Fatal("expected binary under GOBIN to be detected as go-install")
	}
}
