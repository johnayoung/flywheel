//! The `flywheel` orchestrator command line, parsed by hand with
//! argparse-identical usage, help, and error text.
//!
//! argparse attributes an error to the parser that detected it: a missing
//! option value, a bad int/float/choice, or a missing positional is reported
//! by the *sub*parser (its own usage line, `flywheel <verb>: error:`), while
//! leftover tokens -- an unknown option or a surplus positional -- bubble up
//! to the *top-level* parser (`flywheel: error: unrecognized arguments:`).
//! Long options accept any unambiguous prefix and the `--flag=value`
//! spelling; a repeated scalar option takes its last value.

use std::fmt;

/// Default `--max-turns` (mirrors `flywheel_core.workflow.DEFAULT_MAX_TURNS`).
pub const DEFAULT_MAX_TURNS: i64 = 500;
/// Default `--max-retries` (mirrors `flywheel_core.workflow.DEFAULT_MAX_RETRIES`).
pub const DEFAULT_MAX_RETRIES: i64 = 1;
/// Default `--lease-seconds`.
pub const DEFAULT_LEASE_SECONDS: f64 = 300.0;
/// Default `--reconcile-seconds`.
pub const DEFAULT_RECONCILE_SECONDS: f64 = 15.0;

/// The verb names in declaration order (the `{...}` choice list).
pub const COMMANDS: [&str; 12] = [
    "init",
    "next",
    "orchestrate",
    "status",
    "live",
    "history",
    "show",
    "archive",
    "validate",
    "recheck-blocked",
    "recover",
    "resolve",
];

/// `--status` choices for `history`.
pub const HISTORY_STATUS_CHOICES: [&str; 3] = ["done", "failed", "failed_validation"];
/// `--store` choices for `init`.
pub const INIT_STORE_BACKENDS: [&str; 2] = ["sqlite", "postgres"];
/// `--source` choices for `init`.
pub const INIT_SOURCE_KINDS: [&str; 2] = ["directory", "github"];

// --- Parsed shapes ----------------------------------------------------------------

