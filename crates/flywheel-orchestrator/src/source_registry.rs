//! The registry of work-source backends.
//!
//! Names the work-source kinds an operator can select in `[source] kind`
//! and binds each to a factory that builds the adapter from a
//! [`SourceBuildContext`] (the parsed `[source]` table plus the operator's
//! default graders and the optional stop/log sinks). The registry depends
//! only on the [`WorkSource`] seam, so adapters register themselves without
//! this module importing them: [`default_registry`] registers the always
//! available `directory` kind, and each GitHub module adds its kind through
//! [`SourceRegistry::register`] with the same [`SourceFactory`] signature.
//!
//! Authoritative spec: `docs/work-sources.md` (Registry and extensibility).

use crate::sources::{DirectoryWorkSource, LogSink, StopEventSink, WorkSource, WorkSourceError};
use flywheel_core::task::Grader;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// The singular family noun used in registry error messages.
pub const SOURCE_FAMILY: &str = "work source";

/// `[source] kind` of the directory adapter (the default kind).
pub const DIRECTORY_KIND: &str = "directory";
/// `[source] kind` of the labeled-GitHub-issues adapter.
pub const GITHUB_KIND: &str = "github";
/// `[source] kind` of the failed-GitHub-CI-runs adapter.
pub const GITHUB_CI_KIND: &str = "github_ci";
/// `[source] kind` of the unresolved-PR-review-threads adapter.
pub const GITHUB_REVIEW_KIND: &str = "github_review";

/// Every kind flywheel ships, in registry order. A policy loader can
/// validate `[source] kind` against this list before any adapter is built.
pub const BUILTIN_SOURCE_KINDS: [&str; 4] = [
    DIRECTORY_KIND,
    GITHUB_KIND,
    GITHUB_CI_KIND,
    GITHUB_REVIEW_KIND,
];

/// A `[source] tasks_dir` setting when the operator gives none.
pub const DEFAULT_TASKS_DIR: &str = ".flywheel/tasks";

/// Everything a source factory may read to construct its adapter.
///
/// `settings` is the parsed `[source]` table of `flywheel.toml` (including
/// `kind`); each factory reads the keys its adapter needs (`tasks_dir`,
/// `repo`, `label`, ...) and fails with [`SourceRegistryError::Config`]
/// when a required one is missing or mistyped. `default_graders` is the
/// operator's `[[defaults.graders]]` fallback for tracker kinds; the
/// directory kind ignores it because directory tasks always carry their
/// own graders. `stop_sink`/`log` are the optional durable stop-event
/// witness and diagnostic line sinks an adapter wires in when present.
#[derive(Clone, Default)]
pub struct SourceBuildContext {
    pub settings: toml::Table,
    pub default_graders: Vec<Grader>,
    pub stop_sink: Option<StopEventSink>,
    pub log: Option<LogSink>,
}

impl fmt::Debug for SourceBuildContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceBuildContext")
            .field("settings", &self.settings)
            .field("default_graders", &self.default_graders)
            .field("stop_sink", &self.stop_sink.is_some())
            .field("log", &self.log.is_some())
            .finish()
    }
}

impl SourceBuildContext {
    /// A context over the given `[source]` table and nothing else.
    pub fn new(settings: toml::Table) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }

    /// A context for the directory kind rooted at `tasks_dir`.
    pub fn for_directory(tasks_dir: impl Into<PathBuf>) -> Self {
        let mut settings = toml::Table::new();
        settings.insert("kind".into(), toml::Value::String(DIRECTORY_KIND.into()));
        settings.insert(
            "tasks_dir".into(),
            toml::Value::String(tasks_dir.into().to_string_lossy().into_owned()),
        );
        Self::new(settings)
    }

    pub fn with_default_graders(mut self, graders: Vec<Grader>) -> Self {
        self.default_graders = graders;
        self
    }

    pub fn with_stop_sink(mut self, sink: StopEventSink) -> Self {
        self.stop_sink = Some(sink);
        self
    }

    pub fn with_log(mut self, log: LogSink) -> Self {
        self.log = Some(log);
        self
    }

    /// The `[source] kind` the context names, if any.
    pub fn kind(&self) -> Option<&str> {
        self.string("kind")
    }

    /// A string-valued setting; `None` when absent or not a string.
    pub fn string(&self, key: &str) -> Option<&str> {
        self.settings.get(key).and_then(toml::Value::as_str)
    }

    /// A boolean-valued setting; `None` when absent or not a boolean.
    pub fn boolean(&self, key: &str) -> Option<bool> {
        self.settings.get(key).and_then(toml::Value::as_bool)
    }

    /// A string setting that the named kind requires.
    pub fn required_string(&self, kind: &str, key: &str) -> Result<&str, SourceRegistryError> {
        match self.settings.get(key) {
            Some(toml::Value::String(value)) => Ok(value),
            Some(other) => Err(SourceRegistryError::Config {
                kind: kind.to_string(),
                detail: format!("[source] {key} must be a string, got {}", other.type_str()),
            }),
            None => Err(SourceRegistryError::Config {
                kind: kind.to_string(),
                detail: format!("[source] {key} is required"),
            }),
        }
    }

    /// The `[source] tasks_dir` setting as a path, when present.
    pub fn tasks_dir(&self) -> Option<PathBuf> {
        self.string("tasks_dir").map(PathBuf::from)
    }
}

