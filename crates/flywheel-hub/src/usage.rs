//! Token usage and cost of agent sessions, read from their transcripts.
//!
//! Claude Code writes each API response's `usage` (uncached input, output,
//! cache reads, and cache writes by TTL) on the assistant lines of its
//! transcript, with the model id; several lines of one response repeat it
//! under one message id, so a repeat replaces rather than adds. Codex
//! writes `token_count` events (cumulative totals) to its rollout, and the
//! model on each `turn_context`; the hub counts the growth of the totals.
//!
//! Costs come from [`PRICES`], a small table keyed by model id. A model
//! not in it is counted in tokens only and reported as unpriced: a price
//! is never guessed. Each session keeps all-time totals and per-day
//! (local date) totals for [`KEEP_DAYS`] days, saved with its suggested
//! commands; a removed session's totals move to the hub's archive
//! ([`UsageArchive`]) so day and week rollups keep them.

use chrono::{DateTime, Datelike, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

/// Where [`PRICES`] come from, and as of when.
pub const PRICE_SOURCE: &str = "Anthropic API pricing (claude-api reference, cached 2026-10-06: per-model input/output/cache-read rates; cache writes 1.25x input for 5-minute TTL, 2x for 1-hour; fast mode 2x) and OpenAI API pricing (openai.com/api/pricing, 2025-11) for GPT-5 models; first-party API list prices, not Bedrock/Vertex or subscription plans";
pub const PRICE_AS_OF: &str = "2026-10-06";
/// Per-day totals kept per session (the week rollup needs 7).
pub const KEEP_DAYS: i64 = 62;
/// Responses remembered per session to replace a repeated one.
const RECENT_TURNS: usize = 512;
/// The archive file in the state dir.
pub const ARCHIVE_FILE: &str = "usage-archive.json";

/// Tokens of one or more API calls. `input` is the uncached part.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    /// Written to the 5-minute cache.
    #[serde(default)]
    pub cache_write_5m: u64,
    /// Written to the 1-hour cache.
    #[serde(default)]
    pub cache_write_1h: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }

    /// Everything sent as the prompt (what long-context pricing keys on).
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }

    fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
    }

    fn sub(&mut self, other: &Tokens) {
        self.input = self.input.saturating_sub(other.input);
        self.output = self.output.saturating_sub(other.output);
        self.cache_read = self.cache_read.saturating_sub(other.cache_read);
        self.cache_write_5m = self.cache_write_5m.saturating_sub(other.cache_write_5m);
        self.cache_write_1h = self.cache_write_1h.saturating_sub(other.cache_write_1h);
    }
}

// --- prices ------------------------------------------------------------------

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}

/// One model's prices.
#[derive(Debug, Clone, Copy)]
pub struct ModelPrice {
    pub id: &'static str,
    pub base: Rate,
    /// Prompts larger than this many tokens are billed at the second rate.
    pub long_context: Option<(u64, Rate)>,
    /// Fast mode's multiplier on every rate (`None`: fast mode unpriced).
    pub fast: Option<f64>,
}

const fn rate(input: f64, output: f64, cache_read: f64) -> Rate {
    Rate {
        input,
        output,
        cache_read,
    }
}

const fn model(id: &'static str, base: Rate) -> ModelPrice {
    ModelPrice {
        id,
        base,
        long_context: None,
        fast: None,
    }
}

/// Cache writes cost this times the input rate (5-minute TTL)...
pub const CACHE_WRITE_5M: f64 = 1.25;
/// ...and this for the 1-hour TTL.
pub const CACHE_WRITE_1H: f64 = 2.0;

