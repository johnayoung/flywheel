// Package storeopen is the plugin seam for store backends. Each supported
// backend is dispatched here based on config.StoreConfig.Backend. Adding a
// new backend means adding a case below and defining an Options type in the
// backend's own package.
package storeopen

import (
	"encoding/json"
	"fmt"

	"github.com/johnayoung/flywheel/internal/config"
	"github.com/johnayoung/flywheel/internal/store"
	"github.com/johnayoung/flywheel/internal/store/jsonl"
)

// Open constructs a store.TaskStore from the given StoreConfig, parsing any
// backend-specific options in cfg.Options.
func Open(cfg config.StoreConfig) (store.TaskStore, error) {
	switch cfg.Backend {
	case "", "jsonl":
		var opts jsonl.Options
		if len(cfg.Options) > 0 {
			if err := json.Unmarshal(cfg.Options, &opts); err != nil {
				return nil, fmt.Errorf("parsing jsonl store options: %w", err)
			}
		}
		return jsonl.New(opts.TasksDir(), opts.LifecycleDir())
	default:
		return nil, fmt.Errorf("unknown store backend %q", cfg.Backend)
	}
}