/// Why a named source could not be selected or built.
#[derive(Debug, thiserror::Error)]
pub enum SourceRegistryError {
    /// A name was requested that no registered backend answers to. The
    /// message lists the registered names so the operator reading a config
    /// error sees the valid choices immediately.
    #[error("unknown {family} {name:?}; registered: {}", format_known(known))]
    Unknown {
        family: &'static str,
        name: String,
        known: Vec<String>,
    },
    /// The context lacks (or mistypes) a setting the kind requires.
    #[error("cannot build {kind} source: {detail}")]
    Config { kind: String, detail: String },
    /// The adapter's own constructor failed (a transport probe, a bad
    /// default grader).
    #[error("cannot build {kind} source: {source}")]
    Build {
        kind: String,
        #[source]
        source: WorkSourceError,
    },
}

fn format_known(known: &[String]) -> String {
    if known.is_empty() {
        return "(none)".to_string();
    }
    known
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds one kind of adapter from a [`SourceBuildContext`].
///
/// This is the signature every future module registers with: a
/// `Send + Sync` closure (or fn pointer) from the context to a boxed
/// [`WorkSource`], failing with a [`SourceRegistryError`].
pub type SourceFactory = Arc<
    dyn Fn(&SourceBuildContext) -> Result<Box<dyn WorkSource>, SourceRegistryError> + Send + Sync,
>;

/// One named backend in the family.
#[derive(Clone)]
pub struct SourceSpec {
    pub name: String,
    /// A short human label for `list`/diagnostics.
    pub summary: String,
    pub factory: SourceFactory,
}

impl fmt::Debug for SourceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceSpec")
            .field("name", &self.name)
            .field("summary", &self.summary)
            .finish()
    }
}

/// A name -> [`SourceSpec`] table for the work-source family.
///
/// Registration order is preserved (so `names()` lists built-ins in the
/// order they were added); re-registering a name replaces its spec in
/// place.
#[derive(Debug, Clone, Default)]
pub struct SourceRegistry {
    specs: Vec<SourceSpec>,
}

impl SourceRegistry {
    /// An empty registry; see [`default_registry`] for the shipped one.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) a backend under `name`.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        summary: impl Into<String>,
        factory: SourceFactory,
    ) {
        let spec = SourceSpec {
            name: name.into(),
            summary: summary.into(),
            factory,
        };
        match self.specs.iter_mut().find(|s| s.name == spec.name) {
            Some(existing) => *existing = spec,
            None => self.specs.push(spec),
        }
    }

    /// Add `name` only if no backend already answers to it; returns
    /// whether it was added. Built-ins win a name collision, so a plugin
    /// can extend the choices but never shadow a shipped kind.
    pub fn register_if_absent(
        &mut self,
        name: impl Into<String>,
        summary: impl Into<String>,
        factory: SourceFactory,
    ) -> bool {
        let name = name.into();
        if self.contains(&name) {
            return false;
        }
        self.register(name, summary, factory);
        true
    }

    pub fn contains(&self, name: &str) -> bool {
        self.specs.iter().any(|s| s.name == name)
    }

    /// The registered names in registration order.
    pub fn names(&self) -> Vec<&str> {
        self.specs.iter().map(|s| s.name.as_str()).collect()
    }

    /// The registered specs in registration order, for diagnostics.
    pub fn specs(&self) -> &[SourceSpec] {
        &self.specs
    }

    /// The spec `name` selects, or the branded unknown-name error.
    pub fn resolve(&self, name: &str) -> Result<&SourceSpec, SourceRegistryError> {
        self.specs
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| SourceRegistryError::Unknown {
                family: SOURCE_FAMILY,
                name: name.to_string(),
                known: self.specs.iter().map(|s| s.name.clone()).collect(),
            })
    }

    /// Build the adapter `name` selects from `context`.
    pub fn build(
        &self,
        name: &str,
        context: &SourceBuildContext,
    ) -> Result<Box<dyn WorkSource>, SourceRegistryError> {
        (self.resolve(name)?.factory)(context)
    }

    /// Build the adapter the context's own `[source] kind` selects
    /// (defaulting to `directory` when the key is absent).
    pub fn build_from_context(
        &self,
        context: &SourceBuildContext,
    ) -> Result<Box<dyn WorkSource>, SourceRegistryError> {
        self.build(context.kind().unwrap_or(DIRECTORY_KIND), context)
    }
}

