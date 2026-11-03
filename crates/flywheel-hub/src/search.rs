//! Full-text search across the hub's sessions.
//!
//! Three sources, each read from what the hub already holds:
//!
//! * **scrollback** -- every logical line a session's server-side emulator
//!   retains (primary scrollback + rows, and the alternate screen while it
//!   is up), for every session with a terminal on this server, exited ones
//!   included while their terminal is kept;
//! * **transcript** -- an agent session's current conversation (Claude
//!   Code's JSONL, Codex's rollout): user and assistant text and tool
//!   calls, also for stopped and restored sessions;
//! * **commands** -- the suggested code blocks the hub saved per session.
//!
//! Work is bounded: a query is at most [`MAX_QUERY_BYTES`]; patterns go
//! through the `regex` crate (linear time, no backtracking) with compiled
//! size capped at [`REGEX_SIZE_LIMIT`]; every request has a time budget
//! ([`TIME_BUDGET`]) after which it answers with what it found (`partial`);
//! hits are capped per session and source and per request; at most
//! [`MAX_CONCURRENT`] searches run at once. Callers run [`Hub::search`] off
//! the async executor.
//!
//! Each source is scanned as one text (units joined by newlines) with one
//! regex pass rather than a call per line. Scrollback text is extracted
//! once per change of the session's output (keyed by bytes written and
//! size); transcripts are indexed incrementally from a byte offset, like
//! the commands tail, bounded per message ([`MAX_MESSAGE_BYTES`]) and per
//! transcript ([`MAX_TRANSCRIPT_BYTES`], oldest messages dropped first).

use crate::agent::SessionKind;
use crate::agentlog::Format;
pub use crate::agentlog::{MAX_LINE_BYTES, Role, tool_text};
use crate::commands::Command;
use crate::hub::{Hub, HubError, SessionView};
use crate::pty::PtySession;
use crate::state::{SessionState, StateError};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

mod page;
mod scan;
mod transcripts;

pub use page::{PageMessage, TranscriptPage};
pub use scan::{Matcher, Snippet, snippet};
pub use transcripts::{Extracted, MessageMeta, claude_messages, codex_messages, messages_of};

use scan::{Corpus, head_chars, scan};
use transcripts::TranscriptIndex;

/// Longest query accepted, in bytes.
pub const MAX_QUERY_BYTES: usize = 1024;
/// Hits per request by default.
pub const DEFAULT_LIMIT: usize = 200;
/// Most hits one request may ask for.
pub const MAX_LIMIT: usize = 2000;
/// Hits per session and source by default (the newest).
pub const DEFAULT_PER_SESSION: usize = 20;
/// Most hits per session and source one request may ask for.
pub const MAX_PER_SESSION: usize = 1000;
/// How long one request may search before answering with what it found.
pub const TIME_BUDGET: Duration = Duration::from_millis(1500);
/// Compiled size cap of a pattern (and of its lazy DFA cache).
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;
/// Searches running at once (more wait their turn).
pub const MAX_CONCURRENT: usize = 2;
/// Text of one transcript message kept for search, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Text kept per transcript, in bytes (oldest messages dropped first).
pub const MAX_TRANSCRIPT_BYTES: usize = 32 * 1024 * 1024;
/// Memory all transcript indexes together may hold; past it the least
/// recently searched are dropped (and re-read from their file when
/// searched again).
pub const MAX_INDEX_BYTES: usize = 256 * 1024 * 1024;
/// A transcript index not searched for this long is dropped.
pub const TRANSCRIPT_IDLE: Duration = Duration::from_secs(10 * 60);
/// Most hits per session and source one request may skip (paging).
pub const MAX_OFFSET: usize = 100_000;
/// Most messages one transcript page returns.
pub const MAX_PAGE_MESSAGES: usize = 200;
/// Characters of context before a match in a snippet (enough for a wide
/// row to show where a long line starts; narrow clients cut it from the
/// left).
const BEFORE_CHARS: usize = 160;
/// Characters of a snippet (before + match + after), at least.
const SNIPPET_CHARS: usize = 320;
/// Characters of the match itself shown in a snippet.
const MATCH_CHARS: usize = 200;
/// Characters of a scrollback match handed to the browser's terminal
/// search (`match_text`).
const MATCH_TEXT_CHARS: usize = 1000;
/// Least time between two looks for a session's transcript that was not
/// found.
const LOCATE_RETRY: Duration = Duration::from_secs(5);
/// Check the clock every this many matches.
const CLOCK_EVERY: usize = 4096;

