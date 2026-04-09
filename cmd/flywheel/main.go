package main

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"runtime/debug"
	"strings"
	"syscall"

	"github.com/spf13/cobra"

	"github.com/johnayoung/flywheel/internal/agent"
	"github.com/johnayoung/flywheel/internal/agent/claudecode"
	"github.com/johnayoung/flywheel/internal/config"
	"github.com/johnayoung/flywheel/internal/conflict"
	"github.com/johnayoung/flywheel/internal/dag"
	"github.com/johnayoung/flywheel/internal/engine"
	"github.com/johnayoung/flywheel/internal/lifecycle"
	"github.com/johnayoung/flywheel/internal/merge"
	"github.com/johnayoung/flywheel/internal/review"
	"github.com/johnayoung/flywheel/internal/store"
	"github.com/johnayoung/flywheel/internal/store/jsonl"
	"github.com/johnayoung/flywheel/internal/task"
	"github.com/johnayoung/flywheel/internal/validate"
	"github.com/johnayoung/flywheel/internal/worktree"
)

func main() {
	if err := rootCmd().Execute(); err != nil {
		os.Exit(1)
	}
}

// version is set at build time via -ldflags.
// When installed with `go install`, Go embeds the module version automatically.
var version = ""

func resolveVersion() string {
	if version != "" {
		return version
	}
	if info, ok := debug.ReadBuildInfo(); ok && info.Main.Version != "" && info.Main.Version != "(devel)" {
		return info.Main.Version
	}
	return "dev"
}

func rootCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:           "flywheel",
		Short:         "Flywheel orchestrates parallel AI-driven task execution",
		Version:       resolveVersion(),
		SilenceUsage:  true,
		SilenceErrors: true,
	}

	cmd.PersistentFlags().String("config", "flywheel.json", "path to config file")

	cmd.AddCommand(initCmd())
	cmd.AddCommand(runCmd())
	cmd.AddCommand(statusCmd())
	cmd.AddCommand(reviewCmd())
	cmd.AddCommand(cleanCmd())
	cmd.AddCommand(validateCmd())

	return cmd
}

func loadConfig(cmd *cobra.Command) (*config.Config, error) {
	cfgPath, err := cmd.Flags().GetString("config")
	if err != nil {
		return nil, fmt.Errorf("reading config flag: %w", err)
	}
	cfg, err := config.Load(cfgPath)
	if err != nil {
		return nil, err
	}
	if err := config.Validate(cfg); err != nil {
		return nil, fmt.Errorf("invalid config: %w", err)
	}
	return cfg, nil
}

// --- init ---

func initCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "init",
		Short: "Initialize a flywheel run: validate tasks, build DAG, and create lifecycle directory",
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := loadConfig(cmd)
			if err != nil {
				return err
			}

			tasks, err := task.ParseDir(cfg.Store.TasksPath)
			if err != nil {
				return fmt.Errorf("loading tasks: %w", err)
			}
			if len(tasks) == 0 {
				return fmt.Errorf("no tasks found in %s", cfg.Store.TasksPath)
			}

			d, err := dag.Build(tasks)
			if err != nil {
				return fmt.Errorf("building DAG: %w", err)
			}

			checker := dag.NewReadinessChecker(d)
			statuses := make(map[string]lifecycle.Status, len(tasks))
			for _, t := range tasks {
				statuses[t.ID] = lifecycle.StatusPending
			}
			waves := checker.ComputeWaves(statuses)

			fmt.Fprintf(cmd.OutOrStdout(), "Execution plan: %d tasks in %d waves\n\n", len(tasks), len(waves))
			for i, wave := range waves {
				fmt.Fprintf(cmd.OutOrStdout(), "Wave %d:\n", i+1)
				for _, id := range wave {
					desc := ""
					for _, t := range tasks {
						if t.ID == id {
							desc = t.Description
							break
						}
					}
					fmt.Fprintf(cmd.OutOrStdout(), "  - %s: %s\n", id, desc)
				}
			}

			lcDir := cfg.Store.LifecyclePath
			if err := os.MkdirAll(lcDir, 0o755); err != nil {
				return fmt.Errorf("creating lifecycle directory: %w", err)
			}
			if err := os.MkdirAll(".flywheel", 0o755); err != nil {
				return fmt.Errorf("creating .flywheel directory: %w", err)
			}

			fmt.Fprintln(cmd.OutOrStdout(), "\nInitialization complete. Run 'flywheel run' to start execution.")
			return nil
		},
	}
}