/// The price table (see [`PRICE_SOURCE`]). Ids match exactly or with a
/// dated snapshot suffix (`-YYYYMMDD`); provider-prefixed ids (Bedrock's
/// `anthropic.`, Vertex's `@date`) are billed differently and stay
/// unpriced.
pub const PRICES: &[ModelPrice] = &[
    model("claude-fable-5-1", rate(10.0, 50.0, 0.25)),
    model("claude-mythos-5-1", rate(10.0, 50.0, 0.25)),
    model("claude-fable-5", rate(10.0, 50.0, 1.0)),
    model("claude-mythos-5", rate(10.0, 50.0, 1.0)),
    ModelPrice {
        fast: Some(2.0),
        ..model("claude-opus-5-5", rate(4.0, 20.0, 0.20))
    },
    ModelPrice {
        fast: Some(2.0),
        ..model("claude-opus-5", rate(5.0, 25.0, 0.50))
    },
    model("claude-opus-4-8", rate(5.0, 25.0, 0.50)),
    model("claude-opus-4-7", rate(5.0, 25.0, 0.50)),
    model("claude-opus-4-6", rate(5.0, 25.0, 0.50)),
    model("claude-opus-4-5", rate(5.0, 25.0, 0.50)),
    model("claude-opus-4-1", rate(15.0, 75.0, 1.50)),
    model("claude-opus-4-0", rate(15.0, 75.0, 1.50)),
    model("claude-opus-4", rate(15.0, 75.0, 1.50)),
    model("claude-sonnet-5-5", rate(2.0, 10.0, 0.20)),
    model("claude-sonnet-5", rate(2.0, 10.0, 0.20)),
    model("claude-sonnet-4-6", rate(3.0, 15.0, 0.30)),
    ModelPrice {
        long_context: Some((200_000, rate(6.0, 22.5, 0.60))),
        ..model("claude-sonnet-4-5", rate(3.0, 15.0, 0.30))
    },
    ModelPrice {
        long_context: Some((200_000, rate(6.0, 22.5, 0.60))),
        ..model("claude-sonnet-4-0", rate(3.0, 15.0, 0.30))
    },
    ModelPrice {
        long_context: Some((200_000, rate(6.0, 22.5, 0.60))),
        ..model("claude-sonnet-4", rate(3.0, 15.0, 0.30))
    },
    ModelPrice {
        long_context: Some((100_000, rate(0.50, 2.50, 0.05))),
        ..model("claude-haiku-5-5", rate(0.10, 0.50, 0.01))
    },
    model("claude-haiku-4-5", rate(1.0, 5.0, 0.10)),
    model("gpt-5", rate(1.25, 10.0, 0.125)),
    model("gpt-5-codex", rate(1.25, 10.0, 0.125)),
    model("gpt-5.1", rate(1.25, 10.0, 0.125)),
    model("gpt-5.1-codex", rate(1.25, 10.0, 0.125)),
    model("gpt-5-mini", rate(0.25, 2.0, 0.025)),
    model("gpt-5.1-codex-mini", rate(0.25, 2.0, 0.025)),
];

/// `model`'s prices: an exact id, or one with a `-YYYYMMDD` snapshot
/// suffix.
pub fn price_of(model: &str) -> Option<&'static ModelPrice> {
    PRICES.iter().find(|p| {
        model == p.id
            || model.strip_prefix(p.id).is_some_and(|rest| {
                rest.len() == 9
                    && rest.starts_with('-')
                    && rest[1..].bytes().all(|b| b.is_ascii_digit())
            })
    })
}

/// How a bucket of tokens is billed: the model, fast mode, and whether
/// its prompts were over the model's long-context threshold.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BillingKey {
    pub model: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub long_context: bool,
}

impl BillingKey {
    /// The key of one call of `model` with these tokens.
    pub fn of(model: &str, fast: bool, tokens: &Tokens) -> Self {
        let long_context = price_of(model)
            .and_then(|p| p.long_context)
            .is_some_and(|(threshold, _)| tokens.prompt() > threshold);
        Self {
            model: model.to_owned(),
            fast,
            long_context,
        }
    }

    /// The USD cost of `tokens` billed this way (`None`: no price known).
    pub fn cost(&self, tokens: &Tokens) -> Option<f64> {
        let price = price_of(&self.model)?;
        let rate = if self.long_context {
            price.long_context?.1
        } else {
            price.base
        };
        let multiplier = if self.fast { price.fast? } else { 1.0 };
        let micro = tokens.input as f64 * rate.input
            + tokens.cache_write_5m as f64 * rate.input * CACHE_WRITE_5M
            + tokens.cache_write_1h as f64 * rate.input * CACHE_WRITE_1H
            + tokens.cache_read as f64 * rate.cache_read
            + tokens.output as f64 * rate.output;
        Some(micro * multiplier / 1_000_000.0)
    }