/// The three resolution inputs every delegated verb shares.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommonArgs {
    pub tasks_dir: Option<String>,
    pub policy: Option<String>,
    pub db: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitArgs {
    pub store: Option<String>,
    pub pg_schema: Option<String>,
    pub provision: bool,
    pub allow_unverified: bool,
    pub source: Option<String>,
    pub repo: Option<String>,
    pub label: Option<String>,
    pub skills: Option<bool>,
    pub defaults: bool,
    pub agent: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrchestrateArgs {
    pub common: CommonArgs,
    pub sandbox_root: Option<String>,
    pub model: Option<String>,
    pub max_turns: i64,
    pub max_retries: i64,
    pub worker_id: Option<String>,
    pub lease_seconds: f64,
    pub reconcile_seconds: f64,
}

impl Default for OrchestrateArgs {
    fn default() -> Self {
        Self {
            common: CommonArgs::default(),
            sandbox_root: None,
            model: None,
            max_turns: DEFAULT_MAX_TURNS,
            max_retries: DEFAULT_MAX_RETRIES,
            worker_id: None,
            lease_seconds: DEFAULT_LEASE_SECONDS,
            reconcile_seconds: DEFAULT_RECONCILE_SECONDS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusArgs {
    pub common: CommonArgs,
    pub json: bool,
    pub rollup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveArgs {
    pub policy: Option<String>,
    pub db: Option<String>,
    pub watch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistoryArgs {
    pub common: CommonArgs,
    pub status: Vec<String>,
    pub phase: Option<String>,
    pub limit: i64,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShowArgs {
    pub run_or_task_id: String,
    pub common: CommonArgs,
    pub json: bool,
    pub raw: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecheckBlockedArgs {
    pub common: CommonArgs,
    pub run_id: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoverArgs {
    pub policy: Option<String>,
    pub db: Option<String>,
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolveArgs {
    pub task_id: String,
    pub reason: String,
    pub policy: Option<String>,
    pub db: Option<String>,
}

/// One parsed verb invocation.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Init(InitArgs),
    Next(CommonArgs),
    Orchestrate(OrchestrateArgs),
    Status(StatusArgs),
    Live(LiveArgs),
    History(HistoryArgs),
    Show(ShowArgs),
    Archive(CommonArgs),
    Validate(CommonArgs),
    RecheckBlocked(RecheckBlockedArgs),
    Recover(RecoverArgs),
    Resolve(ResolveArgs),
}

impl Command {
    /// The verb token the command was parsed from.
    pub fn verb(&self) -> &'static str {
        match self {
            Command::Init(_) => "init",
            Command::Next(_) => "next",
            Command::Orchestrate(_) => "orchestrate",
            Command::Status(_) => "status",
            Command::Live(_) => "live",
            Command::History(_) => "history",
            Command::Show(_) => "show",
            Command::Archive(_) => "archive",
            Command::Validate(_) => "validate",
            Command::RecheckBlocked(_) => "recheck-blocked",
            Command::Recover(_) => "recover",
            Command::Resolve(_) => "resolve",
        }
    }
}

/// What `parse_args` produced: a command to run, or a help text to print
/// (exit 0).
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Command(Command),
    /// `-h`/`--help`: the full help text to print on stdout.
    Help(String),
}

/// An argparse usage error: the usage block plus the `error:` line, both
/// printed to stderr before exiting 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgError {
    pub usage: String,
    pub prog: String,
    pub message: String,
}

impl ArgError {
    fn top(message: impl Into<String>) -> Self {
        Self {
            usage: TOP_USAGE.to_owned(),
            prog: "flywheel".to_owned(),
            message: message.into(),
        }
    }

    fn sub(verb: &str, message: impl Into<String>) -> Self {
        Self {
            usage: usage_for(verb).to_owned(),
            prog: format!("flywheel {verb}"),
            message: message.into(),
        }
    }

    /// The exact stderr text argparse prints: usage, then `prog: error: msg`.
    pub fn render(&self) -> String {
        format!("{}{}: error: {}\n", self.usage, self.prog, self.message)
    }
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: error: {}", self.prog, self.message)
    }
}

impl std::error::Error for ArgError {}

// --- Option tables ----------------------------------------------------------------

/// How an option consumes the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    /// A `store_true` / `BooleanOptionalAction` flag: no value.
    Flag,
    /// Exactly one value.
    One,
}

#[derive(Debug, Clone, Copy)]
struct OptSpec {
    name: &'static str,
    arity: Arity,
}

const fn flag(name: &'static str) -> OptSpec {
    OptSpec {
        name,
        arity: Arity::Flag,
    }
}

const fn one(name: &'static str) -> OptSpec {
    OptSpec {
        name,
        arity: Arity::One,
    }
}

const COMMON_OPTS: [OptSpec; 3] = [one("--tasks-dir"), one("--policy"), one("--db")];

fn options_for(verb: &str) -> Vec<OptSpec> {
    let mut opts = vec![flag("-h"), flag("--help")];
    match verb {
        "init" => opts.extend([
            one("--store"),
            one("--pg-schema"),
            flag("--provision"),
            flag("--allow-unverified"),
            one("--source"),
            one("--repo"),
            one("--label"),
            flag("--skills"),
            flag("--no-skills"),
            flag("--defaults"),
            flag("--agent"),
        ]),
        "next" | "archive" => opts.extend(COMMON_OPTS),
        "validate" => opts.extend([one("--tasks-dir"), one("--policy")]),
        "orchestrate" => {
            opts.extend(COMMON_OPTS);
            opts.extend([
                one("--sandbox-root"),
                one("--model"),
                one("--max-turns"),
                one("--max-retries"),
                one("--worker-id"),
                one("--lease-seconds"),
                one("--reconcile-seconds"),
            ]);
        }
        "status" => {
            opts.extend(COMMON_OPTS);
            opts.extend([flag("--json"), flag("--rollup")]);
        }
        "live" => opts.extend([one("--policy"), one("--db"), one("--watch")]),
        "history" => {
            opts.extend(COMMON_OPTS);
            opts.extend([
                one("--status"),
                one("--phase"),
                one("--limit"),
                flag("--json"),
            ]);
        }
        "show" => {
            opts.extend(COMMON_OPTS);
            opts.extend([flag("--json"), flag("--raw")]);
        }
        "recheck-blocked" => {
            opts.extend(COMMON_OPTS);
            opts.extend([one("--run-id"), flag("--dry-run")]);
        }
        "recover" => opts.extend([one("--policy"), one("--db"), one("--task-id")]),
        "resolve" => opts.extend([one("--reason"), one("--policy"), one("--db")]),
        _ => {}
    }
    opts
}

/// A raw option occurrence after prefix resolution and `=` splitting.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Opt {
        name: &'static str,
        value: Option<String>,
    },
    Positional(String),
    /// An unknown option, bubbled to the top-level parser as an extra.
    Unknown(String),
}

/// Whether a token looks like a negative number (argparse lets those stand
/// as option values because no option here looks like one).
fn is_negative_number(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };
    let int_ok = int_part.chars().all(|c| c.is_ascii_digit());
    match frac_part {
        None => int_ok && !int_part.is_empty(),
        Some(f) => {
            int_ok
                && f.chars().all(|c| c.is_ascii_digit())
                && !(int_part.is_empty() && f.is_empty())
        }
    }
}

/// Whether a token is an option flag rather than a value/positional.
fn looks_like_option(token: &str) -> bool {
    token.starts_with('-') && token.len() > 1 && !is_negative_number(token)
}

/// Resolve `token` (already known to start with `-`) against `opts`:
/// exact match, else a unique long-option prefix.
fn resolve_option(verb: &str, token: &str, opts: &[OptSpec]) -> Result<Option<OptSpec>, ArgError> {
    if let Some(spec) = opts.iter().find(|o| o.name == token) {
        return Ok(Some(*spec));
    }
    if !token.starts_with("--") {
        return Ok(None);
    }
    let matches: Vec<OptSpec> = opts
        .iter()
        .filter(|o| o.name.starts_with("--") && o.name.starts_with(token))
        .copied()
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => {
            let names: Vec<&str> = matches.iter().map(|o| o.name).collect();
            Err(ArgError::sub(
                verb,
                format!("ambiguous option: {token} could match {}", names.join(", ")),
            ))
        }
    }
}