// --- run ---

func runCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "run",
		Short: "Execute the flywheel orchestration loop",
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := loadConfig(cmd)
			if err != nil {
				return err
			}

			maxParallel, _ := cmd.Flags().GetInt("max-parallel")
			if maxParallel > 0 {
				cfg.MaxParallel = maxParallel
			}

			dryRun, _ := cmd.Flags().GetBool("dry-run")
			if dryRun {
				return runDryRun(cmd, cfg)
			}

			ctx, cancel := context.WithCancel(context.Background())
			sigCh := make(chan os.Signal, 1)
			signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
			go func() {
				<-sigCh
				fmt.Fprintln(os.Stderr, "\nReceived interrupt, shutting down gracefully...")
				cancel()
			}()

			st, err := jsonl.New(cfg.Store.TasksPath, cfg.Store.LifecyclePath)
			if err != nil {
				return fmt.Errorf("creating store: %w", err)
			}

			wm := worktree.NewManager(cfg.Repo, filepath.Join(".flywheel", "worktrees"), cfg.BranchPrefix)

			agentFn := func() agent.Agent {
				return claudecode.New()
			}

			validator := validate.New(claudecode.New(), cfg.BuildCommand)

			reviewerFn := func(t task.Task) review.Reviewer {
				mode := config.ReviewModeForTask(*cfg, t.Review)
				if mode == "human" {
					return review.NewHITLReviewer(os.Stdin, os.Stdout)
				}
				return review.NewAgentReviewer(claudecode.New())
			}

			merger := merge.NewSequentialMerger(
				cfg.Repo,
				filepath.Join(".flywheel", "merge-workspace"),
				filepath.Join(".flywheel", "merge.lock"),
			)

			resolver := conflict.New(claudecode.New())

			eng := engine.New(*cfg, st, wm, agentFn, validator, reviewerFn, merger, resolver)
			summary, err := eng.Run(ctx)
			if err != nil {
				return fmt.Errorf("engine run failed: %w", err)
			}

			fmt.Fprintf(cmd.OutOrStdout(), "\nRun complete: %s\n", summary.RunID)
			fmt.Fprintf(cmd.OutOrStdout(), "  Total:   %d\n", summary.TotalTasks)
			fmt.Fprintf(cmd.OutOrStdout(), "  Merged:  %d\n", summary.Merged)
			fmt.Fprintf(cmd.OutOrStdout(), "  Failed:  %d\n", summary.Failed)
			fmt.Fprintf(cmd.OutOrStdout(), "  Pending: %d\n", summary.Pending)
			fmt.Fprintf(cmd.OutOrStdout(), "  Duration: %s\n", summary.Duration.Round(1000000000))

			return nil
		},
	}

	cmd.Flags().Int("max-parallel", 0, "override max parallel workers from config")
	cmd.Flags().Bool("dry-run", false, "print execution plan without running")

	return cmd
}

func runDryRun(cmd *cobra.Command, cfg *config.Config) error {
	tasks, err := task.ParseDir(cfg.Store.TasksPath)
	if err != nil {
		return fmt.Errorf("loading tasks: %w", err)
	}

	d, err := dag.Build(tasks)
	if err != nil {
		return fmt.Errorf("building DAG: %w", err)
	}

	checker := dag.NewReadinessChecker(d)
	statuses := make(map[string]lifecycle.Status, len(tasks))
	for _, t := range tasks {
		statuses[t.ID] = lifecycle.StatusPending
	}
	waves := checker.ComputeWaves(statuses)

	fmt.Fprintf(cmd.OutOrStdout(), "Dry run: %d tasks in %d waves (max_parallel=%d)\n\n", len(tasks), len(waves), cfg.MaxParallel)
	for i, wave := range waves {
		fmt.Fprintf(cmd.OutOrStdout(), "Wave %d:\n", i+1)
		for _, id := range wave {
			for _, t := range tasks {
				if t.ID == id {
					fmt.Fprintf(cmd.OutOrStdout(), "  - %s: %s\n", id, t.Description)
					break
				}
			}
		}
	}
	return nil
}

