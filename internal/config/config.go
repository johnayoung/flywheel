package config

import (
	"encoding/json"
	"fmt"
	"os"
	"time"
)

// Config holds the full run configuration for a Flywheel session.
type Config struct {
	Version            string      `json:"version"`
	Repo               string      `json:"repo"`
	BaseRef            string      `json:"base_ref"`
	BranchPrefix       string      `json:"branch_prefix"`
	MaxParallel        int         `json:"max_parallel"`
	BuildCommand       string      `json:"build_command"`
	Store              StoreConfig `json:"store"`
	MergeStrategy      string      `json:"merge_strategy"`
	Review             string      `json:"review"`
	Agent              string      `json:"agent"`
	Timeout            string      `json:"timeout"`
	MaxRetries         int         `json:"max_retries"`
	MaxResolveAttempts int         `json:"max_resolve_attempts"`
}

// StoreConfig holds configuration for the task/lifecycle store backend.
type StoreConfig struct {
	Backend       string `json:"backend"`
	TasksPath     string `json:"tasks_path"`
	LifecyclePath string `json:"lifecycle_path"`
	DSN           string `json:"dsn,omitempty"`
}

// LoadWithDefaults returns a Config populated with sensible defaults.
func LoadWithDefaults() *Config {
	return &Config{
		Version:      "1",
		Repo:         ".",
		BaseRef:      "main",
		BranchPrefix: "flywheel/",
		MaxParallel:  3,
		Store: StoreConfig{
			Backend:       "jsonl",
			TasksPath:     "./tasks",
			LifecyclePath: "./.flywheel/lifecycle",
		},
		MergeStrategy:      "sequential",
		Review:             "agent",
		Agent:              "claude-code",
		Timeout:            "30m",
		MaxRetries:         2,
		MaxResolveAttempts: 2,
	}
}

// Load reads a JSON config file from path, layering its values on top of the
// defaults so that any omitted fields retain their default values.
func Load(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("reading config file: %w", err)
	}

	cfg := LoadWithDefaults()
	if err := json.Unmarshal(data, cfg); err != nil {
		return nil, fmt.Errorf("parsing config file: %w", err)
	}
	return cfg, nil
}

// Validate checks cfg for constraint violations and returns the first error found.
func Validate(cfg *Config) error {
	if cfg.Repo == "" {
		return fmt.Errorf("repo must not be empty")
	}
	if cfg.MergeStrategy != "sequential" {
		return fmt.Errorf("merge strategy %q not yet implemented", cfg.MergeStrategy)
	}
	if cfg.Store.Backend != "jsonl" {
		return fmt.Errorf("store backend %q not yet implemented", cfg.Store.Backend)
	}
	switch cfg.Review {
	case "agent", "human", "none":
		// valid
	default:
		return fmt.Errorf("review must be one of: agent, human, none; got %q", cfg.Review)
	}
	if cfg.MaxParallel < 1 {
		return fmt.Errorf("max_parallel must be >= 1, got %d", cfg.MaxParallel)
	}
	if _, err := time.ParseDuration(cfg.Timeout); err != nil {
		return fmt.Errorf("invalid timeout %q: %w", cfg.Timeout, err)
	}
	if cfg.MaxRetries < 0 {
		return fmt.Errorf("max_retries must be >= 0, got %d", cfg.MaxRetries)
	}
	if cfg.MaxResolveAttempts < 0 {
		return fmt.Errorf("max_resolve_attempts must be >= 0, got %d", cfg.MaxResolveAttempts)
	}
	return nil
}

// ReviewModeForTask returns the review mode for a specific task. If the task
// specifies its own review mode (non-empty taskReview), that takes precedence
// over the config-level default.
func ReviewModeForTask(cfg Config, taskReview string) string {
	if taskReview != "" {
		return taskReview
	}
	return cfg.Review
}