/// Tokenize the subcommand's argv against its option table.
fn tokenize(verb: &str, argv: &[String]) -> Result<Vec<Token>, ArgError> {
    let opts = options_for(verb);
    let mut tokens = Vec::new();
    let mut iter = argv.iter().peekable();
    let mut only_positionals = false;
    while let Some(arg) = iter.next() {
        if only_positionals || !looks_like_option(arg) {
            tokens.push(Token::Positional(arg.clone()));
            continue;
        }
        if arg == "--" {
            only_positionals = true;
            continue;
        }
        let (flag_text, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (arg.as_str(), None),
        };
        let Some(spec) = resolve_option(verb, flag_text, &opts)? else {
            tokens.push(Token::Unknown(arg.clone()));
            continue;
        };
        match spec.arity {
            Arity::Flag => {
                if let Some(value) = inline {
                    return Err(ArgError::sub(
                        verb,
                        format!(
                            "argument {}: ignored explicit argument '{value}'",
                            spec.name
                        ),
                    ));
                }
                tokens.push(Token::Opt {
                    name: spec.name,
                    value: None,
                });
            }
            Arity::One => {
                let value = match inline {
                    Some(value) => value,
                    None => match iter.peek() {
                        Some(next) if !looks_like_option(next) => {
                            let v = (*next).clone();
                            iter.next();
                            v
                        }
                        _ => {
                            return Err(ArgError::sub(
                                verb,
                                format!("argument {}: expected one argument", spec.name),
                            ));
                        }
                    },
                };
                tokens.push(Token::Opt {
                    name: spec.name,
                    value: Some(value),
                });
            }
        }
    }
    Ok(tokens)
}

fn parse_int(verb: &str, name: &str, raw: &str) -> Result<i64, ArgError> {
    let text = raw.trim();
    let text = text.strip_prefix('+').unwrap_or(text);
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    if text.is_empty() || text.starts_with('_') || text.ends_with('_') || text.contains("__") {
        return Err(ArgError::sub(
            verb,
            format!("argument {name}: invalid int value: '{raw}'"),
        ));
    }
    cleaned
        .parse::<i64>()
        .map_err(|_| ArgError::sub(verb, format!("argument {name}: invalid int value: '{raw}'")))
}

fn parse_float(verb: &str, name: &str, raw: &str) -> Result<f64, ArgError> {
    let text = raw.trim();
    let cleaned: String = text.chars().filter(|c| *c != '_').collect();
    if text.is_empty() {
        return Err(ArgError::sub(
            verb,
            format!("argument {name}: invalid float value: '{raw}'"),
        ));
    }
    cleaned.parse::<f64>().map_err(|_| {
        ArgError::sub(
            verb,
            format!("argument {name}: invalid float value: '{raw}'"),
        )
    })
}

fn check_choice(verb: &str, name: &str, value: &str, choices: &[&str]) -> Result<(), ArgError> {
    if choices.contains(&value) {
        return Ok(());
    }
    Err(ArgError::sub(
        verb,
        format!(
            "argument {name}: invalid choice: '{value}' (choose from {})",
            choices.join(", ")
        ),
    ))
}

/// The per-verb state the token walk fills in.
#[derive(Default)]
struct Collected {
    common: CommonArgs,
    init: InitArgs,
    orchestrate: OrchestrateArgs,
    status_json: bool,
    rollup: bool,
    watch: i64,
    history_status: Vec<String>,
    phase: Option<String>,
    limit: i64,
    raw: bool,
    run_id: Option<String>,
    dry_run: bool,
    task_id: Option<String>,
    reason: Option<String>,
    positionals: Vec<String>,
    extras: Vec<String>,
    help: bool,
}

fn collect(verb: &str, tokens: Vec<Token>) -> Result<Collected, ArgError> {
    let mut c = Collected::default();
    for token in tokens {
        match token {
            Token::Positional(p) => c.positionals.push(p),
            Token::Unknown(u) => c.extras.push(u),
            Token::Opt { name, value } => {
                let v = value.unwrap_or_default();
                match name {
                    "-h" | "--help" => c.help = true,
                    "--tasks-dir" => c.common.tasks_dir = Some(v),
                    "--policy" => c.common.policy = Some(v),
                    "--db" => c.common.db = Some(v),
                    "--store" => {
                        check_choice(verb, name, &v, &INIT_STORE_BACKENDS)?;
                        c.init.store = Some(v);
                    }
                    "--pg-schema" => c.init.pg_schema = Some(v),
                    "--provision" => c.init.provision = true,
                    "--allow-unverified" => c.init.allow_unverified = true,
                    "--source" => {
                        check_choice(verb, name, &v, &INIT_SOURCE_KINDS)?;
                        c.init.source = Some(v);
                    }
                    "--repo" => c.init.repo = Some(v),
                    "--label" => c.init.label = Some(v),
                    "--skills" => c.init.skills = Some(true),
                    "--no-skills" => c.init.skills = Some(false),
                    "--defaults" => c.init.defaults = true,
                    "--agent" => c.init.agent = true,
                    "--sandbox-root" => c.orchestrate.sandbox_root = Some(v),
                    "--model" => c.orchestrate.model = Some(v),
                    "--max-turns" => c.orchestrate.max_turns = parse_int(verb, name, &v)?,
                    "--max-retries" => c.orchestrate.max_retries = parse_int(verb, name, &v)?,
                    "--worker-id" => c.orchestrate.worker_id = Some(v),
                    "--lease-seconds" => c.orchestrate.lease_seconds = parse_float(verb, name, &v)?,
                    "--reconcile-seconds" => {
                        c.orchestrate.reconcile_seconds = parse_float(verb, name, &v)?;
                    }
                    "--json" => c.status_json = true,
                    "--rollup" => c.rollup = true,
                    "--watch" => c.watch = parse_int(verb, name, &v)?,
                    "--status" => {
                        check_choice(verb, name, &v, &HISTORY_STATUS_CHOICES)?;
                        c.history_status.push(v);
                    }
                    "--phase" => c.phase = Some(v),
                    "--limit" => c.limit = parse_int(verb, name, &v)?,
                    "--raw" => c.raw = true,
                    "--run-id" => c.run_id = Some(v),
                    "--dry-run" => c.dry_run = true,
                    "--task-id" => c.task_id = Some(v),
                    "--reason" => c.reason = Some(v),
                    _ => {}
                }
            }
        }
    }
    Ok(c)
}

