package claudecode

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os/exec"
	"strings"
	"sync"

	"github.com/johnayoung/flywheel/internal/agent"
)

// ClaudeCode implements agent.Agent using the Claude Code CLI.
type ClaudeCode struct {
	claudePath string
	streaming  bool
	onChunk    func(string)
}

// Option configures a ClaudeCode instance.
type Option func(*ClaudeCode)

// WithClaudePath sets the path to the claude CLI binary.
func WithClaudePath(path string) Option {
	return func(c *ClaudeCode) {
		c.claudePath = path
	}
}

// WithStreaming enables stream-json output parsing. When true, the agent
// invokes claude with `--output-format stream-json --verbose` and reads
// one JSON event per line. When false (default), behavior is identical
// to the original buffered `--output-format json` mode.
func WithStreaming(enabled bool) Option {
	return func(c *ClaudeCode) {
		c.streaming = enabled
	}
}

// WithOnChunk registers a callback invoked for every interim assistant or
// tool event when streaming is enabled. The callback runs synchronously
// on the goroutine reading the agent's stdout; it must not block.
func WithOnChunk(fn func(string)) Option {
	return func(c *ClaudeCode) {
		c.onChunk = fn
	}
}

// New creates a new ClaudeCode agent with the given options.
func New(opts ...Option) *ClaudeCode {
	c := &ClaudeCode{
		claudePath: "claude",
	}
	for _, opt := range opts {
		opt(c)
	}
	return c
}

// claudeOutput represents the JSON output from claude --output-format json
// (and the terminal `result` event in stream-json mode).
type claudeOutput struct {
	Result  string `json:"result"`
	IsError bool   `json:"is_error"`
}

// BuildPrompt constructs the prompt string for the Claude Code CLI from an execution request.
func BuildPrompt(req agent.ExecutionRequest) string {
	var b strings.Builder

	fmt.Fprintf(&b, "Task: %s\n\n", req.Description)

	if len(req.Steps) > 0 {
		b.WriteString("Steps:\n")
		for i, step := range req.Steps {
			fmt.Fprintf(&b, "%d. %s\n", i+1, step)
		}
		b.WriteString("\n")
	}

	if req.ResumeFrom > 0 {
		fmt.Fprintf(&b, "Resume from step %d. Steps 1 through %d are already completed.\n\n", req.ResumeFrom, req.ResumeFrom-1)
	}

	fmt.Fprintf(&b, "When finished, commit your work with the message: \"task(%s): implement changes\"\n\n", req.TaskID)
	b.WriteString("Stay within scope: only modify files directly related to this task. Do not refactor unrelated code or make changes outside the task requirements.\n")

	return b.String()
}

// Execute runs the Claude Code CLI against the given request.
func (c *ClaudeCode) Execute(ctx context.Context, req agent.ExecutionRequest) (*agent.ExecutionResult, error) {
	prompt := BuildPrompt(req)

	if c.streaming {
		return c.executeStreaming(ctx, req, prompt)
	}
	return c.executeBuffered(ctx, req, prompt)
}

// executeBuffered runs claude with `--output-format json` and parses the
// single JSON document on stdout. Preserves legacy behavior.
func (c *ClaudeCode) executeBuffered(ctx context.Context, req agent.ExecutionRequest, prompt string) (*agent.ExecutionResult, error) {
	cmd := exec.CommandContext(ctx, c.claudePath, "--dangerously-skip-permissions", "-p", prompt, "--output-format", "json")
	cmd.Dir = req.WorktreePath

	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	err := cmd.Run()
	if ctx.Err() != nil {
		return nil, fmt.Errorf("execution cancelled: %w", ctx.Err())
	}

	if err != nil {
		exitErr := ""
		if stderr.Len() > 0 {
			exitErr = stderr.String()
		} else {
			exitErr = err.Error()
		}
		// Try to parse JSON output even on non-zero exit
		var out claudeOutput
		if jsonErr := json.Unmarshal(stdout.Bytes(), &out); jsonErr == nil && out.Result != "" {
			return &agent.ExecutionResult{
				Success:             false,
				StepsCompleted:      0,
				Output:              out.Result,
				ImplementationNotes: out.Result,
				Error:               exitErr,
			}, nil
		}
		return &agent.ExecutionResult{
			Success:             false,
			StepsCompleted:      0,
			Output:              stdout.String(),
			ImplementationNotes: "",
			Error:               exitErr,
		}, nil
	}

	var out claudeOutput
	if jsonErr := json.Unmarshal(stdout.Bytes(), &out); jsonErr != nil {
		return &agent.ExecutionResult{
			Success:             false,
			StepsCompleted:      0,
			Output:              stdout.String(),
			ImplementationNotes: "",
			Error:               fmt.Sprintf("failed to parse claude output: %v", jsonErr),
		}, nil
	}

	if out.IsError {
		return &agent.ExecutionResult{
			Success:             false,
			StepsCompleted:      0,
			Output:              out.Result,
			ImplementationNotes: out.Result,
			Error:               out.Result,
		}, nil
	}

	return &agent.ExecutionResult{
		Success:             true,
		StepsCompleted:      len(req.Steps),
		Output:              out.Result,
		ImplementationNotes: out.Result,
	}, nil
}

