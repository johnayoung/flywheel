#!/usr/bin/env bash
# scorecard.sh -- print one JSON object of repo-health / performance metrics.
#
# Used by the improvement loop to compare a candidate change against the last
# kept baseline. Progress goes to stderr; stdout carries only the JSON.
#
# Usage:
#   scripts/scorecard.sh [--skip-coverage] [--skip-build] [--skip-tests]
#                        [--skip-clippy] [--skip-bench] [--fast]
#                        [--list-unwrap-sites]
#
#   --fast   = --skip-coverage --skip-build --skip-bench (static metrics,
#              clippy, and test time only). Default runs every section.
#   A skipped section reports null for its fields.
#
# Metrics and method:
#   coverage_line_pct      cargo llvm-cov --workspace --summary-only --json,
#                          totals.lines.percent (needs cargo-llvm-cov).
#   unwrap_expect_nontest  `.unwrap()` and `.expect(` method calls in
#                          crates/*/src/**/*.rs, excluding test-only code.
#                          Each file is lexed into Rust tokens (line, nested
#                          block and doc comments dropped; string, raw string
#                          r#"..."#, byte/C-string and char literals made
#                          opaque, multi-line included; lifetimes told apart
#                          from chars), so braces or calls inside literals
#                          never count. Excluded: files named tests.rs or
#                          *_tests.rs or under a `tests` directory; every item
#                          carrying #[test], #[cfg(test)] or
#                          #[cfg(all(.., test, ..))] (skipped to its `;` at
#                          depth 0 or the balanced close of its first `{`);
#                          files with an inner #![cfg(test)]; and the file /
#                          directory of every out-of-line `#[cfg(test)] mod
#                          x;`. --list-unwrap-sites prints each counted
#                          file:line to stderr for auditing.
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
list_unwrap_sites=0
for arg in "$@"; do
  case "$arg" in
    --skip-coverage) skip_coverage=1 ;;
    --skip-build) skip_build=1 ;;
    --skip-tests) skip_tests=1 ;;
    --skip-clippy) skip_clippy=1 ;;
    --skip-bench) skip_bench=1 ;;
    --fast) skip_coverage=1; skip_build=1; skip_bench=1 ;;
    --list-unwrap-sites) list_unwrap_sites=1 ;;
    -h|--help) sed -n '2,51p' "${BASH_SOURCE[0]}"; exit 0 ;;
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
python3 - "$REPO_ROOT" "$list_unwrap_sites" > "$tmp/static.json" <<'PY'
import json, pathlib, re, sys

root = pathlib.Path(sys.argv[1])
LIST_SITES = sys.argv[2] == "1"
IDENT_START = re.compile(r"[A-Za-z_]")
IDENT = re.compile(r"[A-Za-z0-9_]*")
NUMBER = re.compile(r"[0-9][0-9A-Za-z_]*(?:\.[0-9][0-9A-Za-z_]*)?")
RAW_PREFIX = re.compile(r'(?:br|cr|r)(#*)"')
STR_PREFIX = re.compile(r'(?:b|c)?"')
CHAR_LIT = re.compile(r"b?'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\}|.)|[^\\'\n])'")
OPEN, CLOSE = "([{", ")]}"


def tokenize(text):
    """Lex Rust source into (kind, value, line) tokens.

    Comments (line, nested block, doc) are dropped; string, raw-string,
    byte/C-string and char literals become one opaque 'lit' token, so
    nothing inside a literal can ever be mistaken for code.
    """
    toks = []
    i, n, line = 0, len(text), 1
    while i < n:
        c = text[i]
        if c == "\n":
            line += 1
            i += 1
        elif c.isspace():
            i += 1
        elif text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            line += text.count("\n", i, j)
            i = j
        elif m := RAW_PREFIX.match(text, i):
            close = '"' + m.group(1)
            j = text.find(close, m.end())
            j = n if j < 0 else j + len(close)
            toks.append(("lit", "", line))
            line += text.count("\n", i, j)
            i = j
        elif m := STR_PREFIX.match(text, i):
            j = m.end()
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            j += 1
            toks.append(("lit", "", line))
            line += text.count("\n", i, j)
            i = j
        elif m := CHAR_LIT.match(text, i):
            toks.append(("lit", "", line))
            i = m.end()
        elif c == "'":
            # Lifetime or label: `'a`, `'static`.
            m = IDENT.match(text, i + 1)
            toks.append(("lifetime", text[i : m.end()], line))
            i = m.end()
        elif IDENT_START.match(c):
            m = IDENT.match(text, i)
            toks.append(("ident", text[i : m.end()], line))
            i = m.end()
        elif c.isdigit():
            m = NUMBER.match(text, i)
            toks.append(("lit", "", line))
            i = m.end()
        else:
            toks.append(("punct", c, line))
            i += 1
    return toks


