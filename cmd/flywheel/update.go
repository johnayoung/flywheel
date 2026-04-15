package main

import (
	"fmt"
	"go/build"
	"os"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/creativeprojects/go-selfupdate"
	"github.com/spf13/cobra"
)

const (
	updateRepoOwner = "johnayoung"
	updateRepoName  = "flywheel"
	updateChecksums = "checksums.txt"
)

func updateCmd() *cobra.Command {
	var (
		checkOnly  bool
		force      bool
		pinVersion string
	)

	cmd := &cobra.Command{
		Use:   "update",
		Short: "Update flywheel to the latest released version",
		Long: `Download the latest flywheel release from GitHub and replace the running binary.

Checksums are verified against the release's checksums.txt before replacement.

If flywheel was installed with 'go install', this command will instead print the
go install invocation needed to upgrade, so it doesn't clobber a Go-managed binary.`,
		RunE: func(cmd *cobra.Command, args []string) error {
			return runUpdate(cmd, checkOnly, force, pinVersion)
		},
	}

	cmd.Flags().BoolVar(&checkOnly, "check", false, "only check for a newer version; do not download")
	cmd.Flags().BoolVar(&force, "force", false, "re-install even if already on the target version, or if running a local dev build")
	cmd.Flags().StringVar(&pinVersion, "version", "", "install a specific release tag (e.g. v0.3.1) instead of the latest")

	return cmd
}

func runUpdate(cmd *cobra.Command, checkOnly, force bool, pinVersion string) error {
	ctx := cmd.Context()
	out := cmd.OutOrStdout()

	current := resolveVersion()
	fmt.Fprintf(out, "Current version: %s\n", current)

	exePath, err := os.Executable()
	if err != nil {
		return fmt.Errorf("resolving executable path: %w", err)
	}
	resolved, err := filepath.EvalSymlinks(exePath)
	if err == nil {
		exePath = resolved
	}

	if isGoInstallPath(exePath) {
		fmt.Fprintln(out, "flywheel appears to be installed via 'go install'.")
		fmt.Fprintln(out, "To upgrade, run:")
		fmt.Fprintln(out, "  go install github.com/johnayoung/flywheel/cmd/flywheel@latest")
		return nil
	}

	if current == "dev" && !force {
		return fmt.Errorf("running a local dev build; refusing to overwrite. Re-run with --force to replace this binary with a released version")
	}

	updater, err := selfupdate.NewUpdater(selfupdate.Config{
		Validator: &selfupdate.ChecksumValidator{UniqueFilename: updateChecksums},
	})
	if err != nil {
		return fmt.Errorf("creating updater: %w", err)
	}
	repo := selfupdate.NewRepositorySlug(updateRepoOwner, updateRepoName)

	var release *selfupdate.Release
	var found bool
	if pinVersion != "" {
		release, found, err = updater.DetectVersion(ctx, repo, strings.TrimPrefix(pinVersion, "v"))
		if err != nil {
			return fmt.Errorf("looking up release %s: %w", pinVersion, err)
		}
		if !found {
			return fmt.Errorf("release %s not found (no asset for %s/%s)", pinVersion, selfupdateOS(), selfupdateArch())
		}
	} else {
		release, found, err = updater.DetectLatest(ctx, repo)
		if err != nil {
			return fmt.Errorf("checking for latest release: %w", err)
		}
		if !found {
			return fmt.Errorf("no release found (no asset for %s/%s)", selfupdateOS(), selfupdateArch())
		}
	}

	label := "Latest version"
	if pinVersion != "" {
		label = "Target version"
	}
	fmt.Fprintf(out, "%s:  v%s\n", label, release.Version())

	if current != "dev" && pinVersion == "" && !force {
		currentSemver := strings.TrimPrefix(current, "v")
		if release.LessOrEqual(currentSemver) {
			fmt.Fprintln(out, "Already up to date.")
			return nil
		}
	}

	if checkOnly {
		fmt.Fprintf(out, "A newer version is available: v%s\n", release.Version())
		if release.URL != "" {
			fmt.Fprintf(out, "Release notes: %s\n", release.URL)
		}
		return nil
	}

	fmt.Fprintf(out, "Downloading %s...\n", release.AssetName)
	if err := updater.UpdateTo(ctx, release, exePath); err != nil {
		return fmt.Errorf("applying update: %w", err)
	}

	fmt.Fprintf(out, "Updated to v%s.\n", release.Version())
	if release.URL != "" {
		fmt.Fprintf(out, "Release notes: %s\n", release.URL)
	}
	return nil
}

// isGoInstallPath reports whether exePath lives under the Go toolchain's
// install directory (GOBIN or GOPATH/bin). Such binaries are managed by
// `go install` and should be upgraded through that same mechanism rather
// than overwritten in place.
func isGoInstallPath(exePath string) bool {
	exeDir := filepath.Dir(exePath)
	candidates := []string{os.Getenv("GOBIN")}
	if gopath := os.Getenv("GOPATH"); gopath != "" {
		for _, p := range filepath.SplitList(gopath) {
			candidates = append(candidates, filepath.Join(p, "bin"))
		}
	} else {
		candidates = append(candidates, filepath.Join(build.Default.GOPATH, "bin"))
	}
	for _, c := range candidates {
		if c == "" {
			continue
		}
		if resolved, err := filepath.EvalSymlinks(c); err == nil {
			c = resolved
		}
		if pathsEqual(c, exeDir) {
			return true
		}
	}
	return false
}

func pathsEqual(a, b string) bool {
	ca, err := filepath.Abs(a)
	if err != nil {
		return false
	}
	cb, err := filepath.Abs(b)
	if err != nil {
		return false
	}
	return filepath.Clean(ca) == filepath.Clean(cb)
}

func selfupdateOS() string   { return runtime.GOOS }
func selfupdateArch() string { return runtime.GOARCH }