    /// How listings name it: `model`, plus `(fast)` / `(long context)`.
    pub fn label(&self) -> String {
        let mut label = self.model.clone();
        if self.fast {
            label.push_str(" (fast)");
        }
        if self.long_context {
            label.push_str(" (long context)");
        }
        label
    }
}

/// Tokens by how they are billed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ledger(Vec<(BillingKey, Tokens)>);

impl Ledger {
    pub fn add(&mut self, key: &BillingKey, tokens: &Tokens) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some((_, t)) => t.add(tokens),
            None => {
                self.0.push((key.clone(), *tokens));
                self.0.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
    }

    fn sub(&mut self, key: &BillingKey, tokens: &Tokens) {
        if let Some((_, t)) = self.0.iter_mut().find(|(k, _)| k == key) {
            t.sub(tokens);
        }
        self.0.retain(|(_, t)| !t.is_zero());
    }

    pub fn merge(&mut self, other: &Ledger) {
        for (key, tokens) in &other.0 {
            self.add(key, tokens);
        }
    }

    pub fn entries(&self) -> &[(BillingKey, Tokens)] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Its tokens and cost: the priced part summed, unpriced models named.
    pub fn cost(&self) -> Cost {
        let mut cost = Cost::default();
        for (key, tokens) in &self.0 {
            cost.tokens.add(tokens);
            match key.cost(tokens) {
                Some(usd) => cost.cost_usd += usd,
                None => {
                    let label = key.label();
                    if !cost.unpriced_models.contains(&label) {
                        cost.unpriced_models.push(label);
                    }
                }
            }
        }
        cost.total_tokens = cost.tokens.total();
        cost.priced = cost.unpriced_models.is_empty();
        cost
    }
}

/// Tokens and what they cost. `cost_usd` covers the priced models only;
/// `priced` is false when some tokens are of a model without a price
/// (named in `unpriced_models`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub tokens: Tokens,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub priced: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
}

impl Default for Cost {
    fn default() -> Self {
        Self {
            tokens: Tokens::default(),
            total_tokens: 0,
            cost_usd: 0.0,
            priced: true,
            unpriced_models: Vec::new(),
        }
    }
}

// --- reading transcripts -----------------------------------------------------

/// One API call's usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// The response's id (Claude's message id): a repeat replaces it.
    pub id: Option<String>,
    pub model: String,
    pub fast: bool,
    pub timestamp: Option<String>,
    pub tokens: Tokens,
}

