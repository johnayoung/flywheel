package events

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sync"
)

// JSONLWriter appends one JSON line per event to a file. The file is
// opened with O_APPEND|O_CREATE; concurrent writers are serialized by
// an internal mutex.
type JSONLWriter struct {
	mu  sync.Mutex
	f   *os.File
	enc *json.Encoder
}

// NewJSONLWriter opens (or creates) the events log file at path.
// The parent directory is created if it does not already exist.
func NewJSONLWriter(path string) (*JSONLWriter, error) {
	if dir := filepath.Dir(path); dir != "" && dir != "." {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return nil, fmt.Errorf("creating events dir: %w", err)
		}
	}
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return nil, fmt.Errorf("opening events file: %w", err)
	}
	return &JSONLWriter{f: f, enc: json.NewEncoder(f)}, nil
}

// Handle implements Subscriber.
func (w *JSONLWriter) Handle(e Event) {
	w.mu.Lock()
	defer w.mu.Unlock()
	_ = w.enc.Encode(e)
}

// Close releases the underlying file.
func (w *JSONLWriter) Close() error {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.f == nil {
		return nil
	}
	err := w.f.Close()
	w.f = nil
	return err
}
