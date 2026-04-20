package strategy

import (
	"context"
	"fmt"
	"regexp"
	"strings"

	"github.com/johnayoung/flywheel/internal/task"
)

type Context struct {
	WorkDir string
	Env     map[string]string
	Meta    map[string]any
}

type Result struct {
	Ref  string
	Meta map[string]any
}

type Strategy interface {
	Setup(ctx context.Context, t *task.Task) (*Context, error)
	Submit(ctx context.Context, sctx *Context) (*Result, error)
	Cleanup(ctx context.Context, sctx *Context) error
}

var conventionalCommitRe = regexp.MustCompile(`^(feat|fix|refactor|test|docs|chore)(\([^)]+\))?:\s*(.+)$`)

// DeriveSlug extracts the subject from a conventional commit message and slugifies it.
func DeriveSlug(commit string) (string, error) {
	subject := commit
	if m := conventionalCommitRe.FindStringSubmatch(commit); m != nil {
		subject = m[3]
	}
	slug := slugify(subject)
	if slug == "" {
		return "", fmt.Errorf("commit %q produces an empty slug", commit)
	}
	return slug, nil
}

func slugify(s string) string {
	s = strings.ToLower(s)
	var b strings.Builder
	prevDash := false
	for _, r := range s {
		if (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') {
			b.WriteRune(r)
			prevDash = false
		} else {
			if !prevDash && b.Len() > 0 {
				b.WriteByte('-')
				prevDash = true
			}
		}
	}
	return strings.TrimRight(b.String(), "-")
}