// --- status ---

func statusCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "status [task_id]",
		Short: "Show task status",
		Args:  cobra.MaximumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := loadConfig(cmd)
			if err != nil {
				return err
			}

			st, err := jsonl.New(cfg.Store.TasksPath, cfg.Store.LifecyclePath)
			if err != nil {
				return fmt.Errorf("creating store: %w", err)
			}

			ctx := context.Background()
			showLifecycle, _ := cmd.Flags().GetBool("lifecycle")

			if len(args) == 1 {
				return showTaskStatus(cmd, ctx, st, args[0], showLifecycle)
			}
			return showAllStatus(cmd, ctx, st)
		},
	}

	cmd.Flags().Bool("lifecycle", false, "show detailed lifecycle timestamps")

	return cmd
}

func showTaskStatus(cmd *cobra.Command, ctx context.Context, st store.TaskStore, taskID string, showLifecycle bool) error {
	t, err := st.GetTask(ctx, taskID)
	if err != nil {
		return fmt.Errorf("task %q: %w", taskID, err)
	}

	lc, err := st.GetLifecycle(ctx, taskID)
	if err != nil {
		fmt.Fprintf(cmd.OutOrStdout(), "Task: %s\n", t.ID)
		fmt.Fprintf(cmd.OutOrStdout(), "Description: %s\n", t.Description)
		fmt.Fprintf(cmd.OutOrStdout(), "Status: no lifecycle (not initialized)\n")
		return nil
	}

	fmt.Fprintf(cmd.OutOrStdout(), "Task: %s\n", t.ID)
	fmt.Fprintf(cmd.OutOrStdout(), "Description: %s\n", t.Description)
	fmt.Fprintf(cmd.OutOrStdout(), "Status: %s\n", lc.Status)
	if lc.Branch != "" {
		fmt.Fprintf(cmd.OutOrStdout(), "Branch: %s\n", lc.Branch)
	}
	if lc.WorktreePath != "" {
		fmt.Fprintf(cmd.OutOrStdout(), "Worktree: %s\n", lc.WorktreePath)
	}
	if lc.Error != "" {
		fmt.Fprintf(cmd.OutOrStdout(), "Error: %s\n", lc.Error)
	}

	if showLifecycle {
		fmt.Fprintln(cmd.OutOrStdout(), "\nTimestamps:")
		if lc.Timestamps.CreatedAt != nil {
			fmt.Fprintf(cmd.OutOrStdout(), "  Created:   %s\n", lc.Timestamps.CreatedAt.Format("2006-01-02 15:04:05"))
		}
		if lc.Timestamps.ReadyAt != nil {
			fmt.Fprintf(cmd.OutOrStdout(), "  Ready:     %s\n", lc.Timestamps.ReadyAt.Format("2006-01-02 15:04:05"))
		}
		if lc.Timestamps.StartedAt != nil {
			fmt.Fprintf(cmd.OutOrStdout(), "  Started:   %s\n", lc.Timestamps.StartedAt.Format("2006-01-02 15:04:05"))
		}
		if lc.Timestamps.CompletedAt != nil {
			fmt.Fprintf(cmd.OutOrStdout(), "  Completed: %s\n", lc.Timestamps.CompletedAt.Format("2006-01-02 15:04:05"))
		}
		if lc.Timestamps.ReviewedAt != nil {
			fmt.Fprintf(cmd.OutOrStdout(), "  Reviewed:  %s\n", lc.Timestamps.ReviewedAt.Format("2006-01-02 15:04:05"))
		}
		if lc.Timestamps.MergedAt != nil {
			fmt.Fprintf(cmd.OutOrStdout(), "  Merged:    %s\n", lc.Timestamps.MergedAt.Format("2006-01-02 15:04:05"))
		}
		if lc.Timestamps.FailedAt != nil {
			fmt.Fprintf(cmd.OutOrStdout(), "  Failed:    %s\n", lc.Timestamps.FailedAt.Format("2006-01-02 15:04:05"))
		}
	}

	return nil
}

