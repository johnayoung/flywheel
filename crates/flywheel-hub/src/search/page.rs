//! The transcript viewer: a window of a session's current conversation.

use super::transcripts::MessageMeta;
use super::{MAX_PAGE_MESSAGES, TIME_BUDGET};
use crate::hub::{Hub, HubError};
use crate::state::StateError;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Instant;

/// One transcript message with its text (the transcript page).
#[derive(Debug, Clone, Serialize)]
pub struct PageMessage {
    #[serde(flatten)]
    pub meta: MessageMeta,
    pub text: String,
}

/// A window of a session's current conversation.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptPage {
    pub session_id: String,
    pub conversation: Option<String>,
    /// The file read (`null`: none found yet).
    pub transcript: Option<PathBuf>,
    /// Oldest message index still held (older ones were dropped).
    pub first: u64,
    /// One past the newest message index.
    pub next: u64,
    /// The file was read to its end.
    pub complete: bool,
    pub messages: Vec<PageMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Hub {
    /// Messages of session `id`'s current conversation: `before` up to
    /// and including message `around` and `after` after it (the newest
    /// `before` when `around` is `None`). Reads what the file has new
    /// first, within [`TIME_BUDGET`]. Blocking.
    pub fn transcript_page(
        &self,
        id: &str,
        around: Option<u64>,
        before: usize,
        after: usize,
    ) -> Result<TranscriptPage, HubError> {
        let view = self.session(id)?;
        let record = &view.record;
        if !record.kind.is_agent() {
            return Err(StateError::Invalid(format!(
                "session {} is a shell: it has no agent transcript",
                record.label()
            ))
            .into());
        }
        let mut page = TranscriptPage {
            session_id: record.id.clone(),
            conversation: record.agent_session_id.clone(),
            transcript: None,
            first: 0,
            next: 0,
            complete: true,
            messages: Vec::new(),
            error: None,
        };
        let Some((path, format)) = self.transcript_of(&view, true) else {
            return Ok(page);
        };
        let slot = self.search_index().transcript(&path, format);
        let mut index = slot.lock();
        page.complete = index.update(Instant::now() + TIME_BUDGET);
        slot.settle(&index);
        page.transcript = Some(path);
        page.first = index.first_index();
        page.next = index.next_index;
        page.error = index.error.clone();
        let before = before.min(MAX_PAGE_MESSAGES);
        let after = after.min(MAX_PAGE_MESSAGES - before);
        let (from, to) = match around {
            Some(at) => (
                at.saturating_sub(before as u64),
                at.saturating_add(after as u64).saturating_add(1),
            ),
            None => (
                index.next_index.saturating_sub(before as u64),
                index.next_index,
            ),
        };
        let first = index.first_index();
        for i in from.max(first)..to.min(index.next_index) {
            let unit = (i - first) as usize;
            page.messages.push(PageMessage {
                meta: index.messages[unit].clone(),
                text: index.corpus.unit(unit).to_owned(),
            });
        }
        drop(index);
        drop(slot);
        self.search_index().sweep();
        Ok(page)
    }
}