fn count(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

/// The usage on a Claude Code transcript line: an `assistant` line with
/// `message.usage` and a real model (synthetic messages cost nothing).
pub fn claude_turn(value: &serde_json::Value) -> Option<Turn> {
    if value.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let message = value.get("message")?;
    let usage = message.get("usage")?.as_object()?;
    let usage = serde_json::Value::Object(usage.clone());
    let model = message.get("model")?.as_str()?;
    if model.is_empty() || model.starts_with('<') {
        return None;
    }
    let written = count(&usage, "cache_creation_input_tokens");
    let (write_5m, write_1h) = match usage.get("cache_creation") {
        Some(split) if split.is_object() => (
            count(split, "ephemeral_5m_input_tokens"),
            count(split, "ephemeral_1h_input_tokens"),
        ),
        // No split: the default (5-minute) TTL.
        _ => (written, 0),
    };
    let tokens = Tokens {
        input: count(&usage, "input_tokens"),
        output: count(&usage, "output_tokens"),
        cache_read: count(&usage, "cache_read_input_tokens"),
        cache_write_5m: write_5m,
        cache_write_1h: write_1h,
    };
    if tokens.is_zero() {
        return None;
    }
    let id = message
        .get("id")
        .or_else(|| value.get("uuid"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some(Turn {
        id,
        model: model.to_owned(),
        fast: usage.get("speed").and_then(serde_json::Value::as_str) == Some("fast"),
        timestamp: value
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        tokens,
    })
}

/// Codex's token counts (OpenAI's shape: `input_tokens` includes the
/// cached ones, `output_tokens` the reasoning ones).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexTotals {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
}

impl CodexTotals {
    fn from_json(value: &serde_json::Value) -> Option<Self> {
        value.as_object()?;
        Some(Self {
            input: count(value, "input_tokens"),
            cached: count(value, "cached_input_tokens"),
            output: count(value, "output_tokens"),
        })
    }

    fn tokens(&self) -> Tokens {
        Tokens {
            input: self.input.saturating_sub(self.cached),
            cache_read: self.cached.min(self.input),
            output: self.output,
            ..Tokens::default()
        }
    }

    /// What `self` adds over `before`, when it grew from it (else `None`:
    /// a new count).
    fn since(&self, before: &Self) -> Option<Self> {
        (self.input >= before.input && self.cached >= before.cached && self.output >= before.output)
            .then(|| Self {
                input: self.input - before.input,
                cached: self.cached - before.cached,
                output: self.output - before.output,
            })
    }
}

/// What a Codex rollout line says about usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexLine {
    /// The model of the turns that follow (`turn_context`).
    Model(String),
    /// A `token_count` event.
    Count {
        total: Option<CodexTotals>,
        last: Option<CodexTotals>,
        timestamp: Option<String>,
    },
}

/// The usage part of a Codex rollout line, if any.
pub fn codex_line(value: &serde_json::Value) -> Option<CodexLine> {
    let payload = value.get("payload")?;
    match value.get("type")?.as_str()? {
        "turn_context" => payload
            .get("model")
            .and_then(serde_json::Value::as_str)
            .filter(|m| !m.is_empty())
            .map(|m| CodexLine::Model(m.to_owned())),
        "event_msg" if payload.get("type")?.as_str()? == "token_count" => {
            let info = payload.get("info").filter(|i| i.is_object())?;
            let total = info
                .get("total_token_usage")
                .and_then(CodexTotals::from_json);
            let last = info
                .get("last_token_usage")
                .and_then(CodexTotals::from_json);
            if total.is_none() && last.is_none() {
                return None;
            }
            Some(CodexLine::Count {
                total,
                last,
                timestamp: value
                    .get("timestamp")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            })
        }
        _ => None,
    }
}

/// Where a Codex rollout's counting stands (kept with its tail).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCursor {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub total: Option<CodexTotals>,
}

/// Placeholder model of Codex tokens counted before any `turn_context`.
pub const CODEX_UNKNOWN_MODEL: &str = "codex (model not recorded)";

impl CodexCursor {
    /// Feed one rollout line's usage; the turn it completes, if any.
    pub fn feed(&mut self, line: CodexLine) -> Option<Turn> {
        match line {
            CodexLine::Model(model) => {
                self.model = Some(model);
                None
            }
            CodexLine::Count {
                total,
                last,
                timestamp,
            } => {
                let delta = match (total, self.total) {
                    // Totals: what they grew by (a repeated event adds
                    // nothing); a smaller total started a new count.
                    (Some(total), Some(before)) => total.since(&before).unwrap_or(total),
                    (Some(total), None) => total,
                    (None, _) => last?,
                };
                if total.is_some() {
                    self.total = total;
                }
                let tokens = delta.tokens();
                (!tokens.is_zero()).then(|| Turn {
                    id: None,
                    model: self
                        .model
                        .clone()
                        .unwrap_or_else(|| CODEX_UNKNOWN_MODEL.to_owned()),
                    fast: false,
                    timestamp,
                    tokens,
                })
            }
        }
    }
}

// --- one session --------------------------------------------------------------

/// A counted response, kept a while so a repeat can replace it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Counted {
    id: String,
    day: NaiveDate,
    key: BillingKey,
    tokens: Tokens,
}

/// The most recent call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastTurn {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub key: BillingKey,
    pub tokens: Tokens,
}

/// A session's usage, as saved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    /// The workspace it belongs to (rollups after the session is gone).
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub turns: u64,
    #[serde(default)]
    pub total: Ledger,
    #[serde(default)]
    pub days: BTreeMap<NaiveDate, Ledger>,
    #[serde(default)]
    recent: VecDeque<Counted>,
    #[serde(default)]
    pub last: Option<LastTurn>,
}

/// The local date of an RFC 3339 timestamp (now when absent or invalid).
pub fn local_day(timestamp: Option<&str>) -> NaiveDate {
    timestamp
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Local).date_naive())
        .unwrap_or_else(|| Local::now().date_naive())
}

/// The Monday starting `day`'s week.
pub fn week_start(day: NaiveDate) -> NaiveDate {
    day - chrono::Duration::days(i64::from(day.weekday().num_days_from_monday()))
}

