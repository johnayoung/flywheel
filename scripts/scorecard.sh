#!/usr/bin/env bash
# scorecard.sh -- print one JSON object of repo-health / performance metrics.
#
# Used by the improvement loop to compare a candidate change against the last
# kept baseline. Progress goes to stderr; stdout carries only the JSON.
#
# Usage:
#   scripts/scorecard.sh [--skip-coverage] [--skip-build] [--skip-tests]
#                        [--skip-clippy] [--skip-bench] [--fast]
#
#   --fast   = --skip-coverage --skip-build --skip-bench (static metrics,
#              clippy, and test time only). Default runs every section.
#   A skipped section reports null for its fields.
#
# Metrics and method:
#   coverage_line_pct      cargo llvm-cov --workspace --summary-only --json,
#                          totals.lines.percent (needs cargo-llvm-cov).
#   unwrap_expect_nontest  occurrences of `.unwrap()` and `.expect(` in
#                          crates/*/src/**/*.rs, excluding: files named
#                          tests.rs or *_tests.rs, any file under a `tests`
#                          directory, and every item annotated #[cfg(test)]
#                          (the attribute's following item is skipped up to
#                          its terminating `;` or its balanced closing `}`).
#                          Line comments and string-literal contents are
#                          blanked before matching and brace counting; raw
#                          strings are not parsed, so the count is a close
#                          approximation that is stable run to run.
#   largest_module_loc     max line count over the same non-test file set,
#                          with its path (largest_module_path).
#   clippy_pedantic_warnings  distinct (file, line, col, lint) clippy
#                          warnings from cargo clippy --workspace
#                          --all-targets -- -W clippy::pedantic.
#   clean_build_secs       proxy: wall-clock of cargo build --release
#                          -p flywheel after cargo clean --release for the
#                          six workspace crates only (third-party deps stay
#                          cached), so it measures our own compile cost.
#   test_secs              wall-clock of cargo test --workspace after the
#                          test binaries are pre-built (--no-run), i.e. test
#                          execution time, not compile time.
#   release_binary_bytes   size of target/release/flywheel.
#   bench_median_ns        criterion median point estimate per bench id
#                          (<group>/<bench>) from
#                          target/criterion/*/*/new/estimates.json, after a
#                          fresh cargo bench of the workspace bench targets.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

skip_coverage=0
skip_build=0
skip_tests=0
skip_clippy=0
skip_bench=0
for arg in "$@"; do
  case "$arg" in
    --skip-coverage) skip_coverage=1 ;;
    --skip-build) skip_build=1 ;;
    --skip-tests) skip_tests=1 ;;
    --skip-clippy) skip_clippy=1 ;;
    --skip-bench) skip_bench=1 ;;
    --fast) skip_coverage=1; skip_build=1; skip_bench=1 ;;
    -h|--help) sed -n '2,45p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "scorecard: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

WORKSPACE_CRATES=(flywheel-agents flywheel-core flywheel-orchestrator flywheel-worktree flywheel-container flywheel)
BENCH_TARGETS=(core_hot_paths orchestrator_hot_paths)

log() { echo "[scorecard] $*" >&2; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

now_secs() { date +%s.%N; }
elapsed() { python3 -c "import sys; print(round(float(sys.argv[2]) - float(sys.argv[1]), 2))" "$1" "$2"; }

# --- static metrics ----------------------------------------------------------------
log "static source metrics"
python3 - "$REPO_ROOT" > "$tmp/static.json" <<'PY'
import json, pathlib, re, sys

root = pathlib.Path(sys.argv[1])
STRING = re.compile(r'"(?:\\.|[^"\\])*"')
CHAR = re.compile(r"'(?:\\.|[^'\\])'")
PATTERN = re.compile(r"\.unwrap\(\)|\.expect\(")

def is_test_file(path):
    rel = path.relative_to(root)
    return (
        "tests" in rel.parts[:-1]
        or path.name == "tests.rs"
        or path.name.endswith("_tests.rs")
    )

def clean(line):
    # Blank string/char literal contents, then drop a trailing line comment.
    line = STRING.sub('""', line)
    line = CHAR.sub("' '", line)
    idx = line.find("//")
    return line if idx < 0 else line[:idx]

def count_nontest(lines):
    total = 0
    i = 0
    n = len(lines)
    while i < n:
        code = clean(lines[i])
        if code.strip().startswith("#[cfg(test)]"):
            # Skip the annotated item: up to `;` before any `{`, or the
            # balanced close of the first `{`.
            depth = 0
            opened = False
            rest = code.strip()[len("#[cfg(test)]"):]
            j = i
            while j < n:
                seg = rest if j == i else clean(lines[j])
                done = False
                for ch in seg:
                    if ch == "{":
                        depth += 1
                        opened = True
                    elif ch == "}":
                        depth -= 1
                        if opened and depth == 0:
                            done = True
                            break
                    elif ch == ";" and not opened:
                        done = True
                        break
                if done:
                    break
                j += 1
            i = j + 1
            continue
        total += len(PATTERN.findall(code))
        i += 1
    return total

files = sorted(
    p for p in root.glob("crates/*/src/**/*.rs") if not is_test_file(p)
)
unwraps = 0
largest = (0, "")
for path in files:
    lines = path.read_text(encoding="utf-8").splitlines()
    unwraps += count_nontest(lines)
    if len(lines) > largest[0]:
        largest = (len(lines), str(path.relative_to(root)))

json.dump(
    {
        "unwrap_expect_nontest": unwraps,
        "largest_module_loc": largest[0],
        "largest_module_path": largest[1],
        "nontest_source_files": len(files),
    },
    sys.stdout,
)
PY

# --- coverage ----------------------------------------------------------------------
coverage=null
if [[ $skip_coverage -eq 0 ]]; then
  if ! cargo llvm-cov --version >/dev/null 2>&1; then
    echo "scorecard: cargo-llvm-cov not installed (cargo install cargo-llvm-cov)" >&2
    exit 1
  fi
  log "coverage (cargo llvm-cov)"
  cargo llvm-cov --workspace --summary-only --json --output-path "$tmp/cov.json" >"$tmp/cov.log" 2>&1 \
    || { cat "$tmp/cov.log" >&2; exit 1; }
  coverage="$(python3 -c "import json,sys; print(round(json.load(open(sys.argv[1]))['data'][0]['totals']['lines']['percent'], 2))" "$tmp/cov.json")"
fi

# --- clippy pedantic ---------------------------------------------------------------
pedantic=null
if [[ $skip_clippy -eq 0 ]]; then
  log "clippy pedantic"
  cargo clippy --workspace --all-targets --message-format=json -- -W clippy::pedantic \
    >"$tmp/clippy.jsonl" 2>"$tmp/clippy.log" || { cat "$tmp/clippy.log" >&2; exit 1; }
  pedantic="$(python3 - "$tmp/clippy.jsonl" <<'PY'
import json, sys
seen = set()
for raw in open(sys.argv[1], encoding="utf-8"):
    try:
        msg = json.loads(raw)
    except ValueError:
        continue
    if msg.get("reason") != "compiler-message":
        continue
    m = msg["message"]
    code = (m.get("code") or {}).get("code") or ""
    if m.get("level") != "warning" or not code.startswith("clippy::"):
        continue
    span = next((s for s in m.get("spans", []) if s.get("is_primary")), None)
    key = (code, span["file_name"], span["line_start"], span["column_start"]) if span else (code, m.get("message"))
    seen.add(key)
print(len(seen))
PY
)"
fi