/// Where a hit was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Scrollback,
    Transcript,
    Commands,
}

impl Source {
    pub const ALL: [Self; 3] = [Self::Scrollback, Self::Transcript, Self::Commands];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scrollback => "scrollback",
            Self::Transcript => "transcript",
            Self::Commands => "commands",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

/// Which screen a scrollback hit is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Screen {
    Primary,
    Alternate,
}

/// What a transcript message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Prose (a prompt, a reply).
    Text,
    /// A tool call: the tool's name and its input.
    Tool,
}

/// One hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Hit {
    Scrollback {
        screen: Screen,
        /// 0-based logical line, oldest first.
        index: usize,
        /// Logical lines after it (the browser counts from the bottom:
        /// its terminal holds the newest lines of the same list).
        from_end: usize,
        matches: usize,
        /// The first match exactly, for the terminal's own search.
        match_text: String,
        /// Earlier occurrences of `match_text` on the line.
        occurrence: usize,
        #[serde(flatten)]
        snippet: Snippet,
    },
    Transcript {
        /// Message index in the conversation (see the transcript page).
        index: u64,
        message_id: String,
        role: Role,
        kind: MessageKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        timestamp: Option<String>,
        matches: usize,
        #[serde(flatten)]
        snippet: Snippet,
    },
    Commands {
        seq: u64,
        lang: Option<String>,
        shell: bool,
        timestamp: Option<String>,
        matches: usize,
        #[serde(flatten)]
        snippet: Snippet,
    },
}

/// Matched units per source (all of them; `hits` holds the newest).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub scrollback: usize,
    pub transcript: usize,
    pub commands: usize,
}

/// One session's hits, each source newest first.
#[derive(Debug, Clone, Serialize)]
pub struct SessionGroup {
    pub session_id: String,
    pub session: String,
    pub workspace_id: String,
    pub workspace: String,
    pub kind: SessionKind,
    /// `running`, `exited`, or `stopped`.
    pub state: &'static str,
    /// The server holds its terminal (scrollback searchable, attachable).
    pub terminal: bool,
    pub conversation: Option<String>,
    pub hits: Vec<Hit>,
    pub counts: Counts,
    /// Some source has matches past this page (`counts` minus `offset`
    /// minus `hits`). A group past the request's `limit` has `hits`
    /// empty and only its `counts`.
    pub more: bool,
}

/// How much was searched.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Searched {
    pub sessions: usize,
    pub lines: usize,
    pub messages: usize,
    pub commands: usize,
}

/// A search's answer.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub sources: Vec<Source>,
    pub groups: Vec<SessionGroup>,
    /// Hits returned.
    pub hits: usize,
    /// The request's `limit` was reached: the sessions after it are only
    /// counted (groups with `counts` and no `hits`).
    pub truncated: bool,
    /// Hits skipped per session and source (the request's `offset`).
    pub offset: usize,
    /// The time budget ran out (or a transcript is still being indexed):
    /// what was searched is complete, the rest was not reached.
    pub partial: bool,
    pub elapsed_ms: u64,
    pub searched: Searched,
    /// Why something could not be searched (an unreadable transcript).
    pub notes: Vec<String>,
}