impl SessionUsage {
    pub fn is_empty(&self) -> bool {
        self.turns == 0 && self.total.is_empty()
    }

    /// Count one call. A repeated id (another line of the same response)
    /// replaces what was counted for it. Returns whether anything changed.
    pub fn observe(&mut self, turn: &Turn) -> bool {
        let key = BillingKey::of(&turn.model, turn.fast, &turn.tokens);
        let day = local_day(turn.timestamp.as_deref());
        if let Some(id) = &turn.id
            && let Some(at) = self.recent.iter().position(|c| &c.id == id)
        {
            let old = self.recent[at].clone();
            if old.tokens == turn.tokens && old.key == key {
                return false;
            }
            self.total.sub(&old.key, &old.tokens);
            if let Some(ledger) = self.days.get_mut(&old.day) {
                ledger.sub(&old.key, &old.tokens);
                if ledger.is_empty() {
                    self.days.remove(&old.day);
                }
            }
            self.recent[at].key = key.clone();
            self.recent[at].tokens = turn.tokens;
            self.recent[at].day = day;
        } else {
            self.turns += 1;
            if let Some(id) = &turn.id {
                if self.recent.len() == RECENT_TURNS {
                    self.recent.pop_front();
                }
                self.recent.push_back(Counted {
                    id: id.clone(),
                    day,
                    key: key.clone(),
                    tokens: turn.tokens,
                });
            }
        }
        self.total.add(&key, &turn.tokens);
        self.days.entry(day).or_default().add(&key, &turn.tokens);
        let newest = *self.days.keys().next_back().unwrap_or(&day);
        let oldest_kept = newest - chrono::Duration::days(KEEP_DAYS);
        self.days.retain(|d, _| *d > oldest_kept);
        self.last = Some(LastTurn {
            model: turn.model.clone(),
            at: turn.timestamp.clone(),
            key,
            tokens: turn.tokens,
        });
        true
    }

    /// What it used on `today` and since `today`'s Monday.
    pub fn day_and_week(&self, today: NaiveDate) -> (Ledger, Ledger) {
        span(&self.days, today)
    }

    /// The view the API serves.
    pub fn view(&self, today: NaiveDate) -> UsageView {
        let (day, week) = self.day_and_week(today);
        UsageView {
            turns: self.turns,
            total: self.total.cost(),
            today: day.cost(),
            week: week.cost(),
            models: self
                .total
                .entries()
                .iter()
                .map(|(key, tokens)| ModelUsage {
                    model: key.model.clone(),
                    label: key.label(),
                    tokens: *tokens,
                    cost_usd: key.cost(tokens),
                })
                .collect(),
            last_turn: self.last.as_ref().map(|last| LastTurnView {
                model: last.model.clone(),
                at: last.at.clone(),
                tokens: last.tokens,
                cost_usd: last.key.cost(&last.tokens),
            }),
        }
    }
}

/// `today`'s ledger and the week's (Monday to `today`) from day ledgers.
fn span(days: &BTreeMap<NaiveDate, Ledger>, today: NaiveDate) -> (Ledger, Ledger) {
    let mut day = Ledger::default();
    let mut week = Ledger::default();
    for (date, ledger) in days.range(week_start(today)..=today) {
        week.merge(ledger);
        if *date == today {
            day.merge(ledger);
        }
    }
    (day, week)
}

/// A session's usage in the API.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageView {
    /// API calls counted.
    pub turns: u64,
    /// All of it.
    #[serde(flatten)]
    pub total: Cost,
    pub today: Cost,
    pub week: Cost,
    /// By model (and billing tier).
    pub models: Vec<ModelUsage>,
    pub last_turn: Option<LastTurnView>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelUsage {
    pub model: String,
    /// With its billing tier (`(fast)`, `(long context)`).
    pub label: String,
    pub tokens: Tokens,
    /// `None`: no price for it.
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LastTurnView {
    pub model: String,
    pub at: Option<String>,
    pub tokens: Tokens,
    pub cost_usd: Option<f64>,
}

// --- removed sessions -----------------------------------------------------------

/// Usage of sessions that were removed, by workspace: what keeps the
/// day, week, and workspace rollups whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageArchive {
    #[serde(default)]
    pub total: BTreeMap<String, Ledger>,
    #[serde(default)]
    pub days: BTreeMap<NaiveDate, BTreeMap<String, Ledger>>,
}