/// Parse the full orchestrator argv (verb first).
pub fn parse_args(argv: &[String]) -> Result<Parsed, ArgError> {
    // Top level: the first token that is not an option is the command.
    let mut extras: Vec<String> = Vec::new();
    let mut command_index: Option<usize> = None;
    for (i, arg) in argv.iter().enumerate() {
        if arg == "-h" || arg == "--help" {
            return Ok(Parsed::Help(TOP_HELP.to_owned()));
        }
        if looks_like_option(arg) {
            extras.push(arg.clone());
            continue;
        }
        command_index = Some(i);
        break;
    }
    let Some(index) = command_index else {
        return Err(ArgError::top(
            "the following arguments are required: command",
        ));
    };
    let verb = argv[index].as_str();
    if !COMMANDS.contains(&verb) {
        return Err(ArgError::top(format!(
            "argument command: invalid choice: '{verb}' (choose from {})",
            COMMANDS.join(", ")
        )));
    }
    let rest = &argv[index + 1..];
    let tokens = tokenize(verb, rest)?;
    let c = collect(verb, tokens)?;
    if c.help {
        return Ok(Parsed::Help(help_for(verb).to_owned()));
    }
    let mut positionals = c.positionals.clone().into_iter();
    let command = match verb {
        "init" => Command::Init(c.init.clone()),
        "next" => Command::Next(c.common.clone()),
        "orchestrate" => {
            let mut args = c.orchestrate.clone();
            args.common = c.common.clone();
            Command::Orchestrate(args)
        }
        "status" => Command::Status(StatusArgs {
            common: c.common.clone(),
            json: c.status_json,
            rollup: c.rollup,
        }),
        "live" => Command::Live(LiveArgs {
            policy: c.common.policy.clone(),
            db: c.common.db.clone(),
            watch: c.watch,
        }),
        "history" => Command::History(HistoryArgs {
            common: c.common.clone(),
            status: c.history_status.clone(),
            phase: c.phase.clone(),
            limit: c.limit,
            json: c.status_json,
        }),
        "show" => {
            let Some(run_or_task_id) = positionals.next() else {
                return Err(ArgError::sub(
                    verb,
                    "the following arguments are required: run_or_task_id",
                ));
            };
            Command::Show(ShowArgs {
                run_or_task_id,
                common: c.common.clone(),
                json: c.status_json,
                raw: c.raw,
            })
        }
        "archive" => Command::Archive(c.common.clone()),
        "validate" => Command::Validate(c.common.clone()),
        "recheck-blocked" => Command::RecheckBlocked(RecheckBlockedArgs {
            common: c.common.clone(),
            run_id: c.run_id.clone(),
            dry_run: c.dry_run,
        }),
        "recover" => Command::Recover(RecoverArgs {
            policy: c.common.policy.clone(),
            db: c.common.db.clone(),
            task_id: c.task_id.clone(),
        }),
        "resolve" => {
            let task_id = positionals.next();
            let mut missing: Vec<&str> = Vec::new();
            if task_id.is_none() {
                missing.push("task_id");
            }
            if c.reason.is_none() {
                missing.push("--reason");
            }
            if !missing.is_empty() {
                return Err(ArgError::sub(
                    verb,
                    format!(
                        "the following arguments are required: {}",
                        missing.join(", ")
                    ),
                ));
            }
            Command::Resolve(ResolveArgs {
                task_id: task_id.unwrap_or_default(),
                reason: c.reason.clone().unwrap_or_default(),
                policy: c.common.policy.clone(),
                db: c.common.db.clone(),
            })
        }
        _ => unreachable!("verb membership checked above"),
    };
    // Surplus positionals and unknown options bubble to the top-level
    // parser, in command-line order: argparse reports the extras it saw
    // before the command first.
    let mut leftovers = extras;
    leftovers.extend(c.extras.iter().cloned());
    leftovers.extend(positionals);
    if !leftovers.is_empty() {
        return Err(ArgError::top(format!(
            "unrecognized arguments: {}",
            leftovers.join(" ")
        )));
    }
    Ok(Parsed::Command(command))
}

// --- Usage and help text ----------------------------------------------------------

pub const TOP_USAGE: &str = "usage: flywheel [-h]
                {init,next,orchestrate,status,live,history,show,archive,validate,recheck-blocked,recover,resolve} ...
";

pub const TOP_HELP: &str = "usage: flywheel [-h]
                {init,next,orchestrate,status,live,history,show,archive,validate,recheck-blocked,recover,resolve} ...

Schedule and drive many flywheel tasks laid out under
.flywheel/tasks/active/<phase>/.