/// What to search for, and where.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// Only this workspace (id).
    pub workspace: Option<String>,
    /// Only this session (id).
    pub session: Option<String>,
    pub kinds: Vec<SessionKind>,
    pub sources: Vec<Source>,
    /// Most hits in all.
    pub limit: usize,
    /// Most hits per session and source.
    pub per_session: usize,
    /// Hits per session and source to skip, newest first (the next page
    /// of a session: the same query with `offset` = hits seen so far).
    pub offset: usize,
    pub budget: Duration,
}

impl SearchRequest {
    /// Every session, every source, the defaults.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            regex: false,
            case_sensitive: false,
            workspace: None,
            session: None,
            kinds: Vec::new(),
            sources: Source::ALL.to_vec(),
            limit: DEFAULT_LIMIT,
            per_session: DEFAULT_PER_SESSION,
            offset: 0,
            budget: TIME_BUDGET,
        }
    }
}

// --- the index --------------------------------------------------------

/// A transcript index with what eviction needs without locking it.
#[derive(Debug)]
struct TranscriptSlot {
    format: Format,
    index: Mutex<TranscriptIndex>,
    /// [`TranscriptIndex::footprint`] after its last update.
    bytes: AtomicUsize,
    /// When it was last searched or paged.
    used: Mutex<Instant>,
}

impl TranscriptSlot {
    /// The index, marked used now.
    fn lock(&self) -> MutexGuard<'_, TranscriptIndex> {
        *self.used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        self.index.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `index`'s size after an update.
    fn settle(&self, index: &TranscriptIndex) {
        self.bytes.store(index.footprint(), Ordering::Relaxed);
    }

    fn used(&self) -> Instant {
        *self.used.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A session's terminal text, read for one search.
#[derive(Debug)]
struct ScrollEntry {
    primary: Corpus,
    alternate: Option<Corpus>,
}

/// Where a session's transcript was last found.
#[derive(Debug, Clone)]
struct Located {
    conversation: Option<String>,
    file: Option<(PathBuf, Format)>,
    at: Instant,
}

/// The hub's search state: cached terminal text, transcript indexes, and
/// the concurrency limit.
#[derive(Debug)]
pub struct SearchIndex {
    permits: Arc<tokio::sync::Semaphore>,
    located: Mutex<HashMap<String, Located>>,
    transcripts: Mutex<HashMap<PathBuf, Arc<TranscriptSlot>>>,
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)),
            located: Mutex::new(HashMap::new()),
            transcripts: Mutex::new(HashMap::new()),
        }
    }
}