impl UsageArchive {
    /// The archive at `path` (empty when absent or unreadable).
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        crate::auth::write_private(path, &bytes)
    }

    /// Add a removed session's usage (under `workspace_id` when it does
    /// not know its own).
    pub fn fold(&mut self, usage: &SessionUsage, workspace_id: &str) {
        let workspace = usage.workspace_id.as_deref().unwrap_or(workspace_id);
        if usage.total.is_empty() {
            return;
        }
        self.total
            .entry(workspace.to_owned())
            .or_default()
            .merge(&usage.total);
        for (day, ledger) in &usage.days {
            self.days
                .entry(*day)
                .or_default()
                .entry(workspace.to_owned())
                .or_default()
                .merge(ledger);
        }
        if let Some(newest) = self.days.keys().next_back().copied() {
            let oldest_kept = newest - chrono::Duration::days(KEEP_DAYS);
            self.days.retain(|d, _| *d > oldest_kept);
        }
    }
}

/// Today, this week, and all time, for one workspace or the whole hub.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UsageRollup {
    pub today: Cost,
    pub week: Cost,
    pub total: Cost,
}

/// Rollups being summed.
#[derive(Debug, Clone, Default)]
pub struct RollupBuilder {
    today: Ledger,
    week: Ledger,
    total: Ledger,
}

impl RollupBuilder {
    pub fn add_session(&mut self, usage: &SessionUsage, today: NaiveDate) {
        let (day, week) = usage.day_and_week(today);
        self.today.merge(&day);
        self.week.merge(&week);
        self.total.merge(&usage.total);
    }

    /// Add `workspace`'s share of the archive (`None`: every workspace's).
    pub fn add_archive(
        &mut self,
        archive: &UsageArchive,
        workspace: Option<&str>,
        today: NaiveDate,
    ) {
        let wanted = |id: &String| workspace.is_none_or(|w| w == id);
        for (id, ledger) in &archive.total {
            if wanted(id) {
                self.total.merge(ledger);
            }
        }
        for (date, by_workspace) in archive.days.range(week_start(today)..=today) {
            for (id, ledger) in by_workspace {
                if wanted(id) {
                    self.week.merge(ledger);
                    if *date == today {
                        self.today.merge(ledger);
                    }
                }
            }
        }
    }

    pub fn build(&self) -> UsageRollup {
        UsageRollup {
            today: self.today.cost(),
            week: self.week.cost(),
            total: self.total.cost(),
        }
    }
}