positional arguments:
  {init,next,orchestrate,status,live,history,show,archive,validate,recheck-blocked,recover,resolve}
    init                Scaffold .flywheel/ (tasks dir, gitignored runtime
                        state) and a flywheel.toml work policy in the current
                        directory. Prompts for store backend and work source
                        on a TTY; flags pre-answer prompts; a non-TTY stdin
                        takes every default. With an existing flywheel.toml,
                        an interactive run offers to reconfigure it (rewriting
                        only the answered keys); a non-interactive run never
                        touches it.
    next                Print path to the next eligible task and exit 0; exit
                        1 if nothing is eligible.
    orchestrate         Drive every eligible task to quiescence: honor
                        prerequisites, reactively unblock and resume blocked
                        lifecycles, one in-process worker. Exit 0 only if
                        every task it ran reached done.
    status              Print the state of every active task.
    live                Print one line per in-flight run showing its latest
                        agent message or harness event (with age) so a watcher
                        can tell at a glance whether progress is still being
                        made.
    history             List finished runs (done / failed /
                        failed_validation), one line per task, most recently
                        finished first. Retried tasks fold into one line
                        (runs=N); 'show' drills into a run.
    show                Show one run in full: lifecycle, attempts, grader
                        receipts, final agent output, and the task's other
                        runs. Accepts a run_id or a task id (resolves to its
                        latest run).
    archive             Move active phase directories whose tasks are all done
                        into archive/.
    validate            Statically validate every active task's command
                        graders (shell-parse + repo-relative path existence)
                        without running them; exit non-zero naming each
                        invalid task.
    recheck-blocked     Re-evaluate blocked lifecycles' persisted requires
                        and, when all predicates are satisfied, transition
                        interrupted -> ready. Default scans every blocked
                        lifecycle; --run-id targets one; --dry-run reports
                        without transitioning.
    recover             Finalize lifecycles stuck in running/validating
                        (prints run_ids transitioned to interrupted).
    resolve             Deliberately abandon a stranded task: record an
                        operator-attributed resolution with a reason so the
                        next archive sweep archives the otherwise-landed
                        phase. The only non-probe path to clearing a strand.

options:
  -h, --help            show this help message and exit
";

const INIT_USAGE: &str = "usage: flywheel init [-h] [--store {sqlite,postgres}] [--pg-schema NAME]
                     [--provision] [--allow-unverified]
                     [--source {directory,github}] [--repo OWNER/NAME]
                     [--label LABEL] [--skills | --no-skills] [--defaults]
                     [--agent]
";

const INIT_OPTIONS: &str = "
options:
  -h, --help            show this help message and exit
  --store {sqlite,postgres}
                        Store backend recorded as [store] backend (pre-answers
                        the prompt; default: sqlite).
  --pg-schema NAME      Postgres schema name recorded as [store] schema
                        (postgres backend only).
  --provision           After the postgres preflight passes, create the schema
                        and run the store's bootstrap DDL now (postgres
                        backend with a resolvable DSN only), so tables exist
                        before the first worker run instead of being created
                        lazily by it.
  --allow-unverified    Scaffold the policy even when the postgres preflight
                        reports a blocking issue (unreachable DSN,
                        transaction-mode pooler, missing privileges,
                        incompatible schema version). The report still prints;
                        this only downgrades the blocks to warnings.
  --source {directory,github}
                        Work source kind recorded as [source] kind (pre-
                        answers the prompt; default: directory).
  --repo OWNER/NAME     GitHub repo for the github work source (default:
                        parsed from the origin remote).
  --label LABEL         Issue label for the github work source (default:
                        flywheel).
  --skills, --no-skills
                        Install (or regenerate) the Claude Code skills (fw-
                        spec, fw-plan, fw-verify, fw-retro, fw-improve,
                        flywheel-ops) under .claude/skills/ (pre-answers the
                        prompt; interactive default: yes, non-interactive
                        default: no). With an existing flywheel.toml, a non-
                        interactive 'flywheel init --skills' regenerates the
                        skills against the current policy without touching the
                        policy file.
  --defaults            Accept every default without prompting (a non-TTY
                        stdin implies this).
  --agent               Drive an agent to inspect the repo and PROPOSE the
                        flywheel.toml policy (detected toolchain -> default
                        graders, sandbox setup, autopilot cadence, .gitignore
                        additions) instead of prompting. The proposal is
                        printed before the policy is written and is re-
                        validated; an unparseable response writes nothing.
";

const COMMON_OPTIONS_WIDE: &str = "  --tasks-dir TASKS_DIR
                        Tasks root directory (default: .flywheel/tasks).
  --policy POLICY       Work-policy file selecting the work source (default:
                        flywheel.toml if present). An explicit --tasks-dir
                        overrides the policy.
  --db DB               SQLite database path (default:
                        .flywheel/flywheel.sqlite).
";

const NEXT_USAGE: &str =
    "usage: flywheel next [-h] [--tasks-dir TASKS_DIR] [--policy POLICY] [--db DB]\n";

const ORCHESTRATE_USAGE: &str =
    "usage: flywheel orchestrate [-h] [--tasks-dir TASKS_DIR] [--policy POLICY]
                            [--db DB] [--sandbox-root SANDBOX_ROOT]
                            [--model MODEL] [--max-turns MAX_TURNS]
                            [--max-retries MAX_RETRIES]
                            [--worker-id WORKER_ID]
                            [--lease-seconds LEASE_SECONDS]
                            [--reconcile-seconds RECONCILE_SECONDS]
";

const ORCHESTRATE_OPTIONS: &str = "  --sandbox-root SANDBOX_ROOT
                        Root under which each task runs in <sandbox-
                        root>/<task-id> (default: .flywheel/worktrees).
                        Relative paths anchor at the repo root; @cache and
                        @sibling select out-of-tree layouts.
  --model MODEL         Override the Claude model passed to the SDK.
  --max-turns MAX_TURNS
                        Max agent turns per iteration (default: 500).
  --max-retries MAX_RETRIES
                        Harness retry budget after failed_validation (default:
                        1).
  --worker-id WORKER_ID
                        Stable id for this worker (default: a random per-
                        process id). Used for the per-task claim lease that
                        keeps concurrent workers from double-running a task.
  --lease-seconds LEASE_SECONDS
                        Task-claim lease window in seconds (default: 300). A
                        live lease is renewed by a heartbeat while the task
                        runs; a crashed worker's lease lapses after this
                        window so the task is reclaimable. Expiry uses each
                        host's wall clock, so set this well above (max cross-
                        host clock skew + max heartbeat gap) or a fast-clocked
                        worker may steal a live peer's lease.
  --reconcile-seconds RECONCILE_SECONDS
                        Steering bridge: re-list the work source every N
                        seconds and enqueue an interrupt for any in-flight run
                        whose item is no longer listed (closed issue, pulled
                        label, deleted task file). 0 disables (default: 15).