/// The factory behind the `directory` kind.
///
/// Reads `[source] tasks_dir` (falling back to [`DEFAULT_TASKS_DIR`]) and
/// wires the context's `log` sink in; `default_graders` and `stop_sink`
/// are ignored because directory tasks always carry their own graders and
/// an unloadable file is skipped and counted by the source itself.
pub fn build_directory_source(
    context: &SourceBuildContext,
) -> Result<Box<dyn WorkSource>, SourceRegistryError> {
    let tasks_dir = match context.settings.get("tasks_dir") {
        Some(toml::Value::String(value)) => PathBuf::from(value),
        Some(other) => {
            return Err(SourceRegistryError::Config {
                kind: DIRECTORY_KIND.to_string(),
                detail: format!(
                    "[source] tasks_dir must be a string, got {}",
                    other.type_str()
                ),
            });
        }
        None => PathBuf::from(DEFAULT_TASKS_DIR),
    };
    let mut source = DirectoryWorkSource::new(tasks_dir);
    if let Some(log) = &context.log {
        source = source.with_log(Arc::clone(log));
    }
    Ok(Box::new(source))
}

/// The shipped registry: the `directory` kind is registered here; the
/// GitHub kinds register themselves from their own modules with the same
/// [`SourceFactory`] signature.
pub fn default_registry() -> SourceRegistry {
    let mut registry = SourceRegistry::new();
    registry.register(
        DIRECTORY_KIND,
        "local directory of task JSON files (default)",
        Arc::new(build_directory_source),
    );
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{WorkItem, WorkReport};
    use flywheel_core::task::Task;
    use std::sync::Mutex;
    use tempfile::TempDir;

    struct MemorySource {
        items: Vec<WorkItem>,
    }

    impl WorkSource for MemorySource {
        fn source_kind(&self) -> &str {
            "memory"
        }
        fn source_name(&self) -> String {
            "mem".into()
        }
        fn list_work(&self) -> Result<Vec<WorkItem>, WorkSourceError> {
            Ok(self.items.clone())
        }
        fn report(&self, _report: &WorkReport) -> Result<(), WorkSourceError> {
            Ok(())
        }
    }

    fn memory_factory(id: &'static str) -> SourceFactory {
        Arc::new(move |_ctx: &SourceBuildContext| {
            let task = Task::new("Goal.", vec![]).with_id(id);
            Ok(Box::new(MemorySource {
                items: vec![WorkItem::new(task, format!("mem://{id}"))],
            }) as Box<dyn WorkSource>)
        })
    }

    #[test]
    fn default_registry_names_the_directory_kind() {
        let registry = default_registry();
        assert_eq!(registry.names(), ["directory"]);
        assert!(registry.contains("directory"));
        assert_eq!(
            registry.specs()[0].summary,
            "local directory of task JSON files (default)"
        );
        assert_eq!(BUILTIN_SOURCE_KINDS[0], DIRECTORY_KIND);
    }

    #[test]
    fn build_directory_returns_directory_source_bound_to_tasks_dir() {
        let registry = default_registry();
        let source = registry
            .build("directory", &SourceBuildContext::for_directory("queue"))
            .unwrap();
        assert_eq!(source.source_kind(), "directory");
        assert_eq!(source.source_name(), "queue");
    }

    #[test]
    fn build_directory_defaults_tasks_dir_and_rejects_wrong_type() {
        let registry = default_registry();
        let source = registry
            .build_from_context(&SourceBuildContext::default())
            .unwrap();
        assert_eq!(source.source_name(), DEFAULT_TASKS_DIR);

        let mut settings = toml::Table::new();
        settings.insert("tasks_dir".into(), toml::Value::Integer(3));
        let err = registry
            .build("directory", &SourceBuildContext::new(settings))
            .err()
            .expect("build must fail");
        assert!(matches!(err, SourceRegistryError::Config { .. }));
        assert!(err.to_string().contains("tasks_dir must be a string"));
    }

    #[test]
    fn build_directory_wires_log_sink_through() {
        let tmp = TempDir::new().unwrap();
        let phase = tmp.path().join("tasks/active/01");
        std::fs::create_dir_all(&phase).unwrap();
        std::fs::write(phase.join("bad.json"), "{").unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = Arc::clone(&lines);
        let log: LogSink =
            Arc::new(move |line: &str| sink_lines.lock().unwrap().push(line.to_string()));

        let source = default_registry()
            .build(
                "directory",
                &SourceBuildContext::for_directory(tmp.path().join("tasks")).with_log(log),
            )
            .unwrap();
        assert!(source.list_work().unwrap().is_empty());
        assert_eq!(lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn unknown_kind_lists_known_names_and_family() {
        let mut registry = default_registry();
        registry.register("github", "labeled GitHub issues", memory_factory("gh-1"));
        let err = registry
            .build("nope", &SourceBuildContext::default())
            .err()
            .expect("build must fail");
        let message = err.to_string();
        assert_eq!(
            message,
            "unknown work source \"nope\"; registered: \"directory\", \"github\""
        );
        assert!(matches!(err, SourceRegistryError::Unknown { .. }));

        let empty = SourceRegistry::new().resolve("x").unwrap_err();
        assert_eq!(
            empty.to_string(),
            "unknown work source \"x\"; registered: (none)"
        );
    }

    #[test]
    fn register_replaces_in_place_and_register_if_absent_keeps_builtin() {
        let mut registry = default_registry();
        registry.register("github_ci", "ci", memory_factory("ci-1"));
        registry.register("github", "issues", memory_factory("gh-1"));
        registry.register("github_ci", "ci (replaced)", memory_factory("ci-2"));
        assert_eq!(registry.names(), ["directory", "github_ci", "github"]);
        let items = registry
            .build("github_ci", &SourceBuildContext::default())
            .unwrap()
            .list_work()
            .unwrap();
        assert_eq!(items[0].id(), "ci-2");

        assert!(!registry.register_if_absent("directory", "plugin", memory_factory("x")));
        assert!(registry.register_if_absent("jira", "plugin", memory_factory("j-1")));
        assert_eq!(
            registry.resolve("directory").unwrap().summary,
            "local directory of task JSON files (default)"
        );
        assert_eq!(registry.names().len(), 4);
    }

    #[test]
    fn source_kind_alone_selects_the_factory() {
        let mut registry = default_registry();
        registry.register("github", "issues", memory_factory("gh-1"));
        let mut settings = toml::Table::new();
        settings.insert("kind".into(), toml::Value::String("github".into()));
        settings.insert("tasks_dir".into(), toml::Value::String("queue".into()));
        let ctx = SourceBuildContext::new(settings);
        assert_eq!(ctx.kind(), Some("github"));
        let source = registry.build_from_context(&ctx).unwrap();
        assert_eq!(source.source_kind(), "memory");
    }

    #[test]
    fn required_string_reports_missing_and_mistyped_keys() {
        let mut settings = toml::Table::new();
        settings.insert("repo".into(), toml::Value::Boolean(true));
        let ctx = SourceBuildContext::new(settings);
        let err = ctx.required_string("github", "label").unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot build github source: [source] label is required"
        );
        let err = ctx.required_string("github", "repo").unwrap_err();
        assert!(
            err.to_string()
                .contains("repo must be a string, got boolean")
        );
        assert_eq!(ctx.boolean("repo"), Some(true));
        assert_eq!(ctx.tasks_dir(), None);
    }
}