/// The hub's usage: everything, then by workspace (`GET /api/usage`).
#[derive(Debug, Clone, Serialize)]
pub struct HubUsage {
    #[serde(flatten)]
    pub hub: UsageRollup,
    pub workspaces: Vec<WorkspaceUsage>,
    /// The local date "today" means, and the Monday "week" starts on.
    pub day: NaiveDate,
    pub week_start: NaiveDate,
    pub price_source: &'static str,
    pub prices_as_of: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceUsage {
    pub workspace_id: String,
    pub name: String,
    #[serde(flatten)]
    pub usage: UsageRollup,
}

/// The archive's path in `state_dir`.
pub fn archive_path(state_dir: &Path) -> PathBuf {
    state_dir.join(ARCHIVE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    /// A short real transcript from claude 2.1.306 (`claude -p --model
    /// haiku`, one reply), trimmed to the lines that matter; Claude Code's
    /// own `cost-state` line said `totalCostUSD: 0.00443369`.
    const REAL_TRANSCRIPT: &str = include_str!("../tests/fixtures/claude-usage.jsonl");

    #[test]
    fn real_transcript_costs_what_claude_code_says() {
        let mut usage = SessionUsage::default();
        let mut claude_cost = None;
        for line in REAL_TRANSCRIPT.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            if value["type"] == "cost-state" {
                claude_cost = value["totalCostUSD"].as_f64();
            }
            if let Some(turn) = claude_turn(&value) {
                usage.observe(&turn);
            }
        }
        // Two lines of one response: counted once.
        assert_eq!(usage.turns, 1);
        let cost = usage.total.cost();
        assert_eq!(
            cost.tokens,
            Tokens {
                input: 2,
                output: 105,
                cache_read: 10579,
                cache_write_5m: 0,
                cache_write_1h: 21376,
            }
        );
        assert!(cost.priced);
        let expected = claude_cost.expect("fixture has Claude Code's own cost");
        assert!(
            (cost.cost_usd - expected).abs() < 1e-9,
            "{} vs {expected}",
            cost.cost_usd
        );
    }

    #[test]
    fn cost_math_covers_cache_ttls_tiers_and_fast_mode() {
        let tokens = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
        };
        // Opus 5.5: 4 + 20 + 0.20 + 5 + 8.
        let key = BillingKey::of("claude-opus-5-5", false, &Tokens::default());
        assert!(close(key.cost(&tokens).unwrap(), 37.2));
        // Fast mode doubles it.
        let fast = BillingKey::of("claude-opus-5-5", true, &Tokens::default());
        assert!(close(fast.cost(&tokens).unwrap(), 74.4));
        // A model without a fast price: unpriced, not guessed.
        assert_eq!(
            BillingKey::of("claude-opus-4-8", true, &Tokens::default()).cost(&tokens),
            None
        );
        // Haiku 5.5 over 100K prompt tokens is billed at the long rate.
        let small = Tokens {
            input: 50_000,
            output: 1_000,
            ..Tokens::default()
        };
        let big = Tokens {
            input: 150_000,
            output: 1_000,
            ..Tokens::default()
        };
        let small_key = BillingKey::of("claude-haiku-5-5", false, &small);
        let big_key = BillingKey::of("claude-haiku-5-5", false, &big);
        assert!(!small_key.long_context && big_key.long_context);
        assert!(close(small_key.cost(&small).unwrap(), 0.005 + 0.0005));
        assert!(close(big_key.cost(&big).unwrap(), 0.075 + 0.0025));
        assert_eq!(big_key.label(), "claude-haiku-5-5 (long context)");
        // Dated snapshot ids match; provider-prefixed ones do not.
        assert!(price_of("claude-haiku-4-5-20251001").is_some());
        assert!(price_of("claude-haiku-4-5-2025").is_none());
        assert!(price_of("anthropic.claude-haiku-4-5").is_none());
        assert!(price_of("claude-sonnet-5-5").is_some());
        // GPT-5: cached input at its own rate.
        let gpt = Tokens {
            input: 1_000_000,
            cache_read: 1_000_000,
            output: 1_000_000,
            ..Tokens::default()
        };
        assert!(close(
            BillingKey::of("gpt-5-codex", false, &gpt)
                .cost(&gpt)
                .unwrap(),
            1.25 + 0.125 + 10.0
        ));
    }

    fn assistant(id: &str, model: &str, ts: &str, input: u64, output: u64) -> serde_json::Value {
        json!({
            "type": "assistant",
            "timestamp": ts,
            "message": {
                "id": id,
                "model": model,
                "usage": {"input_tokens": input, "output_tokens": output, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 0},
            },
        })
    }