";

const STATUS_USAGE: &str = "usage: flywheel status [-h] [--tasks-dir TASKS_DIR] [--policy POLICY]
                       [--db DB] [--json] [--rollup]
";

const STATUS_OPTIONS: &str =
    "  --json                Emit machine-readable JSON instead of a text table.
  --rollup              Render a phase-grouped, evidence-derived rollup: each
                        task's status is computed from grader receipts
                        (verified vs accepted vs blocked/failed/not-started),
                        never self-reported.
";

const LIVE_USAGE: &str =
    "usage: flywheel live [-h] [--policy POLICY] [--db DB] [--watch SECONDS]\n";

const LIVE_OPTIONS: &str = "
options:
  -h, --help       show this help message and exit
  --policy POLICY  Work-policy file selecting the work source (default:
                   flywheel.toml if present). An explicit --tasks-dir
                   overrides the policy.
  --db DB          SQLite database path (default: .flywheel/flywheel.sqlite).
  --watch SECONDS  Refresh continuously every SECONDS; clears the screen on
                   each tick. Default 0 prints one snapshot and exits.
";

const HISTORY_USAGE: &str = "usage: flywheel history [-h] [--tasks-dir TASKS_DIR] [--policy POLICY]
                        [--db DB] [--status {done,failed,failed_validation}]
                        [--phase PHASE] [--limit N] [--json]
";

const HISTORY_OPTIONS: &str = "  --status {done,failed,failed_validation}
                        Only list runs whose terminal status matches
                        (repeatable; default: all terminal statuses).
  --phase PHASE         Only list tasks attributed to this phase directory
                        name.
  --limit N             Cap the number of task rows printed (default: 0 =
                        all).
  --json                Emit machine-readable JSON instead of a text table.
";

const SHOW_USAGE: &str =
    "usage: flywheel show [-h] [--tasks-dir TASKS_DIR] [--policy POLICY] [--db DB]
                     [--json] [--raw]
                     run_or_task_id
";

const SHOW_POSITIONALS: &str = "
positional arguments:
  run_or_task_id        Run id, or a task id (its most recent run is shown).
";

const SHOW_OPTIONS: &str =
    "  --json                Emit machine-readable JSON instead of the text report.
  --raw                 Disable redaction of decision-record output excerpts
                        and emit them verbatim as stored. On by default,
                        decision output passes through a best-effort secret-
                        redaction policy; use --raw only for authorized
                        forensics on trusted output sinks.
";

const ARCHIVE_USAGE: &str = "usage: flywheel archive [-h] [--tasks-dir TASKS_DIR] [--policy POLICY]
                        [--db DB]
";

const VALIDATE_USAGE: &str =
    "usage: flywheel validate [-h] [--tasks-dir TASKS_DIR] [--policy POLICY]\n";

const VALIDATE_OPTIONS: &str = "
options:
  -h, --help            show this help message and exit
  --tasks-dir TASKS_DIR
                        Tasks root directory (default: .flywheel/tasks).
  --policy POLICY       Work-policy file selecting the work source (default:
                        flywheel.toml if present). An explicit --tasks-dir
                        overrides the policy.
";

const RECHECK_USAGE: &str =
    "usage: flywheel recheck-blocked [-h] [--tasks-dir TASKS_DIR] [--policy POLICY]
                                [--db DB] [--run-id RUN_ID] [--dry-run]
";

const RECHECK_OPTIONS: &str =
    "  --run-id RUN_ID       Target a single lifecycle by run_id; skips the default
                        scan. Prints a clear message and exits 0 when the run
                        is missing or not blocked.
  --dry-run             Evaluate predicates and report without applying the
                        interrupted -> ready transition. Emits
                        harness.recheck_attempted for the audit trail; does
                        not emit harness.unblocked.
";

const RECOVER_USAGE: &str =
    "usage: flywheel recover [-h] [--policy POLICY] [--db DB] [--task-id TASK_ID]\n";

const RECOVER_OPTIONS: &str = "
options:
  -h, --help         show this help message and exit
  --policy POLICY    Work-policy file selecting the work source (default:
                     flywheel.toml if present). An explicit --tasks-dir
                     overrides the policy.
  --db DB            SQLite database path (default:
                     .flywheel/flywheel.sqlite).
  --task-id TASK_ID  Only recover stranded lifecycles for this task id. Omit
                     to recover every stranded lifecycle in the store.
";

const RESOLVE_USAGE: &str =
    "usage: flywheel resolve [-h] --reason REASON [--policy POLICY] [--db DB]
                        task_id
";

const RESOLVE_BODY: &str = "
positional arguments:
  task_id          Task id of the strand to abandon (the stop-event subject).
                   One subject per invocation.

options:
  -h, --help       show this help message and exit
  --reason REASON  Why the strand is being abandoned. Recorded verbatim on the
                   resolution marker for the audit trail.
  --policy POLICY  Work-policy file selecting the work source (default:
                   flywheel.toml if present). An explicit --tasks-dir
                   overrides the policy.
  --db DB          SQLite database path (default: .flywheel/flywheel.sqlite).
";

const OPTIONS_HEAD_WIDE: &str = "
options:
  -h, --help            show this help message and exit
";