func showAllStatus(cmd *cobra.Command, ctx context.Context, st store.TaskStore) error {
	tasks, err := st.ListTasks(ctx, store.TaskFilter{})
	if err != nil {
		return fmt.Errorf("listing tasks: %w", err)
	}

	lifecycles, err := st.ListLifecycles(ctx, store.LifecycleFilter{})
	if err != nil {
		return fmt.Errorf("listing lifecycles: %w", err)
	}

	lcMap := make(map[string]lifecycle.Status, len(lifecycles))
	for _, lc := range lifecycles {
		lcMap[lc.TaskID] = lc.Status
	}

	fmt.Fprintf(cmd.OutOrStdout(), "%-30s %-20s %s\n", "ID", "STATUS", "DESCRIPTION")
	fmt.Fprintf(cmd.OutOrStdout(), "%-30s %-20s %s\n", strings.Repeat("-", 30), strings.Repeat("-", 20), strings.Repeat("-", 40))

	for _, t := range tasks {
		status := lifecycle.Status("--")
		if s, ok := lcMap[t.ID]; ok {
			status = s
		}
		desc := t.Description
		if len(desc) > 60 {
			desc = desc[:57] + "..."
		}
		fmt.Fprintf(cmd.OutOrStdout(), "%-30s %-20s %s\n", t.ID, status, desc)
	}

	return nil
}

// --- review ---

func reviewCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "review",
		Short: "Manage task reviews",
	}

	cmd.AddCommand(reviewListCmd())
	cmd.AddCommand(reviewApproveCmd())
	cmd.AddCommand(reviewRejectCmd())

	return cmd
}

func reviewListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list",
		Short: "List tasks awaiting review",
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := loadConfig(cmd)
			if err != nil {
				return err
			}

			st, err := jsonl.New(cfg.Store.TasksPath, cfg.Store.LifecyclePath)
			if err != nil {
				return fmt.Errorf("creating store: %w", err)
			}

			ctx := context.Background()
			lcs, err := st.ListLifecycles(ctx, store.LifecycleFilter{
				Statuses: []lifecycle.Status{lifecycle.StatusReviewing},
			})
			if err != nil {
				return fmt.Errorf("listing lifecycles: %w", err)
			}

			if len(lcs) == 0 {
				fmt.Fprintln(cmd.OutOrStdout(), "No tasks awaiting review.")
				return nil
			}

			fmt.Fprintf(cmd.OutOrStdout(), "%-30s %-20s %s\n", "ID", "STATUS", "BRANCH")
			fmt.Fprintf(cmd.OutOrStdout(), "%-30s %-20s %s\n", strings.Repeat("-", 30), strings.Repeat("-", 20), strings.Repeat("-", 40))

			for _, lc := range lcs {
				fmt.Fprintf(cmd.OutOrStdout(), "%-30s %-20s %s\n", lc.TaskID, lc.Status, lc.Branch)
			}

			return nil
		},
	}
}

func reviewApproveCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "approve <task_id>",
		Short: "Approve a task in review",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := loadConfig(cmd)
			if err != nil {
				return err
			}

			st, err := jsonl.New(cfg.Store.TasksPath, cfg.Store.LifecyclePath)
			if err != nil {
				return fmt.Errorf("creating store: %w", err)
			}

			ctx := context.Background()
			taskID := args[0]

			lc, err := st.GetLifecycle(ctx, taskID)
			if err != nil {
				return fmt.Errorf("getting lifecycle for %q: %w", taskID, err)
			}

			if lc.Status != lifecycle.StatusReviewing {
				return fmt.Errorf("task %q is in status %q, not %q", taskID, lc.Status, lifecycle.StatusReviewing)
			}

			if err := lifecycle.Transition(lc, lifecycle.StatusMerging); err != nil {
				return fmt.Errorf("transitioning task %q: %w", taskID, err)
			}

			if err := st.UpdateLifecycle(ctx, lc); err != nil {
				return fmt.Errorf("updating lifecycle for %q: %w", taskID, err)
			}

			fmt.Fprintf(cmd.OutOrStdout(), "Task %q approved and moved to merging.\n", taskID)
			return nil
		},
	}
}

func reviewRejectCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "reject <task_id>",
		Short: "Reject a task in review",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := loadConfig(cmd)
			if err != nil {
				return err
			}

			reason, _ := cmd.Flags().GetString("reason")
			if reason == "" {
				return fmt.Errorf("--reason is required")
			}

			st, err := jsonl.New(cfg.Store.TasksPath, cfg.Store.LifecyclePath)
			if err != nil {
				return fmt.Errorf("creating store: %w", err)
			}

			ctx := context.Background()
			taskID := args[0]

			lc, err := st.GetLifecycle(ctx, taskID)
			if err != nil {
				return fmt.Errorf("getting lifecycle for %q: %w", taskID, err)
			}

			if lc.Status != lifecycle.StatusReviewing {
				return fmt.Errorf("task %q is in status %q, not %q", taskID, lc.Status, lifecycle.StatusReviewing)
			}

			lc.Error = reason
			if err := lifecycle.Transition(lc, lifecycle.StatusRejected); err != nil {
				return fmt.Errorf("transitioning task %q: %w", taskID, err)
			}

			if err := st.UpdateLifecycle(ctx, lc); err != nil {
				return fmt.Errorf("updating lifecycle for %q: %w", taskID, err)
			}

			fmt.Fprintf(cmd.OutOrStdout(), "Task %q rejected: %s\n", taskID, reason)
			return nil
		},
	}

	cmd.Flags().String("reason", "", "rejection reason (required)")

	return cmd
}

// --- clean ---

func cleanCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "clean",
		Short: "Remove flywheel worktrees and state",
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := loadConfig(cmd)
			if err != nil {
				return err
			}

			worktreesOnly, _ := cmd.Flags().GetBool("worktrees-only")

			ctx := context.Background()
			wm := worktree.NewManager(cfg.Repo, filepath.Join(".flywheel", "worktrees"), cfg.BranchPrefix)

			wts, err := wm.List(ctx)
			if err != nil {
				fmt.Fprintf(cmd.ErrOrStderr(), "Warning: could not list worktrees: %v\n", err)
			} else {
				for _, wt := range wts {
					if err := wm.Remove(ctx, wt.TaskID); err != nil {
						fmt.Fprintf(cmd.ErrOrStderr(), "Warning: could not remove worktree for %s: %v\n", wt.TaskID, err)
					} else {
						fmt.Fprintf(cmd.OutOrStdout(), "Removed worktree: %s (%s)\n", wt.TaskID, wt.Path)
					}
				}
				if len(wts) == 0 {
					fmt.Fprintln(cmd.OutOrStdout(), "No worktrees to remove.")
				}
			}

			if !worktreesOnly {
				if err := os.RemoveAll(".flywheel"); err != nil {
					return fmt.Errorf("removing .flywheel directory: %w", err)
				}
				fmt.Fprintln(cmd.OutOrStdout(), "Removed .flywheel/ directory.")
			}

			return nil
		},
	}

	cmd.Flags().Bool("worktrees-only", false, "only remove worktrees, keep .flywheel/ state")

	return cmd
}

// --- validate ---

func validateCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "validate",
		Short: "Validate tasks and DAG",
	}

	cmd.AddCommand(validateTasksCmd())
	cmd.AddCommand(validateDagCmd())

	return cmd
}

func validateTasksCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "tasks",
		Short: "Validate task definitions",
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := loadConfig(cmd)
			if err != nil {
				return err
			}

			tasks, err := task.ParseDir(cfg.Store.TasksPath)
			if err != nil {
				return fmt.Errorf("task validation failed: %w", err)
			}

			fmt.Fprintf(cmd.OutOrStdout(), "%d tasks validated successfully.\n", len(tasks))
			return nil
		},
	}
}

func validateDagCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "dag",
		Short: "Validate task dependency graph",
		RunE: func(cmd *cobra.Command, args []string) error {
			cfg, err := loadConfig(cmd)
			if err != nil {
				return err
			}

			tasks, err := task.ParseDir(cfg.Store.TasksPath)
			if err != nil {
				return fmt.Errorf("loading tasks: %w", err)
			}

			d, err := dag.Build(tasks)
			if err != nil {
				return fmt.Errorf("DAG validation failed: %w", err)
			}

			roots := d.Roots()
			fmt.Fprintf(cmd.OutOrStdout(), "DAG valid. %d tasks, %d roots.\n", d.NodeCount(), len(roots))
			return nil
		},
	}
}