    #[test]
    fn repeats_replace_unknown_models_stay_unpriced_and_days_roll_up() {
        let mut usage = SessionUsage::default();
        let monday = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        assert_eq!(week_start(monday), monday);
        let ts = |d: u32, h: u32| {
            chrono::Local
                .with_ymd_and_hms(2026, 11, d, h, 0, 0)
                .unwrap()
                .to_rfc3339()
        };
        use chrono::TimeZone;
        let first = claude_turn(&assistant("m1", "claude-sonnet-5-5", &ts(2, 9), 10, 5)).unwrap();
        // No cache split: the write is a 5-minute one.
        assert_eq!(first.tokens.cache_write_5m, 100);
        assert!(usage.observe(&first));
        assert!(
            !usage.observe(&first),
            "the same line again changes nothing"
        );
        // A later line of the same response with more output replaces it.
        let grown = claude_turn(&assistant("m1", "claude-sonnet-5-5", &ts(2, 9), 10, 50)).unwrap();
        assert!(usage.observe(&grown));
        assert_eq!(usage.turns, 1);
        assert_eq!(usage.total.cost().tokens.output, 50);
        // Another model, unknown, on Wednesday.
        let other = claude_turn(&assistant("m2", "claude-next-9", &ts(4, 9), 1000, 10)).unwrap();
        usage.observe(&other);
        let wednesday = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let view = usage.view(wednesday);
        assert_eq!(view.turns, 2);
        assert!(!view.total.priced);
        assert_eq!(view.total.unpriced_models, ["claude-next-9"]);
        let sonnet = (10.0 * 2.0 + 100.0 * 2.0 * 1.25 + 50.0 * 10.0) / 1e6;
        assert!(close(view.total.cost_usd, sonnet), "priced part only");
        assert_eq!(view.today.tokens.input, 1000);
        assert_eq!(view.week.tokens.input, 1010);
        let model = view
            .models
            .iter()
            .find(|m| m.model == "claude-next-9")
            .unwrap();
        assert_eq!(model.cost_usd, None);
        assert_eq!(view.last_turn.unwrap().model, "claude-next-9");
        // Next Monday: a new week.
        let next = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        assert!(usage.view(next).week.tokens.is_zero());
        // Synthetic and zero-usage lines are not calls.
        assert!(claude_turn(&assistant("m3", "<synthetic>", &ts(4, 9), 1, 1)).is_none());
        assert!(claude_turn(&json!({"type": "user", "message": {"usage": {}}})).is_none());

        // Removed: it moves to the archive, which keeps the rollups.
        let mut archive = UsageArchive::default();
        archive.fold(&usage, "w1");
        let mut all = RollupBuilder::default();
        all.add_archive(&archive, None, wednesday);
        let mut none = RollupBuilder::default();
        none.add_archive(&archive, Some("w2"), wednesday);
        assert_eq!(all.build().week.tokens.input, 1010);
        assert!(none.build().total.tokens.is_zero());
        let dir = tempfile::tempdir().unwrap();
        let path = archive_path(dir.path());
        archive.save(&path).unwrap();
        assert_eq!(UsageArchive::load(&path), archive);
        // Saved and loaded, a session's usage is the same.
        let saved = serde_json::to_string(&usage).unwrap();
        assert_eq!(serde_json::from_str::<SessionUsage>(&saved).unwrap(), usage);
    }

    #[test]
    fn codex_counts_the_growth_of_its_totals() {
        let lines = [
            json!({"type": "session_meta", "payload": {"id": "t"}}),
            json!({"type": "event_msg", "timestamp": "2026-11-02T10:00:00Z", "payload": {"type": "token_count", "info": null}}),
            json!({"type": "turn_context", "payload": {"model": "gpt-5-codex"}}),
            json!({"type": "event_msg", "timestamp": "2026-11-02T10:00:01Z", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 1000, "cached_input_tokens": 400, "output_tokens": 50, "reasoning_output_tokens": 20, "total_tokens": 1050},
                "last_token_usage": {"input_tokens": 1000, "cached_input_tokens": 400, "output_tokens": 50},
            }}}),
            // Repeated event: nothing new.
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 1000, "cached_input_tokens": 400, "output_tokens": 50},
            }}}),
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 3000, "cached_input_tokens": 1400, "output_tokens": 80},
            }}}),
        ];
        let mut cursor = CodexCursor::default();
        let turns: Vec<Turn> = lines
            .iter()
            .filter_map(codex_line)
            .filter_map(|line| cursor.feed(line))
            .collect();
        assert_eq!(turns.len(), 2);
        assert_eq!(
            turns[0].tokens,
            Tokens {
                input: 600,
                cache_read: 400,
                output: 50,
                ..Tokens::default()
            }
        );
        assert_eq!(turns[0].model, "gpt-5-codex");
        assert_eq!(turns[1].tokens.input, 1000);
        assert_eq!(turns[1].tokens.cache_read, 1000);
        assert_eq!(turns[1].tokens.output, 30);
        // Without totals, the last call's counts; before any model, a
        // placeholder that is never priced.
        let mut fresh = CodexCursor::default();
        let only_last = json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
            "last_token_usage": {"input_tokens": 10, "output_tokens": 1},
        }}});
        let turn = fresh.feed(codex_line(&only_last).unwrap()).unwrap();
        assert_eq!(turn.model, CODEX_UNKNOWN_MODEL);
        assert!(price_of(&turn.model).is_none());
    }
}