def matching(toks, k):
    """Index of the delimiter closing the group opened at toks[k]."""
    depth = 0
    for j in range(k, len(toks)):
        kind, val, _ = toks[j]
        if kind == "punct" and val in OPEN:
            depth += 1
        elif kind == "punct" and val in CLOSE:
            depth -= 1
            if depth == 0:
                return j
    return len(toks) - 1


def is_test_only_cfg(attr):
    """attr = token values inside #[...]. True for `test`, `cfg(test)`,
    and `cfg(all(.., test, ..))` -- predicates that imply a test build."""
    if attr == ["test"]:
        return True
    if attr[:2] != ["cfg", "("]:
        return False
    pred = attr[2:-1]

    def implies_test(p):
        if p == ["test"]:
            return True
        if p[:2] == ["all", "("]:
            inner, depth, cur = p[2:-1], 0, []
            parts = []
            for v in inner:
                if v == "," and depth == 0:
                    parts.append(cur)
                    cur = []
                    continue
                depth += v in OPEN
                depth -= v in CLOSE
                cur.append(v)
            parts.append(cur)
            return any(implies_test(part) for part in parts if part)
        return False

    return implies_test(pred)


def item_end(toks, k):
    """Given toks[k] = first token after a test-only attribute, return the
    index of the item's last token: further outer attributes are skipped,
    then the item runs to a `;` at nesting depth 0 or to the close of the
    first `{` opened at depth 0."""
    n = len(toks)
    while k + 1 < n and toks[k][1] == "#" and toks[k + 1][1] == "[":
        k = matching(toks, k + 1) + 1
    depth = 0
    j = k
    while j < n:
        kind, val, _ = toks[j]
        if kind == "punct":
            if val == "{" and depth == 0:
                return matching(toks, j)
            if val in OPEN:
                depth += 1
            elif val in CLOSE:
                if depth == 0:
                    return j - 1  # enclosing group closed: item ended
                depth -= 1
            elif val == ";" and depth == 0:
                return j
        j += 1
    return n - 1


def analyse(text):
    """Return (sites, test_mod_decls, whole_file_test) for one file.

    sites: line numbers of non-test `.unwrap()` / `.expect(` calls.
    test_mod_decls: names of out-of-line `#[cfg(test)] mod x;` declarations.
    """
    toks = tokenize(text)
    vals = [t[1] for t in toks]
    n = len(toks)
    sites, test_mods = [], []
    k = 0
    while k < n:
        kind, val, line = toks[k]
        if val == "#" and k + 2 < n and vals[k + 1] in ("[", "!"):
            inner_attr = vals[k + 1] == "!"
            open_idx = k + 2 if inner_attr else k + 1
            if open_idx < n and vals[open_idx] == "[":
                close_idx = matching(toks, open_idx)
                attr = vals[open_idx + 1 : close_idx]
                if is_test_only_cfg(attr):
                    if inner_attr:
                        return [], [], True
                    end = item_end(toks, close_idx + 1)
                    item = vals[close_idx + 1 : end + 1]
                    if "mod" in item and item and item[-1] == ";":
                        name = item[item.index("mod") + 1]
                        test_mods.append(name)
                    k = end + 1
                    continue
                k = close_idx + 1
                continue
        if (
            val == "."
            and k + 2 < n
            and toks[k + 1][0] == "ident"
            and vals[k + 2] == "("
            and (
                vals[k + 1] == "expect"
                or (vals[k + 1] == "unwrap" and k + 3 < n and vals[k + 3] == ")")
            )
        ):
            sites.append(line)
        k += 1
    return sites, test_mods, False


def is_test_file(path):
    rel = path.relative_to(root)
    return (
        "tests" in rel.parts[:-1]
        or path.name == "tests.rs"
        or path.name.endswith("_tests.rs")
    )


all_files = sorted(root.glob("crates/*/src/**/*.rs"))
results = {p: analyse(p.read_text(encoding="utf-8")) for p in all_files}

# Out-of-line test modules: `#[cfg(test)] mod x;` in a/b.rs -> a/b/x.rs or
# a/b/x/mod.rs (in lib.rs/main.rs/mod.rs: sibling x.rs or x/mod.rs), and
# everything beneath that module's directory.
test_paths = set()
for path, (_, mods, whole) in results.items():
    if whole:
        test_paths.add(path)
    base = (
        path.parent
        if path.name in ("lib.rs", "main.rs", "mod.rs")
        else path.parent / path.stem
    )
    for name in mods:
        test_paths.add(base / f"{name}.rs")
        test_paths.add(base / name)


def under_test_module(path):
    return any(path == t or t in path.parents for t in test_paths)


files = [p for p in all_files if not is_test_file(p)]
SITES = [
    (str(p.relative_to(root)), ln)
    for p in files
    if not under_test_module(p)
    for ln in results[p][0]
]
unwraps = len(SITES)
largest = (0, "")
for path in files:
    count = len(path.read_text(encoding="utf-8").splitlines())
    if count > largest[0]:
        largest = (count, str(path.relative_to(root)))

if LIST_SITES:
    for rel, ln in SITES:
        print(f"{rel}:{ln}", file=sys.stderr)

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
