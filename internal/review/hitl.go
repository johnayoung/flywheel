package review

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"strings"
	"sync"
)

// HITLReviewer implements human-in-the-loop code review via interactive prompts.
type HITLReviewer struct {
	in  io.Reader
	out io.Writer
	mu  *sync.Mutex
}

// NewHITLReviewer creates a reviewer that prompts a human operator for approval.
func NewHITLReviewer(in io.Reader, out io.Writer) *HITLReviewer {
	return &HITLReviewer{
		in:  in,
		out: out,
		mu:  &sync.Mutex{},
	}
}

// Review presents the diff and task info to the operator and collects a verdict.
func (h *HITLReviewer) Review(_ context.Context, req ReviewRequest) (*ReviewResult, error) {
	h.mu.Lock()
	defer h.mu.Unlock()

	scanner := bufio.NewScanner(h.in)

	// Print summary
	diffLines := strings.Split(req.Diff, "\n")
	files, insertions, deletions := diffStats(diffLines)
	fmt.Fprintf(h.out, "\n--- Review: %s ---\n", req.Task.ID)
	fmt.Fprintf(h.out, "Description: %s\n", req.Task.Description)
	fmt.Fprintf(h.out, "Diff: %d file(s) changed, %d insertion(s), %d deletion(s)\n\n", files, insertions, deletions)

	for {
		fmt.Fprint(h.out, "[a]pprove / [r]eject / [d]iff / [s]kip: ")

		if !scanner.Scan() {
			return nil, fmt.Errorf("unexpected end of input")
		}
		choice := strings.TrimSpace(scanner.Text())

		switch strings.ToLower(choice) {
		case "a":
			return &ReviewResult{Approved: true}, nil

		case "r":
			fmt.Fprint(h.out, "Rejection reason: ")
			if !scanner.Scan() {
				return nil, fmt.Errorf("unexpected end of input reading rejection reason")
			}
			reason := strings.TrimSpace(scanner.Text())
			return &ReviewResult{Approved: false, Feedback: reason}, nil

		case "d":
			fmt.Fprintln(h.out, req.Diff)
			continue

		case "s":
			return &ReviewResult{Approved: false, Feedback: "skipped"}, nil

		default:
			fmt.Fprintf(h.out, "Unknown option %q, try again.\n", choice)
		}
	}
}

// diffStats counts files changed, insertions, and deletions from unified diff lines.
func diffStats(lines []string) (files, insertions, deletions int) {
	seen := make(map[string]bool)
	for _, line := range lines {
		if strings.HasPrefix(line, "+++ ") {
			name := strings.TrimPrefix(line, "+++ ")
			if name != "/dev/null" && !seen[name] {
				seen[name] = true
				files++
			}
		} else if strings.HasPrefix(line, "+") && !strings.HasPrefix(line, "+++") {
			insertions++
		} else if strings.HasPrefix(line, "-") && !strings.HasPrefix(line, "---") {
			deletions++
		}
	}
	return files, insertions, deletions
}