impl SearchIndex {
    /// A turn to search (at most [`MAX_CONCURRENT`] run at once).
    pub async fn permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("the search semaphore is never closed")
    }

    /// A session's terminal text now. Not kept here: the emulator keeps
    /// what it read last and re-reads only rows scrolled in since (see
    /// [`crate::vt::Emulator::search_text`]), so this is a copy, cheap to
    /// take, and no second copy of every scrollback stays in memory.
    fn scroll_entry(pty: &PtySession) -> ScrollEntry {
        let text = pty.search_text();
        ScrollEntry {
            primary: Corpus {
                text: text.primary.text,
                starts: text.primary.starts,
            },
            alternate: text.alternate.as_ref().map(Corpus::from_units),
        }
    }

    fn transcript(&self, path: &Path, format: Format) -> Arc<TranscriptSlot> {
        let mut transcripts = self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = transcripts.get(path).filter(|s| s.format == format) {
            return Arc::clone(slot);
        }
        let slot = Arc::new(TranscriptSlot {
            format,
            index: Mutex::new(TranscriptIndex::new(path.to_path_buf(), format)),
            bytes: AtomicUsize::new(0),
            used: Mutex::new(Instant::now()),
        });
        transcripts.insert(path.to_path_buf(), Arc::clone(&slot));
        slot
    }

    /// Bytes all transcript indexes hold.
    pub fn transcript_bytes(&self) -> usize {
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|s| s.bytes.load(Ordering::Relaxed))
            .sum()
    }

    /// Transcript indexes held.
    pub fn transcript_count(&self) -> usize {
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Drop transcript indexes idle for [`TRANSCRIPT_IDLE`], then the
    /// least recently used until all hold at most [`MAX_INDEX_BYTES`].
    /// One in use by a search right now is kept. Cheap (no file work):
    /// the transcript watcher calls it every poll, searches after each
    /// transcript.
    pub fn sweep(&self) {
        self.sweep_with(MAX_INDEX_BYTES, TRANSCRIPT_IDLE);
    }

    fn sweep_with(&self, cap: usize, idle: Duration) {
        let mut transcripts = self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Held only by the map: no search is using it.
        let free = |slot: &Arc<TranscriptSlot>| Arc::strong_count(slot) == 1;
        transcripts.retain(|_, slot| !free(slot) || slot.used().elapsed() < idle);
        let mut total: usize = transcripts
            .values()
            .map(|s| s.bytes.load(Ordering::Relaxed))
            .sum();
        if total <= cap {
            return;
        }
        let mut oldest: Vec<(Instant, PathBuf, usize)> = transcripts
            .iter()
            .filter(|(_, slot)| free(slot))
            .map(|(path, slot)| {
                (
                    slot.used(),
                    path.clone(),
                    slot.bytes.load(Ordering::Relaxed),
                )
            })
            .collect();
        oldest.sort();
        for (_, path, bytes) in oldest {
            if total <= cap {
                break;
            }
            transcripts.remove(&path);
            total -= bytes;
        }
    }

    /// Forget what belongs to sessions no longer there (`sessions`: every
    /// session the hub has).
    fn prune(&self, sessions: &HashSet<String>) {
        let mut located = self.located.lock().unwrap_or_else(PoisonError::into_inner);
        located.retain(|id, _| sessions.contains(id));
        let used: HashSet<PathBuf> = located
            .values()
            .filter_map(|l| l.file.as_ref().map(|(p, _)| p.clone()))
            .collect();
        drop(located);
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|path, _| used.contains(path));
    }
}

/// Which of a session's hits (per source, newest first) a request shows.
#[derive(Debug, Clone, Copy)]
struct Page {
    skip: usize,
    take: usize,
}

impl Page {
    /// Newest matched units to keep while scanning.
    fn keep(self) -> usize {
        self.skip + self.take
    }
}