// executeStreaming runs claude with `--output-format stream-json --verbose`
// and parses NDJSON events as they arrive. Interim assistant text and
// tool_use names are forwarded to OnChunk; the terminal `result` event
// produces the final ExecutionResult.
func (c *ClaudeCode) executeStreaming(ctx context.Context, req agent.ExecutionRequest, prompt string) (*agent.ExecutionResult, error) {
	cmd := exec.CommandContext(ctx, c.claudePath, "--dangerously-skip-permissions", "-p", prompt, "--output-format", "stream-json", "--verbose")
	cmd.Dir = req.WorktreePath

	stdoutPipe, err := cmd.StdoutPipe()
	if err != nil {
		return nil, fmt.Errorf("stdout pipe: %w", err)
	}
	var stderr bytes.Buffer
	cmd.Stderr = &stderr

	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("starting claude: %w", err)
	}

	var (
		mu        sync.Mutex
		finalOut  claudeOutput
		gotResult bool
		parseErrs int
	)

	scanDone := make(chan struct{})
	go func() {
		defer close(scanDone)
		scanner := bufio.NewScanner(stdoutPipe)
		// Allow large lines: assistant messages can carry sizable text.
		scanner.Buffer(make([]byte, 64*1024), 4*1024*1024)
		for scanner.Scan() {
			line := scanner.Bytes()
			if len(bytes.TrimSpace(line)) == 0 {
				continue
			}
			c.handleStreamLine(line, &mu, &finalOut, &gotResult, &parseErrs)
		}
		// scanner.Err is intentionally ignored; we surface errors via cmd.Wait.
	}()

	waitErr := cmd.Wait()
	<-scanDone
	// Drain any residual data from the pipe (best-effort).
	_, _ = io.Copy(io.Discard, stdoutPipe)

	if ctx.Err() != nil {
		return nil, fmt.Errorf("execution cancelled: %w", ctx.Err())
	}

	mu.Lock()
	got := gotResult
	out := finalOut
	mu.Unlock()

	if waitErr != nil {
		exitErr := stderr.String()
		if exitErr == "" {
			exitErr = waitErr.Error()
		}
		if got && out.Result != "" {
			return &agent.ExecutionResult{
				Success:             false,
				Output:              out.Result,
				ImplementationNotes: out.Result,
				Error:               exitErr,
			}, nil
		}
		return &agent.ExecutionResult{
			Success: false,
			Error:   exitErr,
		}, nil
	}

	if !got {
		return &agent.ExecutionResult{
			Success: false,
			Error:   "stream-json terminated without a result event",
		}, nil
	}

	if out.IsError {
		return &agent.ExecutionResult{
			Success:             false,
			Output:              out.Result,
			ImplementationNotes: out.Result,
			Error:               out.Result,
		}, nil
	}

	return &agent.ExecutionResult{
		Success:             true,
		StepsCompleted:      len(req.Steps),
		Output:              out.Result,
		ImplementationNotes: out.Result,
	}, nil
}

// handleStreamLine parses a single NDJSON line. Recognized event types:
//   - {"type":"result","result":"...","is_error":bool} -> terminal result
//   - {"type":"assistant","message":{"content":[{"type":"text","text":"..."}]}}
//   - {"type":"assistant","message":{"content":[{"type":"tool_use","name":"..."}]}}
//
// All other types are silently dropped.
func (c *ClaudeCode) handleStreamLine(line []byte, mu *sync.Mutex, finalOut *claudeOutput, gotResult *bool, parseErrs *int) {
	var env struct {
		Type    string          `json:"type"`
		Result  string          `json:"result"`
		IsError bool            `json:"is_error"`
		Message json.RawMessage `json:"message"`
	}
	if err := json.Unmarshal(line, &env); err != nil {
		mu.Lock()
		*parseErrs++
		mu.Unlock()
		return
	}
	switch env.Type {
	case "result":
		mu.Lock()
		finalOut.Result = env.Result
		finalOut.IsError = env.IsError
		*gotResult = true
		mu.Unlock()
	case "assistant":
		if c.onChunk == nil || len(env.Message) == 0 {
			return
		}
		var msg struct {
			Content []struct {
				Type string `json:"type"`
				Text string `json:"text"`
				Name string `json:"name"`
			} `json:"content"`
		}
		if err := json.Unmarshal(env.Message, &msg); err != nil {
			return
		}
		for _, part := range msg.Content {
			switch part.Type {
			case "text":
				if part.Text != "" {
					c.onChunk(part.Text)
				}
			case "tool_use":
				if part.Name != "" {
					c.onChunk("[tool_use] " + part.Name)
				}
			}
		}
	}
}
