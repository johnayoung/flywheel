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
	// HeartbeatInterval is how often the engine emits a run_heartbeat event
	// while workers are active. Suppressed when a phase transition occurred
	// within the last 2 seconds.
	HeartbeatInterval string `json:"heartbeat_interval"`
	// ConsecutiveFailureCap bounds the number of consecutive failed runs
	// (distinct RunIDs) a task can accumulate before auto-reset refuses to
	// revive it. Operators must use `flywheel retry --force` once this is hit.
	ConsecutiveFailureCap int `json:"consecutive_failure_cap"`
}

// StoreConfig selects a store backend and carries its backend-specific
// options as raw JSON. Each backend defines its own options schema and is
// responsible for parsing Options into its concrete type.
type StoreConfig struct {
	Backend string          `json:"backend"`
	Options json.RawMessage `json:"options,omitempty"`
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
			Backend: "jsonl",
		},
		MergeStrategy:      "sequential",
		Review:             "agent",
		Agent:              "claude-code",
		Timeout:            "30m",
		MaxRetries:            2,
		MaxResolveAttempts:    2,
		HeartbeatInterval:     "30s",
		ConsecutiveFailureCap: 2,
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
	if cfg.HeartbeatInterval != "" {
		if _, err := time.ParseDuration(cfg.HeartbeatInterval); err != nil {
			return fmt.Errorf("invalid heartbeat_interval %q: %w", cfg.HeartbeatInterval, err)
		}
	}
	if cfg.ConsecutiveFailureCap < 0 {
		return fmt.Errorf("consecutive_failure_cap must be >= 0, got %d", cfg.ConsecutiveFailureCap)
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