fn state_name(state: &SessionState) -> &'static str {
    match state {
        SessionState::Running => "running",
        SessionState::Exited { .. } => "exited",
        SessionState::Stopped { .. } => "stopped",
    }
}
impl Hub {
    /// Where `view`'s current conversation is written, looked up at most
    /// every [`LOCATE_RETRY`] while it is not found.
    fn transcript_of(&self, view: &SessionView) -> Option<(PathBuf, Format)> {
        let record = &view.record;
        if !record.kind.is_agent() {
            return None;
        }
        let index = self.search_index();
        let cached = index
            .located
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&record.id)
            .cloned();
        if let Some(found) = cached.filter(|l| {
            l.conversation == record.agent_session_id
                && (l.file.is_some() || l.at.elapsed() < LOCATE_RETRY)
        }) {
            return found.file;
        }
        let file = self.command_book().transcript_file(record);
        index
            .located
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                record.id.clone(),
                Located {
                    conversation: record.agent_session_id.clone(),
                    file: file.clone(),
                    at: Instant::now(),
                },
            );
        file
    }

    /// Search the sessions `request` names. Blocking (reads terminals and
    /// files): call it off the async executor, holding a
    /// [`SearchIndex::permit`].
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResponse, HubError> {
        let started = Instant::now();
        let deadline = started + request.budget;
        let matcher = Matcher::new(&request.query, request.regex, request.case_sensitive)
            .map_err(StateError::Invalid)?;
        let limit = request.limit.clamp(1, MAX_LIMIT);
        let per_session = request.per_session.clamp(1, MAX_PER_SESSION);
        let offset = request.offset.min(MAX_OFFSET);
        let sources: Vec<Source> = Source::ALL
            .into_iter()
            .filter(|s| request.sources.contains(s))
            .collect();
        if let Some(id) = &request.workspace {
            self.workspace(id)?;
        }
        if let Some(id) = &request.session {
            self.session(id)?;
        }
        let workspaces: HashMap<String, String> = self
            .workspaces()
            .into_iter()
            .map(|w| (w.id, w.name))
            .collect();
        let all = self.sessions(None);
        let ids: HashSet<String> = all.iter().map(|s| s.record.id.clone()).collect();
        self.search_index().prune(&ids);

        let mut response = SearchResponse {
            query: request.query.clone(),
            regex: request.regex,
            case_sensitive: request.case_sensitive,
            sources: sources.clone(),
            groups: Vec::new(),
            hits: 0,
            truncated: false,
            offset,
            partial: false,
            elapsed_ms: 0,
            searched: Searched::default(),
            notes: Vec::new(),
        };
        let wanted = all.into_iter().filter(|view| {
            let r = &view.record;
            request
                .workspace
                .as_ref()
                .is_none_or(|w| &r.workspace_id == w)
                && request.session.as_ref().is_none_or(|s| &r.id == s)
                && (request.kinds.is_empty() || request.kinds.contains(&r.kind))
        });
        for view in wanted {
            if Instant::now() >= deadline {
                response.partial = true;
                break;
            }
            // Past the limit, sessions are still searched but only
            // counted, so every one that matches is listed.
            let counting = response.hits >= limit;
            if counting {
                response.truncated = true;
            }
            let page = if counting {
                Page { skip: 0, take: 0 }
            } else {
                Page {
                    skip: offset,
                    take: per_session,
                }
            };
            response.searched.sessions += 1;
            let record = &view.record;
            let mut group = SessionGroup {
                session_id: record.id.clone(),
                session: record.name.clone(),
                workspace_id: record.workspace_id.clone(),
                workspace: workspaces
                    .get(&record.workspace_id)
                    .cloned()
                    .unwrap_or_default(),
                kind: record.kind,
                state: state_name(&record.state),
                terminal: view.live.is_some(),
                conversation: record.agent_session_id.clone(),
                hits: Vec::new(),
                counts: Counts::default(),
                more: false,
            };
            for &source in &sources {
                if Instant::now() >= deadline {
                    response.partial = true;
                    break;
                }
                let complete = match source {
                    Source::Scrollback => self.search_scrollback(
                        &view,
                        &matcher,
                        page,
                        deadline,
                        &mut group,
                        &mut response.searched,
                    ),
                    Source::Transcript => self.search_transcript(
                        &view,
                        &matcher,
                        page,
                        deadline,
                        &mut group,
                        &mut response,
                    ),
                    Source::Commands => {
                        self.search_commands(
                            &view,
                            &matcher,
                            page,
                            &mut group,
                            &mut response.searched,
                        );
                        true
                    }
                };
                if !complete {
                    response.partial = true;
                }
            }
            if group.hits.is_empty() && !group.more {
                continue;
            }
            let room = limit.saturating_sub(response.hits);
            if group.hits.len() > room {
                group.hits.truncate(room);
                group.more = true;
                response.truncated = true;
            }
            response.hits += group.hits.len();
            response.groups.push(group);
        }
        response.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(response)
    }

    fn search_scrollback(
        &self,
        view: &SessionView,
        matcher: &Matcher,
        page: Page,
        deadline: Instant,
        group: &mut SessionGroup,
        searched: &mut Searched,
    ) -> bool {
        let keep = page.keep();
        let Some(pty) = self.live_session(&view.record.id) else {
            return true;
        };
        let entry = SearchIndex::scroll_entry(&pty);
        let mut complete = true;
        let mut hits = Vec::new();
        // The screen in view first (the alternate one, when up), then
        // the primary's newest lines.
        let screens = entry
            .alternate
            .as_ref()
            .map(|c| (Screen::Alternate, c))
            .into_iter()
            .chain(std::iter::once((Screen::Primary, &entry.primary)));
        for (screen, corpus) in screens {
            searched.lines += corpus.len();
            let found = scan(corpus, matcher, keep.saturating_sub(hits.len()), deadline);
            complete &= found.complete;
            group.counts.scrollback += found.units;
            for hit in found.hits.iter().rev() {
                let line = corpus.unit(hit.unit);
                let (match_text, _) = head_chars(&line[hit.start..hit.end], MATCH_TEXT_CHARS);
                hits.push(Hit::Scrollback {
                    screen,
                    index: hit.unit,
                    from_end: corpus.len() - 1 - hit.unit,
                    matches: hit.matches,
                    occurrence: line[..hit.start].matches(match_text).count(),
                    match_text: match_text.to_owned(),
                    snippet: snippet(line, hit.start, hit.end),
                });
            }
            if !complete {
                break;
            }
        }
        group.more |= group.counts.scrollback > hits.len();
        group.hits.extend(hits.into_iter().skip(page.skip));
        complete
    }

    fn search_transcript(
        &self,
        view: &SessionView,
        matcher: &Matcher,
        page: Page,
        deadline: Instant,
        group: &mut SessionGroup,
        response: &mut SearchResponse,
    ) -> bool {
        let Some((path, format)) = self.transcript_of(view) else {
            return true;
        };
        let slot = self.search_index().transcript(&path, format);
        let mut index = slot.lock();
        let read = index.update(deadline);
        slot.settle(&index);
        if let Some(error) = &index.error {
            response
                .notes
                .push(format!("{}: {error}", view.record.label()));
        }
        response.searched.messages += index.messages.len();
        let found = scan(&index.corpus, matcher, page.keep(), deadline);
        group.counts.transcript += found.units;
        group.more |= found.units > found.hits.len();
        for hit in found.hits.iter().rev().skip(page.skip) {
            let meta = &index.messages[hit.unit];
            let text = index.corpus.unit(hit.unit);
            group.hits.push(Hit::Transcript {
                index: meta.index,
                message_id: meta.message_id.clone(),
                role: meta.role,
                kind: meta.kind,
                tool: meta.tool.clone(),
                timestamp: meta.timestamp.clone(),
                matches: hit.matches,
                snippet: snippet(text, hit.start, hit.end),
            });
        }
        drop(index);
        drop(slot);
        self.search_index().sweep();
        read && found.complete
    }

    fn search_commands(
        &self,
        view: &SessionView,
        matcher: &Matcher,
        page: Page,
        group: &mut SessionGroup,
        searched: &mut Searched,
    ) {
        let keep = page.keep();
        if !view.record.kind.is_agent() {
            return;
        }
        // Up to date first, as the drawer's list is (a running agent's
        // blocks are otherwise read within the watcher's next poll).
        let running = view.live.as_ref().is_some_and(|l| l.pid.is_some());
        if running {
            self.refresh_commands(&view.record);
        }
        let mut hits = Vec::new();
        let mut count = 0;
        self.command_book()
            .visit_commands(&view.record.id, |command: &Command| {
                searched.commands += 1;
                let mut matches = matcher.find_iter(&command.text);
                if let Some((start, end)) = matches.next() {
                    count += 1;
                    if hits.len() < keep {
                        hits.push(Hit::Commands {
                            seq: command.seq,
                            lang: command.lang.clone(),
                            shell: command.shell,
                            timestamp: command.timestamp.clone(),
                            matches: 1 + matches.count(),
                            snippet: snippet(&command.text, start, end),
                        });
                    }
                }
                true
            });
        group.counts.commands += count;
        group.more |= count > hits.len();
        group.hits.extend(hits.into_iter().skip(page.skip));
    }
}

#[cfg(test)]
mod tests;