/// The usage block (with trailing newline) argparse prints for `verb`.
pub fn usage_for(verb: &str) -> &'static str {
    match verb {
        "init" => INIT_USAGE,
        "next" => NEXT_USAGE,
        "orchestrate" => ORCHESTRATE_USAGE,
        "status" => STATUS_USAGE,
        "live" => LIVE_USAGE,
        "history" => HISTORY_USAGE,
        "show" => SHOW_USAGE,
        "archive" => ARCHIVE_USAGE,
        "validate" => VALIDATE_USAGE,
        "recheck-blocked" => RECHECK_USAGE,
        "recover" => RECOVER_USAGE,
        "resolve" => RESOLVE_USAGE,
        _ => TOP_USAGE,
    }
}

/// The full `--help` text (with trailing newline) for `verb`.
pub fn help_for(verb: &str) -> String {
    match verb {
        "init" => format!("{INIT_USAGE}{INIT_OPTIONS}"),
        "next" => format!("{NEXT_USAGE}{OPTIONS_HEAD_WIDE}{COMMON_OPTIONS_WIDE}"),
        "orchestrate" => format!(
            "{ORCHESTRATE_USAGE}{OPTIONS_HEAD_WIDE}{COMMON_OPTIONS_WIDE}{ORCHESTRATE_OPTIONS}"
        ),
        "status" => {
            format!("{STATUS_USAGE}{OPTIONS_HEAD_WIDE}{COMMON_OPTIONS_WIDE}{STATUS_OPTIONS}")
        }
        "live" => format!("{LIVE_USAGE}{LIVE_OPTIONS}"),
        "history" => {
            format!("{HISTORY_USAGE}{OPTIONS_HEAD_WIDE}{COMMON_OPTIONS_WIDE}{HISTORY_OPTIONS}")
        }
        "show" => format!(
            "{SHOW_USAGE}{SHOW_POSITIONALS}{OPTIONS_HEAD_WIDE}{COMMON_OPTIONS_WIDE}{SHOW_OPTIONS}"
        ),
        "archive" => format!("{ARCHIVE_USAGE}{OPTIONS_HEAD_WIDE}{COMMON_OPTIONS_WIDE}"),
        "validate" => format!("{VALIDATE_USAGE}{VALIDATE_OPTIONS}"),
        "recheck-blocked" => {
            format!("{RECHECK_USAGE}{OPTIONS_HEAD_WIDE}{COMMON_OPTIONS_WIDE}{RECHECK_OPTIONS}")
        }
        "recover" => format!("{RECOVER_USAGE}{RECOVER_OPTIONS}"),
        "resolve" => format!("{RESOLVE_USAGE}{RESOLVE_BODY}"),
        _ => TOP_HELP.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_owned()).collect()
    }

    fn err(parts: &[&str]) -> String {
        match parse_args(&argv(parts)) {
            Err(e) => e.render(),
            Ok(other) => panic!("expected an error, got {other:?}"),
        }
    }

    fn cmd(parts: &[&str]) -> Command {
        match parse_args(&argv(parts)) {
            Ok(Parsed::Command(c)) => c,
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn missing_command_is_a_top_level_error() {
        assert_eq!(
            err(&[]),
            format!("{TOP_USAGE}flywheel: error: the following arguments are required: command\n")
        );
    }

    #[test]
    fn unknown_command_lists_the_choices() {
        assert_eq!(
            err(&["bogus"]),
            format!(
                "{TOP_USAGE}flywheel: error: argument command: invalid choice: 'bogus' (choose from init, next, orchestrate, status, live, history, show, archive, validate, recheck-blocked, recover, resolve)\n"
            )
        );
    }

    #[test]
    fn unknown_option_and_surplus_positional_bubble_to_the_top_parser() {
        assert_eq!(
            err(&["status", "--bogus"]),
            format!("{TOP_USAGE}flywheel: error: unrecognized arguments: --bogus\n")
        );
        assert_eq!(
            err(&["status", "extra"]),
            format!("{TOP_USAGE}flywheel: error: unrecognized arguments: extra\n")
        );
        assert_eq!(
            err(&["show", "a", "b"]),
            format!("{TOP_USAGE}flywheel: error: unrecognized arguments: b\n")
        );
    }

    #[test]
    fn value_errors_are_reported_by_the_subparser() {
        assert_eq!(
            err(&["history", "--limit", "x"]),
            format!(
                "{HISTORY_USAGE}flywheel history: error: argument --limit: invalid int value: 'x'\n"
            )
        );
        assert_eq!(
            err(&["history", "--status", "x"]),
            format!(
                "{HISTORY_USAGE}flywheel history: error: argument --status: invalid choice: 'x' (choose from done, failed, failed_validation)\n"
            )
        );
        assert_eq!(
            err(&["init", "--store", "mysql"]),
            format!(
                "{INIT_USAGE}flywheel init: error: argument --store: invalid choice: 'mysql' (choose from sqlite, postgres)\n"
            )
        );
        assert_eq!(
            err(&["live", "--watch"]),
            format!("{LIVE_USAGE}flywheel live: error: argument --watch: expected one argument\n")
        );
        assert_eq!(
            err(&["live", "--watch", "x"]),
            format!("{LIVE_USAGE}flywheel live: error: argument --watch: invalid int value: 'x'\n")
        );
        assert_eq!(
            err(&["orchestrate", "--lease-seconds", "x"]),
            format!(
                "{ORCHESTRATE_USAGE}flywheel orchestrate: error: argument --lease-seconds: invalid float value: 'x'\n"
            )
        );
        assert_eq!(
            err(&["orchestrate", "--max-turns", "1.5"]),
            format!(
                "{ORCHESTRATE_USAGE}flywheel orchestrate: error: argument --max-turns: invalid int value: '1.5'\n"
            )
        );
        assert_eq!(
            err(&["init", "--skills", "--no-skills", "--store"]),
            format!("{INIT_USAGE}flywheel init: error: argument --store: expected one argument\n")
        );
        assert_eq!(
            err(&["status", "--json=1"]),
            format!(
                "{STATUS_USAGE}flywheel status: error: argument --json: ignored explicit argument '1'\n"
            )
        );
        assert_eq!(
            err(&["init", "--defaults=1"]),
            format!(
                "{INIT_USAGE}flywheel init: error: argument --defaults: ignored explicit argument '1'\n"
            )
        );
    }

    #[test]
    fn required_arguments_are_listed_in_declaration_order() {
        assert_eq!(
            err(&["show"]),
            format!(
                "{SHOW_USAGE}flywheel show: error: the following arguments are required: run_or_task_id\n"
            )
        );
        assert_eq!(
            err(&["resolve", "t"]),
            format!(
                "{RESOLVE_USAGE}flywheel resolve: error: the following arguments are required: --reason\n"
            )
        );
        assert_eq!(
            err(&["resolve"]),
            format!(
                "{RESOLVE_USAGE}flywheel resolve: error: the following arguments are required: task_id, --reason\n"
            )
        );
        assert_eq!(
            err(&["resolve", "t", "--reason"]),
            format!(
                "{RESOLVE_USAGE}flywheel resolve: error: argument --reason: expected one argument\n"
            )
        );
    }

    #[test]
    fn help_flags_render_the_matching_help_text() {
        assert_eq!(
            parse_args(&argv(&["-h"])),
            Ok(Parsed::Help(TOP_HELP.to_owned()))
        );
        assert_eq!(
            parse_args(&argv(&["status", "-h"])),
            Ok(Parsed::Help(help_for("status")))
        );
        let status_help = help_for("status");
        assert!(status_help.ends_with("never self-reported.\n"));
        assert!(status_help.contains("\noptions:\n  -h, --help            show this help message and exit\n  --tasks-dir TASKS_DIR\n"));
        assert!(help_for("resolve").contains("positional arguments:\n  task_id"));
    }

    #[test]
    fn parses_every_verb_with_repeats_prefixes_and_inline_values() {
        assert_eq!(
            cmd(&[
                "history", "--status", "done", "--status", "failed", "--limit", "3", "--json"
            ]),
            Command::History(HistoryArgs {
                common: CommonArgs::default(),
                status: vec!["done".into(), "failed".into()],
                phase: None,
                limit: 3,
                json: true,
            })
        );
        assert_eq!(
            cmd(&["status", "--tasks=x", "--db", "y", "--roll"]),
            Command::Status(StatusArgs {
                common: CommonArgs {
                    tasks_dir: Some("x".into()),
                    policy: None,
                    db: Some("y".into()),
                },
                json: false,
                rollup: true,
            })
        );
        assert_eq!(
            cmd(&[
                "init",
                "--skills",
                "--no-skills",
                "--store",
                "postgres",
                "--pg-schema",
                "s"
            ]),
            Command::Init(InitArgs {
                store: Some("postgres".into()),
                pg_schema: Some("s".into()),
                skills: Some(false),
                ..InitArgs::default()
            })
        );
        assert_eq!(
            cmd(&["resolve", "--reason", "why", "t1", "--db", "d"]),
            Command::Resolve(ResolveArgs {
                task_id: "t1".into(),
                reason: "why".into(),
                policy: None,
                db: Some("d".into()),
            })
        );
        assert_eq!(
            cmd(&[
                "orchestrate",
                "--max-turns",
                "+7",
                "--lease-seconds",
                "1e2",
                "--worker-id",
                "w"
            ]),
            Command::Orchestrate(OrchestrateArgs {
                max_turns: 7,
                lease_seconds: 100.0,
                worker_id: Some("w".into()),
                ..OrchestrateArgs::default()
            })
        );
        assert_eq!(
            cmd(&["live", "--watch", "5"]),
            Command::Live(LiveArgs {
                policy: None,
                db: None,
                watch: 5,
            })
        );
        assert_eq!(
            cmd(&["recheck-blocked", "--run-id", "r", "--dry-run"]),
            Command::RecheckBlocked(RecheckBlockedArgs {
                common: CommonArgs::default(),
                run_id: Some("r".into()),
                dry_run: true,
            })
        );
        assert_eq!(
            cmd(&["recover", "--task-id", "t"]),
            Command::Recover(RecoverArgs {
                policy: None,
                db: None,
                task_id: Some("t".into()),
            })
        );
        assert!(matches!(cmd(&["next"]), Command::Next(_)));
        assert!(matches!(cmd(&["archive"]), Command::Archive(_)));
        assert!(matches!(cmd(&["validate"]), Command::Validate(_)));
        assert_eq!(
            cmd(&["show", "run-1", "--raw"]),
            Command::Show(ShowArgs {
                run_or_task_id: "run-1".into(),
                common: CommonArgs::default(),
                json: false,
                raw: true,
            })
        );
    }

    #[test]
    fn ambiguous_prefix_is_an_error_and_negative_numbers_are_values() {
        let rendered = err(&["history", "--p", "x"]);
        assert!(rendered.contains("ambiguous option: --p could match --policy, --phase"));
        assert_eq!(
            cmd(&["history", "--limit", "-1"]),
            Command::History(HistoryArgs {
                limit: -1,
                ..HistoryArgs::default()
            })
        );
    }
}