# --- clean build + binary size -----------------------------------------------------
build_secs=null
binary_bytes=null
if [[ $skip_build -eq 0 ]]; then
  log "release build of workspace crates"
  clean_args=()
  for crate in "${WORKSPACE_CRATES[@]}"; do clean_args+=(-p "$crate"); done
  cargo clean --release "${clean_args[@]}" >/dev/null 2>&1
  start="$(now_secs)"
  cargo build --release -p flywheel >"$tmp/build.log" 2>&1 || { cat "$tmp/build.log" >&2; exit 1; }
  build_secs="$(elapsed "$start" "$(now_secs)")"
  binary_bytes="$(stat -c %s target/release/flywheel 2>/dev/null || stat -f %z target/release/flywheel)"
fi

# --- test wall-clock ---------------------------------------------------------------
test_secs=null
if [[ $skip_tests -eq 0 ]]; then
  log "cargo test (pre-build, then timed run)"
  cargo test --workspace --no-run >"$tmp/test-build.log" 2>&1 || { cat "$tmp/test-build.log" >&2; exit 1; }
  start="$(now_secs)"
  cargo test --workspace >"$tmp/test.log" 2>&1 || { cat "$tmp/test.log" >&2; exit 1; }
  test_secs="$(elapsed "$start" "$(now_secs)")"
fi

# --- benches -----------------------------------------------------------------------
echo '{}' > "$tmp/bench.json"
benches=null
if [[ $skip_bench -eq 0 ]]; then
  log "criterion benches"
  rm -rf target/criterion
  bench_args=()
  for bench in "${BENCH_TARGETS[@]}"; do bench_args+=(--bench "$bench"); done
  cargo bench --workspace "${bench_args[@]}" >"$tmp/bench.log" 2>&1 || { cat "$tmp/bench.log" >&2; exit 1; }
  python3 - target/criterion > "$tmp/bench.json" <<'PY'
import json, pathlib, sys
out = {}
for est in sorted(pathlib.Path(sys.argv[1]).glob("*/*/new/estimates.json")):
    bench_dir = est.parent.parent
    key = f"{bench_dir.parent.name}/{bench_dir.name}"
    out[key] = round(json.loads(est.read_text())["median"]["point_estimate"], 1)
json.dump(out, sys.stdout)
PY
  benches=present
fi

# --- assemble ----------------------------------------------------------------------
python3 - "$tmp/static.json" "$tmp/bench.json" \
  "$coverage" "$pedantic" "$build_secs" "$test_secs" "$binary_bytes" "$benches" \
  "$(git rev-parse HEAD)" <<'PY'
import json, sys

static = json.load(open(sys.argv[1]))
bench = json.load(open(sys.argv[2]))
coverage, pedantic, build, tests, binary, benches, commit = sys.argv[3:10]

def num(value):
    return None if value == "null" else json.loads(value)

result = {
    "commit": commit,
    "coverage_line_pct": num(coverage),
    "unwrap_expect_nontest": static["unwrap_expect_nontest"],
    "largest_module_loc": static["largest_module_loc"],
    "largest_module_path": static["largest_module_path"],
    "clippy_pedantic_warnings": num(pedantic),
    "clean_build_secs": num(build),
    "test_secs": num(tests),
    "release_binary_bytes": num(binary),
    "bench_median_ns": bench if benches != "null" else None,
}
json.dump(result, sys.stdout, indent=2)
print()
PY
