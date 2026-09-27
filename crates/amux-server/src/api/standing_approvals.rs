//! Standing approvals (AMUX-5270): answers the owner already gave, applied
//! without asking him again.
//!
//! # Why this exists
//!
//! 2026-09-27 04:24, two `amux alert` escalations reached Ethan's phone:
//!
//! 1. mixpeek-cicd, P0 prod 500 on duplicate uploads: the canary refused a paid
//!    lifecycle probe (a few cents) and asked for a go. He had ALREADY approved
//!    exactly that the day before, in gs-4-gke-minimization's decision #7
//!    ("standing approval for canary paid work: a few cents per engine release,
//!    capped at 4 calls a day"). That answer lived only in gs-4's terminal.
//! 2. mvs-infra, WAL divergence during a primary roll with both lineages
//!    preserved in GCS, asked go/no-go on the reconciliation.
//!
//! His reply: "this shouldn't need my input this should be fixed on its own".
//! An approval that only one lane can see is an approval every other lane will
//! ask for again. So the record lives here, is published into every worker's
//! MEMORY.md, and the two doors a lane uses to reach the owner (the fire alarm
//! and a `needsyou` card) consult it first.
//!
//! # Matching is deliberately dumb
//!
//! Category plus keyword overlap, plus the structured caps. Every verdict names
//! the approval and the overlapping words, so a wrong match is explainable from
//! the log line alone. It errs toward NOT matching: a miss costs the owner one
//! page he would have had anyway; a false match spends money or touches prod
//! data on an approval he never gave.
//!
//! Routes (merged at the root):
//! - GET    /api/approvals/standing[?session=<w>]  (anyone; `session` resolves scope)
//! - POST   /api/approvals/standing                (owner only)
//! - PATCH  /api/approvals/standing/{id}           (owner only; revoke or edit)
//! - DELETE /api/approvals/standing/{id}           (owner only; revokes, never erases)
//! - GET    /api/approvals/standing/uses           (the FYI feed)
//! - POST   /api/approvals/standing/check          (dry-run a match, records nothing)

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;

/// Kill switch. Default ON; process env wins, then worker > group > global,
/// the same ladder as every other scoped gate (`needsyou_ask_required`).
pub const KILL_SWITCH_KEY: &str = "AMUX_STANDING_APPROVALS";

/// The closed category vocabulary. Five of these are the `needsyou` ask types;
/// `prod_data` is the one the ask vocabulary lacks, and it is the category of
/// the second incident above.
pub const CATEGORIES: [&str; 6] = [
    "budget",
    "customer_outbound",
    "prod_data",
    "credential",
    "access",
    "decision",
];

/// Distinct content words an ask must share with the approval's sentence.
/// Two, not one: a single shared word ("production") is how an unrelated ask
/// would ride an approval it has nothing to do with.
pub const MIN_OVERLAP: usize = 2;

/// The line every worker reads, verbatim (the card's wording).
pub const COVERED_LINE: &str =
    "An ask covered here is already approved: act, then report on your card. Do not escalate it.";

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct StandingApproval {
    pub id: i64,
    pub title: String,
    pub allowed: String,
    pub category: String,
    pub limits: String,
    pub max_per_day: Option<i64>,
    pub max_amount_usd: Option<f64>,
    pub require_terms: Vec<String>,
    pub scope: String,
    pub granted_by: String,
    pub granted_at: i64,
    pub source: String,
    pub expires_at: Option<i64>,
    pub revoked: bool,
    pub revoked_at: Option<i64>,
    pub revoked_by: Option<String>,
}

impl StandingApproval {
    pub fn label(&self) -> String {
        format!("SA-{}", self.id)
    }

    pub fn is_active(&self, now: i64) -> bool {
        !self.revoked && self.expires_at.is_none_or(|e| e > now)
    }

    /// One line a lane can quote back: free text plus the structured caps.
    pub fn limits_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.limits.trim().is_empty() {
            parts.push(self.limits.trim().to_string());
        }
        if let Some(n) = self.max_per_day {
            parts.push(format!("max {n}/day"));
        }
        if let Some(a) = self.max_amount_usd {
            parts.push(format!("max ${a} per ask"));
        }
        if !self.require_terms.is_empty() {
            parts.push(format!(
                "only when the ask mentions one of: {}",
                self.require_terms.join(", ")
            ));
        }
        if parts.is_empty() {
            "no stated limit".into()
        } else {
            parts.join("; ")
        }
    }

    pub fn to_json(&self, now: i64) -> Value {
        json!({
            "id": self.label(),
            "num": self.id,
            "title": self.title,
            "allowed": self.allowed,
            "category": self.category,
            "limits": self.limits,
            "max_per_day": self.max_per_day,
            "max_amount_usd": self.max_amount_usd,
            "require_terms": self.require_terms,
            "limits_line": self.limits_line(),
            "scope": self.scope,
            "granted_by": self.granted_by,
            "granted_at": self.granted_at,
            "source": self.source,
            "expires_at": self.expires_at,
            "revoked": self.revoked,
            "revoked_at": self.revoked_at,
            "revoked_by": self.revoked_by,
            "active": self.is_active(now),
        })
    }
}

/// `SA-3`, `sa-3` or `3`.
pub fn parse_id(s: &str) -> Option<i64> {
    let t = s.trim();
    let t = t
        .strip_prefix("SA-")
        .or_else(|| t.strip_prefix("sa-"))
        .unwrap_or(t);
    t.parse().ok().filter(|n: &i64| *n > 0)
}

pub fn valid_scope(scope: &str) -> bool {
    let s = scope.trim();
    s == "global"
        || s.strip_prefix("group:").is_some_and(|g| !g.trim().is_empty())
        || s.strip_prefix("worker:").is_some_and(|w| !w.trim().is_empty())
}

/// Does an approval at `scope` cover `session`, a member of `groups`?
///
/// An anonymous caller (no session) is covered by `global` only: a scoped
/// approval was granted to someone in particular, and "we could not tell who
/// asked" is not them.
pub fn scope_applies(scope: &str, session: &str, groups: &[String]) -> bool {
    let s = scope.trim();
    if s == "global" {
        return true;
    }
    let session = session.trim();
    if session.is_empty() {
        return false;
    }
    if let Some(w) = s.strip_prefix("worker:") {
        return w.trim().eq_ignore_ascii_case(session);
    }
    if let Some(g) = s.strip_prefix("group:") {
        let g = g.trim();
        return groups.iter().any(|x| x.trim().eq_ignore_ascii_case(g));
    }
    false
}

// ---------------------------------------------------------------------------
// Matching (pure)
// ---------------------------------------------------------------------------

/// Words that carry no claim about WHAT is being asked. Includes the verbs of
/// asking itself ("approve", "proceed", "go"), which every ask and every
/// approval share and which would otherwise make everything overlap.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "per", "with", "when", "then", "than", "this", "that", "these", "those",
    "from", "into", "onto", "over", "most", "least", "few", "each", "both", "only", "just", "need",
    "needs", "needed", "ask", "asks", "asked", "asking", "want", "wants", "please", "approve",
    "approval", "approved", "proceed", "report", "card", "verify", "owner", "ethan", "day", "days",
    "today", "allow", "allowed", "can", "may", "will", "would", "should", "could", "our", "its",
    "their", "them", "they", "you", "your", "not", "yes", "all", "any", "some", "was", "were",
    "are", "has", "have", "had", "been", "being", "there", "here", "what", "which", "who", "why",
    "how", "about", "after", "before", "during", "while", "once", "get", "got", "run", "running",
    "one", "two", "does", "did", "done", "also", "still", "now", "via", "per", "a", "an", "is",
    "it", "on", "in", "of", "to", "or", "at", "by", "be", "as", "if", "so", "we", "i", "me", "my",
    "go", "no", "nogo", "state", "prior",
];

/// Category inference. A token matches a keyword when it STARTS WITH it, so
/// `reconcil` covers reconcile/reconciliation and `divergen` covers
/// divergence/divergent.
const CATEGORY_KEYWORDS: &[(&str, &[&str])] = &[
    (
        "budget",
        &[
            "paid", "pay", "cost", "cent", "spend", "budget", "billing", "bill", "price",
            "purchase", "buy", "dollar", "usd", "quota", "invoice", "subscription", "ticket",
        ],
    ),
    (
        "customer_outbound",
        &["email", "customer", "outbound", "publish", "tweet", "linkedin", "dm", "newsletter"],
    ),
    (
        "prod_data",
        &[
            "prod", "data", "database", "wal", "lineage", "reconcil", "restore", "snapshot",
            "backup", "migrat", "delet", "overwrit", "divergen", "shard", "replica", "rollback",
            "p0", "incident", "outage", "corrupt",
        ],
    ),
    (
        "credential",
        &["credential", "token", "secret", "password", "oauth", "apikey", "keychain"],
    ),
    (
        "access",
        &["access", "permission", "console", "iam", "role", "invite", "sudo"],
    ),
    ("decision", &["decide", "decision", "choose", "direction", "priorit", "tradeoff"]),
];

/// Crude stemmer: enough that probe/probes, lineage/lineages and
/// preserve/preserved compare equal (via [`tok_eq`]'s prefix rule).
fn stem(w: &str) -> String {
    let w = w.to_ascii_lowercase();
    for (suf, min, rep) in [("ies", 5, "y"), ("ing", 6, ""), ("ed", 5, ""), ("s", 4, "")] {
        if w.len() >= min && w.ends_with(suf) && !w.ends_with("ss") {
            return format!("{}{}", &w[..w.len() - suf.len()], rep);
        }
    }
    w
}

/// Equal, or one a prefix of the other with at least 4 characters in common
/// ("prod" ~ "production", "preserv" ~ "preserve").
fn tok_eq(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= 4 && long.starts_with(short)
}

/// Content tokens of `text`, stemmed, stopwords removed, first-seen order.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let lower = raw.to_ascii_lowercase();
        if STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        // Keep short tokens only when they mix letters and digits (p0, s3):
        // "go", "ok" and friends are noise, "p0" is a severity.
        let short_ok = lower.len() == 2
            && lower.chars().any(|c| c.is_ascii_digit())
            && lower.chars().any(|c| c.is_ascii_alphabetic());
        if lower.len() < 3 && !short_ok {
            continue;
        }
        if lower.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let s = stem(&lower);
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

pub fn infer_categories(text: &str) -> BTreeSet<&'static str> {
    let toks = tokens(text);
    let mut out = BTreeSet::new();
    for (cat, kws) in CATEGORY_KEYWORDS {
        if toks.iter().any(|t| kws.iter().any(|k| t.starts_with(k))) {
            out.insert(*cat);
        }
    }
    // A dollar figure is a spend whatever words surround it.
    if max_dollar_amount(text).is_some() {
        out.insert("budget");
    }
    out
}

/// The largest `$N` figure in `text` (`$749`, `$1,200`, `$0.05`).
pub fn max_dollar_amount(text: &str) -> Option<f64> {
    let mut best: Option<f64> = None;
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let mut j = i + 1;
            let mut num = String::new();
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b',' || b[j] == b'.') {
                if b[j] != b',' {
                    num.push(b[j] as char);
                }
                j += 1;
            }
            let num = num.trim_end_matches('.');
            if let Ok(v) = num.parse::<f64>() {
                best = Some(best.map_or(v, |x: f64| x.max(v)));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    best
}

#[derive(Clone, Debug, PartialEq)]
pub enum MatchVerdict {
    /// Covered: do not page, tell the lane to act.
    Applied {
        approval: StandingApproval,
        overlap: Vec<String>,
        uses_today: i64,
    },
    /// It WOULD be covered, but a structured cap is spent or exceeded, so the
    /// owner is asked exactly as before. `cap` names which cap.
    CapReached {
        approval: StandingApproval,
        overlap: Vec<String>,
        uses_today: i64,
        cap: String,
    },
    NoMatch {
        reason: String,
    },
}

impl MatchVerdict {
    /// The log verdict. Grep these.
    pub fn verdict(&self) -> &'static str {
        match self {
            Self::Applied { .. } => "standing_approval_applied",
            Self::CapReached { .. } => "standing_approval_cap_reached",
            Self::NoMatch { .. } => "standing_approval_no_match",
        }
    }

    pub fn applied(&self) -> Option<&StandingApproval> {
        match self {
            Self::Applied { approval, .. } => Some(approval),
            _ => None,
        }
    }

    /// The instruction a covered lane receives.
    pub fn instruction(&self) -> String {
        match self {
            Self::Applied { approval, .. } => format!(
                "Covered by standing approval {} ({}): {} Limits: {}. \
                 The owner was NOT paged. Act within those limits, then report what you did \
                 on your card. Do not escalate this again.",
                approval.label(),
                approval.title,
                approval.allowed.trim_end_matches('.').to_string() + ".",
                approval.limits_line()
            ),
            Self::CapReached { approval, cap, .. } => format!(
                "Standing approval {} ({}) would cover this, but {cap}, so this goes to the \
                 owner as before.",
                approval.label(),
                approval.title
            ),
            Self::NoMatch { reason } => format!("No standing approval covers this ({reason})."),
        }
    }

    pub fn to_json(&self, now: i64) -> Value {
        match self {
            Self::Applied {
                approval,
                overlap,
                uses_today,
            } => json!({
                "applied": true, "verdict": self.verdict(),
                "id": approval.label(), "approval": approval.to_json(now),
                "limits": approval.limits_line(), "matched_words": overlap,
                "uses_today": uses_today + 1, "max_per_day": approval.max_per_day,
                "instruction": self.instruction(),
            }),
            Self::CapReached {
                approval,
                overlap,
                uses_today,
                cap,
            } => json!({
                "applied": false, "verdict": self.verdict(),
                "id": approval.label(), "approval": approval.to_json(now),
                "limits": approval.limits_line(), "matched_words": overlap,
                "uses_today": uses_today, "max_per_day": approval.max_per_day,
                "cap": cap, "instruction": self.instruction(),
            }),
            Self::NoMatch { reason } => json!({
                "applied": false, "verdict": self.verdict(), "reason": reason,
            }),
        }
    }
}

/// Match an ask against the approvals. Pure: the per-day counter is injected.
///
/// An approval is a CANDIDATE when all of these hold:
/// 1. it is active (not revoked, not expired) and its scope covers `session`;
/// 2. its category is the ask's declared category or one inferred from the
///    ask's words (a "decision" ask about a WAL divergence is prod_data);
/// 3. if it lists `require_terms`, the ask contains one of them (the
///    precondition: "prior state preserved" must actually be claimed);
/// 4. the ask shares at least [`MIN_OVERLAP`] content words with the
///    approval's title and sentence.
///
/// The candidate with the most shared words wins. Its caps then decide
/// Applied versus CapReached.
pub fn match_ask(
    approvals: &[StandingApproval],
    text: &str,
    declared_category: Option<&str>,
    session: &str,
    groups: &[String],
    now: i64,
    uses_today: &dyn Fn(i64) -> i64,
) -> MatchVerdict {
    let ask_toks = tokens(text);
    let cats = infer_categories(text);
    let declared = declared_category
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty());
    let active: Vec<&StandingApproval> = approvals
        .iter()
        .filter(|a| a.is_active(now) && scope_applies(&a.scope, session, groups))
        .collect();
    if active.is_empty() {
        return MatchVerdict::NoMatch {
            reason: "no active standing approval is in scope for this lane".into(),
        };
    }
    let mut best: Option<(&StandingApproval, Vec<String>)> = None;
    let mut near: Vec<String> = Vec::new();
    for a in active {
        let cat_ok = declared.as_deref() == Some(a.category.as_str())
            || cats.contains(a.category.as_str());
        if !cat_ok {
            continue;
        }
        if !a.require_terms.is_empty() {
            let req_ok = a.require_terms.iter().any(|r| {
                let r = stem(r.trim());
                ask_toks.iter().any(|t| tok_eq(t, &r))
            });
            if !req_ok {
                near.push(format!("{}: precondition not stated", a.label()));
                continue;
            }
        }
        let appr_toks = tokens(&format!("{} {}", a.title, a.allowed));
        let overlap: Vec<String> = ask_toks
            .iter()
            .filter(|t| appr_toks.iter().any(|x| tok_eq(t, x)))
            .cloned()
            .collect();
        if overlap.len() < MIN_OVERLAP {
            near.push(format!(
                "{}: {} shared word(s), need {MIN_OVERLAP}",
                a.label(),
                overlap.len()
            ));
            continue;
        }
        if best.as_ref().is_none_or(|(_, o)| overlap.len() > o.len()) {
            best = Some((a, overlap));
        }
    }
    let Some((a, overlap)) = best else {
        let mut named: Vec<String> = cats.iter().map(|c| c.to_string()).collect();
        if let Some(d) = &declared {
            if !named.contains(d) {
                named.push(d.clone());
            }
        }
        return MatchVerdict::NoMatch {
            reason: if near.is_empty() {
                format!(
                    "no approval in category {}",
                    if named.is_empty() {
                        "(none inferred)".to_string()
                    } else {
                        named.join("/")
                    }
                )
            } else {
                near.join("; ")
            },
        };
    };
    let used = uses_today(a.id);
    if let (Some(cap), Some(amount)) = (a.max_amount_usd, max_dollar_amount(text)) {
        if amount > cap {
            return MatchVerdict::CapReached {
                approval: a.clone(),
                overlap,
                uses_today: used,
                cap: format!("the ask names ${amount}, above its ${cap} cap"),
            };
        }
    }
    if let Some(max) = a.max_per_day {
        if used >= max {
            return MatchVerdict::CapReached {
                approval: a.clone(),
                overlap,
                uses_today: used,
                cap: format!("its {max}/day cap is spent ({used} used in the last 24h)"),
            };
        }
    }
    MatchVerdict::Applied {
        approval: a.clone(),
        overlap,
        uses_today: used,
    }
}

// ---------------------------------------------------------------------------
// Kill switch
// ---------------------------------------------------------------------------

/// Pure resolver: `process_value` first, then the scoped value. Default ON.
pub fn enabled_from(process_value: Option<&str>, scoped: Option<&str>) -> bool {
    fn is_off(v: &str) -> bool {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    }
    if let Some(v) = process_value.filter(|v| !v.trim().is_empty()) {
        return !is_off(v);
    }
    scoped.is_none_or(|v| !is_off(v))
}

pub fn enabled(session: &str) -> bool {
    let process_value = std::env::var(KILL_SWITCH_KEY).ok();
    let scoped = if session.trim().is_empty() {
        None
    } else {
        crate::api::session_verbs::scoped_setting_in(
            &crate::api::session_verbs::home(),
            session,
            KILL_SWITCH_KEY,
        )
    };
    enabled_from(process_value.as_deref(), scoped.as_deref())
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

const COLS: &str = "id, title, allowed, category, limits, max_per_day, max_amount_usd, \
    require_terms, scope, granted_by, granted_at, source, expires_at, revoked, revoked_at, revoked_by";

fn row_to_approval(r: &rusqlite::Row<'_>) -> rusqlite::Result<StandingApproval> {
    let req: String = r.get(7)?;
    Ok(StandingApproval {
        id: r.get(0)?,
        title: r.get(1)?,
        allowed: r.get(2)?,
        category: r.get(3)?,
        limits: r.get(4)?,
        max_per_day: r.get(5)?,
        max_amount_usd: r.get(6)?,
        require_terms: split_terms(&req),
        scope: r.get(8)?,
        granted_by: r.get(9)?,
        granted_at: r.get(10)?,
        source: r.get(11)?,
        expires_at: r.get(12)?,
        revoked: r.get::<_, i64>(13)? != 0,
        revoked_at: r.get(14)?,
        revoked_by: r.get(15)?,
    })
}

fn split_terms(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

pub fn load_all(conn: &Connection) -> rusqlite::Result<Vec<StandingApproval>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM standing_approvals ORDER BY id"
    ))?;
    let rows = stmt.query_map([], row_to_approval)?;
    rows.collect()
}

pub fn load_one(conn: &Connection, id: i64) -> rusqlite::Result<Option<StandingApproval>> {
    conn.query_row(
        &format!("SELECT {COLS} FROM standing_approvals WHERE id=?1"),
        [id],
        row_to_approval,
    )
    .optional()
}

/// Applied uses in the rolling 24h before `now`. Rolling rather than calendar
/// day: "4 a day" read at 23:59 and 00:01 should not mean 8.
pub fn uses_in_last_day(conn: &Connection, approval_id: i64, now: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM standing_approval_uses \
         WHERE approval_id=?1 AND verdict='applied' AND ts > ?2",
        rusqlite::params![approval_id, now - 86_400],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Groups of `session`, from the same CC_TAGS reader the scope screen uses.
fn groups_of(session: &str) -> Vec<String> {
    if session.trim().is_empty() {
        return vec![];
    }
    crate::api::scope::session_tags_of(&crate::api::session_verbs::home(), session)
}

// ---------------------------------------------------------------------------
// The shared door: evaluate, record, log, FYI
// ---------------------------------------------------------------------------

/// Evaluate an ask arriving at `door` ("alert" | "needsyou").
///
/// Returns `None` when the kill switch is off for this lane, or the store
/// could not be read (both logged). `record` false is the dry run: nothing is
/// written and no FYI goes out.
pub async fn evaluate(
    state: &AppState,
    door: &str,
    session: &str,
    text: &str,
    declared_category: Option<&str>,
    reference: &str,
    record: bool,
) -> Option<MatchVerdict> {
    if !enabled(session) {
        tracing::info!(
            verdict = "standing_approval_disabled",
            door,
            session,
            "standing approvals are switched off for this lane ({KILL_SWITCH_KEY}=0); the ask goes to the owner"
        );
        return None;
    }
    let groups = groups_of(session);
    let (text_o, declared_o, session_o) = (
        text.to_string(),
        declared_category.map(str::to_string),
        session.to_string(),
    );
    let now = now_s();
    let verdict = match state
        .store
        .read_async(move |conn| {
            let all = load_all(conn)?;
            Ok(match_ask(
                &all,
                &text_o,
                declared_o.as_deref(),
                &session_o,
                &groups,
                now,
                &|id| uses_in_last_day(conn, id, now),
            ))
        })
        .await
    {
        Ok(v) => v,
        Err(e) => {
            // Fail toward the owner: an unreadable store must not silently
            // swallow an escalation.
            tracing::warn!(
                verdict = "standing_approval_unmeasured",
                door,
                session,
                error = %e,
                "standing approvals could not be read; the ask goes to the owner as before"
            );
            return None;
        }
    };
    match &verdict {
        MatchVerdict::Applied {
            approval, overlap, ..
        } => tracing::info!(
            verdict = verdict.verdict(),
            door,
            session,
            approval = %approval.label(),
            matched = %overlap.join(","),
            dry_run = !record,
            "standing approval answered an ask; the owner was not paged"
        ),
        MatchVerdict::CapReached { approval, cap, .. } => tracing::warn!(
            verdict = verdict.verdict(),
            door,
            session,
            approval = %approval.label(),
            cap = %cap,
            dry_run = !record,
            "standing approval matched but its cap is spent; the ask goes to the owner"
        ),
        MatchVerdict::NoMatch { reason } => tracing::info!(
            verdict = verdict.verdict(),
            door,
            session,
            reason = %reason,
            dry_run = !record,
            "no standing approval covers this ask"
        ),
    }
    if record {
        let row = match &verdict {
            MatchVerdict::Applied { approval, .. } => Some((approval.id, "applied")),
            MatchVerdict::CapReached { approval, .. } => Some((approval.id, "cap_reached")),
            MatchVerdict::NoMatch { .. } => None,
        };
        if let Some((aid, v)) = row {
            let (session, door, ask, reference) = (
                session.to_string(),
                door.to_string(),
                truncate(text, 600),
                reference.to_string(),
            );
            let res = state
                .store
                .write_async(move |conn| {
                    conn.execute(
                        "INSERT INTO standing_approval_uses (approval_id, ts, session, door, verdict, ask, reference) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![aid, now, session, door, v, ask, reference],
                    )?;
                    Ok(crate::db::WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .await;
            if let Err(e) = res {
                // The per-day cap reads this table, so a lost row is a cap
                // that undercounts. Loud.
                tracing::warn!(
                    verdict = "standing_approval_use_unrecorded",
                    approval = aid,
                    error = %e,
                    "standing approval use could not be recorded; its per-day count is now low by one"
                );
            }
        }
    }
    Some(verdict)
}

/// The owner's FYI line for an applied approval.
pub fn fyi_text(v: &MatchVerdict, session: &str, door: &str, ask: &str) -> Option<String> {
    let MatchVerdict::Applied {
        approval,
        uses_today,
        ..
    } = v
    else {
        return None;
    };
    let count = match approval.max_per_day {
        Some(m) => format!(" ({} of {m} today)", uses_today + 1),
        None => String::new(),
    };
    Some(format!(
        "{} auto-answered a {door} from {}{count}: {}",
        approval.label(),
        if session.is_empty() { "an unnamed caller" } else { session },
        truncate(ask, 160)
    ))
}

/// Low-priority owner FYI: a plain web push tagged `standing-approval-fyi`,
/// the channel the non-urgent owner digests use (AMUX-5240). No SMS, no email,
/// no "URGENT" prefix. The same entry is on the dashboard via
/// GET /api/approvals/standing/uses, which is the durable copy.
pub async fn send_fyi_push(state: &AppState, session: &str, text: &str) {
    let results =
        crate::push::send_all(state, "amux FYI", text, session, "standing-approval-fyi", "/")
            .await;
    match crate::api::alerts::push_delivery_verdict(&results) {
        Ok(()) => tracing::info!(
            verdict = "standing_approval_fyi_sent",
            session,
            "standing approval FYI pushed to the owner"
        ),
        Err(e) => tracing::info!(
            verdict = "standing_approval_fyi_undelivered",
            session,
            detail = %e,
            "standing approval FYI reached no push device; it is still on the dashboard feed"
        ),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/approvals/standing",
            axum::routing::get(list).post(create),
        )
        .route("/api/approvals/standing/uses", axum::routing::get(uses))
        .route("/api/approvals/standing/check", axum::routing::post(check))
        .route(
            "/api/approvals/standing/{id}",
            axum::routing::get(get_one).patch(patch).delete(revoke),
        )
}

/// Owner-only writes: the rule `grants.rs` and `/api/config/cross-group` use.
/// A request carrying a worker identity is a worker; so is a scoped (invited,
/// non-global) dashboard member. A lane that could write here would be
/// granting itself standing permission, which is the one thing this table
/// must never let it do.
fn refuse_non_owner(headers: &HeaderMap) -> Option<Response> {
    let worker = ["x-amux-session", "x-amux-worker"].iter().any(|h| {
        headers
            .get(*h)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| !s.trim().is_empty())
    });
    let scoped_member =
        super::org::local_member_scope(headers).is_some_and(|scope| !scope.is_global());
    if !worker && !scoped_member {
        return None;
    }
    tracing::warn!(
        verdict = "standing_approval_write_refused",
        caller = %super::alerts::hdr_worker(headers),
        "a non-owner tried to write a standing approval"
    );
    Some(
        (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": "only the owner may create or revoke a standing approval",
                "code": "standing_approval_owner_only",
                "why": "a standing approval pre-answers the owner's escalations; a lane writing one would be approving its own asks",
                "how": "the owner runs `amux approvals add --stdin` from his own shell (no AMUX_SESSION), or uses the dashboard",
            })),
        )
            .into_response(),
    )
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

#[derive(Deserialize)]
struct ListQuery {
    session: Option<String>,
    all: Option<String>,
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let now = now_s();
    let all = match state.store.read_async(|conn| Ok(load_all(conn)?)).await {
        Ok(v) => v,
        Err(e) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": e.to_string(), "why": "an empty list here would read as 'no approvals'"}),
            )
        }
    };
    let include_inactive = q.all.as_deref().is_some_and(|v| v == "1" || v == "true");
    let session = q.session.unwrap_or_default();
    let groups = groups_of(&session);
    let rows: Vec<Value> = all
        .iter()
        .filter(|a| include_inactive || a.is_active(now))
        .filter(|a| session.is_empty() || scope_applies(&a.scope, &session, &groups))
        .map(|a| a.to_json(now))
        .collect();
    Json(json!({
        "approvals": rows,
        "count": rows.len(),
        "total_recorded": all.len(),
        "session": if session.is_empty() { Value::Null } else { json!(session) },
        "resolved_scope": !session.is_empty(),
        "enabled": enabled(&session),
        "kill_switch": KILL_SWITCH_KEY,
        "note": if include_inactive { "includes revoked and expired" } else { "active only; ?all=1 includes revoked and expired" },
    }))
    .into_response()
}

async fn get_one(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let Some(n) = parse_id(&id) else {
        return err(StatusCode::BAD_REQUEST, json!({"error": "id must look like SA-3"}));
    };
    match state.store.read_async(move |c| Ok(load_one(c, n)?)).await {
        Ok(Some(a)) => Json(a.to_json(now_s())).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, json!({"error": "no such standing approval"})),
        Err(e) => err(StatusCode::SERVICE_UNAVAILABLE, json!({"error": e.to_string()})),
    }
}

fn str_field(b: &Value, k: &str) -> String {
    b.get(k)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// `max_per_day` / `max_amount_usd` may sit at the top level or inside a
/// `caps` object; either spelling is the same fact.
fn cap_field<'a>(b: &'a Value, k: &str) -> Option<&'a Value> {
    b.get(k)
        .or_else(|| b.get("caps").and_then(|c| c.get(k)))
        .filter(|v| !v.is_null())
}

fn terms_field(b: &Value) -> String {
    match b.get("require_terms") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::String(s)) => split_terms(s).join(","),
        _ => String::new(),
    }
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = refuse_non_owner(&headers) {
        return r;
    }
    let b = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let title = str_field(&b, "title");
    let allowed = str_field(&b, "allowed");
    let category = str_field(&b, "category").to_ascii_lowercase();
    let scope = {
        let s = str_field(&b, "scope");
        if s.is_empty() {
            "global".to_string()
        } else {
            s
        }
    };
    if title.is_empty() || allowed.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error": "title and allowed (the plain sentence of what is allowed) are required"}),
        );
    }
    if !CATEGORIES.contains(&category.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error": "unknown category", "allowed_categories": CATEGORIES}),
        );
    }
    if !valid_scope(&scope) {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error": "scope must be global, group:<g> or worker:<w>"}),
        );
    }
    let max_per_day = match cap_field(&b, "max_per_day") {
        None => None,
        Some(v) => match v.as_i64().filter(|n| *n >= 0) {
            Some(n) => Some(n),
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({"error": "max_per_day must be a non-negative integer"}),
                )
            }
        },
    };
    let max_amount_usd = match cap_field(&b, "max_amount_usd") {
        None => None,
        Some(v) => match v.as_f64().filter(|n| *n >= 0.0) {
            Some(n) => Some(n),
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({"error": "max_amount_usd must be a non-negative number"}),
                )
            }
        },
    };
    let expires_at = b.get("expires_at").and_then(Value::as_i64);
    let limits = str_field(&b, "limits");
    let source = str_field(&b, "source");
    let require_terms = terms_field(&b);
    let granted_by = {
        let g = str_field(&b, "granted_by");
        if g.is_empty() {
            "owner".to_string()
        } else {
            g
        }
    };
    let now = now_s();
    let new_id: Arc<Mutex<i64>> = Arc::new(Mutex::new(0));
    let slot = new_id.clone();
    let res = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO standing_approvals (title, allowed, category, limits, max_per_day, \
                 max_amount_usd, require_terms, scope, granted_by, granted_at, source, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    title,
                    allowed,
                    category,
                    limits,
                    max_per_day,
                    max_amount_usd,
                    require_terms,
                    scope,
                    granted_by,
                    now,
                    source,
                    expires_at
                ],
            )?;
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = conn.last_insert_rowid();
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    if let Err(e) = res {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()}));
    }
    let id = *new_id.lock().unwrap_or_else(|e| e.into_inner());
    match state.store.read_async(move |c| Ok(load_one(c, id)?)).await {
        Ok(Some(a)) => {
            tracing::info!(
                verdict = "standing_approval_created",
                approval = %a.label(),
                category = %a.category,
                scope = %a.scope,
                "standing approval recorded"
            );
            (StatusCode::CREATED, Json(json!({"ok": true, "approval": a.to_json(now)})))
                .into_response()
        }
        _ => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "inserted but could not be read back"}),
        ),
    }
}

/// PATCH: `{"revoked": true}` revokes; `expires_at`, `limits`, caps and
/// `scope` may be edited. `title`/`allowed`/`category` are NOT editable: a
/// different sentence is a different approval, so revoke and add.
async fn patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = refuse_non_owner(&headers) {
        return r;
    }
    let Some(n) = parse_id(&id) else {
        return err(StatusCode::BAD_REQUEST, json!({"error": "id must look like SA-3"}));
    };
    let b = body.map(|Json(v)| v).unwrap_or(Value::Null);
    for k in ["title", "allowed", "category"] {
        if b.get(k).is_some() {
            return err(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("{k} is not editable"), "how": "revoke this approval and add a new one"}),
            );
        }
    }
    if let Some(s) = b.get("scope").and_then(Value::as_str) {
        if !valid_scope(s) {
            return err(
                StatusCode::BAD_REQUEST,
                json!({"error": "scope must be global, group:<g> or worker:<w>"}),
            );
        }
    }
    let revoked = b.get("revoked").and_then(Value::as_bool);
    apply_patch(&state, n, b, revoked == Some(true)).await
}

async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(id): AxPath<String>,
) -> Response {
    if let Some(r) = refuse_non_owner(&headers) {
        return r;
    }
    let Some(n) = parse_id(&id) else {
        return err(StatusCode::BAD_REQUEST, json!({"error": "id must look like SA-3"}));
    };
    apply_patch(&state, n, json!({"revoked": true}), true).await
}

async fn apply_patch(state: &AppState, n: i64, b: Value, revoking: bool) -> Response {
    let now = now_s();
    let found: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let slot = found.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let exists: bool = conn
                .query_row("SELECT 1 FROM standing_approvals WHERE id=?1", [n], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = exists;
            if !exists {
                return Ok(crate::db::WriteOutcome {
                    applied: false,
                    events: vec![],
                });
            }
            if let Some(r) = b.get("revoked").and_then(Value::as_bool) {
                if r {
                    // Revoke is a flag, never a delete: the uses rows keep
                    // pointing at something that says what was approved.
                    conn.execute(
                        "UPDATE standing_approvals SET revoked=1, revoked_at=?2, revoked_by='owner' \
                         WHERE id=?1 AND revoked=0",
                        rusqlite::params![n, now],
                    )?;
                } else {
                    conn.execute(
                        "UPDATE standing_approvals SET revoked=0, revoked_at=NULL, revoked_by=NULL WHERE id=?1",
                        [n],
                    )?;
                }
            }
            if let Some(v) = b.get("expires_at") {
                conn.execute(
                    "UPDATE standing_approvals SET expires_at=?2 WHERE id=?1",
                    rusqlite::params![n, v.as_i64()],
                )?;
            }
            if let Some(v) = b.get("limits").and_then(Value::as_str) {
                conn.execute(
                    "UPDATE standing_approvals SET limits=?2 WHERE id=?1",
                    rusqlite::params![n, v.trim()],
                )?;
            }
            if let Some(v) = b.get("scope").and_then(Value::as_str) {
                conn.execute(
                    "UPDATE standing_approvals SET scope=?2 WHERE id=?1",
                    rusqlite::params![n, v.trim()],
                )?;
            }
            if let Some(v) = cap_field(&b, "max_per_day").or(b.get("max_per_day")) {
                conn.execute(
                    "UPDATE standing_approvals SET max_per_day=?2 WHERE id=?1",
                    rusqlite::params![n, v.as_i64()],
                )?;
            }
            if let Some(v) = cap_field(&b, "max_amount_usd").or(b.get("max_amount_usd")) {
                conn.execute(
                    "UPDATE standing_approvals SET max_amount_usd=?2 WHERE id=?1",
                    rusqlite::params![n, v.as_f64()],
                )?;
            }
            if b.get("require_terms").is_some() {
                conn.execute(
                    "UPDATE standing_approvals SET require_terms=?2 WHERE id=?1",
                    rusqlite::params![n, terms_field(&b)],
                )?;
            }
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    if let Err(e) = res {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()}));
    }
    if !*found.lock().unwrap_or_else(|e| e.into_inner()) {
        return err(StatusCode::NOT_FOUND, json!({"error": "no such standing approval"}));
    }
    match state.store.read_async(move |c| Ok(load_one(c, n)?)).await {
        Ok(Some(a)) => {
            tracing::info!(
                verdict = if revoking { "standing_approval_revoked" } else { "standing_approval_edited" },
                approval = %a.label(),
                "standing approval changed by the owner"
            );
            Json(json!({"ok": true, "approval": a.to_json(now)})).into_response()
        }
        _ => err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not read back"})),
    }
}

#[derive(Deserialize)]
struct UsesQuery {
    limit: Option<i64>,
    approval: Option<String>,
}

/// The FYI feed: every covered ask, newest first. Denominator included.
async fn uses(State(state): State<AppState>, Query(q): Query<UsesQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let only = q.approval.as_deref().and_then(parse_id);
    let res = state
        .store
        .read_async(move |conn| {
            let total: i64 = conn.query_row(
                "SELECT COUNT(*) FROM standing_approval_uses WHERE (?1 IS NULL OR approval_id=?1)",
                [only],
                |r| r.get(0),
            )?;
            let mut stmt = conn.prepare(
                "SELECT u.id, u.approval_id, u.ts, u.session, u.door, u.verdict, u.ask, u.reference, \
                        COALESCE(a.title, '') \
                 FROM standing_approval_uses u LEFT JOIN standing_approvals a ON a.id = u.approval_id \
                 WHERE (?1 IS NULL OR u.approval_id=?1) ORDER BY u.ts DESC, u.id DESC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![only, limit], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "approval": format!("SA-{}", r.get::<_, i64>(1)?),
                        "ts": r.get::<_, i64>(2)?,
                        "session": r.get::<_, String>(3)?,
                        "door": r.get::<_, String>(4)?,
                        "verdict": r.get::<_, String>(5)?,
                        "ask": r.get::<_, String>(6)?,
                        "reference": r.get::<_, String>(7)?,
                        "approval_title": r.get::<_, String>(8)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((total, rows))
        })
        .await;
    match res {
        Ok((total, rows)) => Json(json!({
            "uses": rows, "shown": rows.len(), "total": total,
            "truncated": (rows.len() as i64) < total,
        }))
        .into_response(),
        Err(e) => err(StatusCode::SERVICE_UNAVAILABLE, json!({"error": e.to_string()})),
    }
}

/// Dry-run a match: `{"text": "...", "category": "...", "session": "..."}`.
/// Records nothing and sends nothing, so a lane (or the owner) can ask "would
/// this be covered" without spending a use.
async fn check(State(state): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let b = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let text = str_field(&b, "text");
    if text.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({"error": "text is required"}));
    }
    let session = {
        let s = str_field(&b, "session");
        if s.is_empty() {
            super::alerts::hdr_worker(&headers)
        } else {
            s
        }
    };
    let cat = str_field(&b, "category");
    let cat = (!cat.is_empty()).then_some(cat);
    match evaluate(&state, "check", &session, &text, cat.as_deref(), "", false).await {
        Some(v) => Json(json!({"dry_run": true, "standing_approval": v.to_json(now_s())})).into_response(),
        None => Json(json!({
            "dry_run": true,
            "standing_approval": {"applied": false, "verdict": "standing_approval_disabled_or_unmeasured"},
        }))
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// MEMORY.md section
// ---------------------------------------------------------------------------

/// The "Standing approvals" section of the composed worker memory.
///
/// Read with a read-only connection because the composer is synchronous and
/// holds no `AppState`. `None` from the store means UNMEASURED, and it says so:
/// a missing section and a failed read must not look the same (AF-320).
pub fn memory_section(session: &str) -> String {
    let path = match std::env::var("AMUX_DB") {
        Ok(p) if !p.trim().is_empty() => std::path::PathBuf::from(p.trim()),
        _ => crate::config::amux_home().join("amux.db"),
    };
    let loaded = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .ok()
        .and_then(|c| load_all(&c).ok());
    memory_section_from(
        loaded.as_deref(),
        session,
        &groups_of(session),
        enabled(session),
        now_s(),
    )
}

/// The renderer with its inputs injected (the `credential_preflight_from`
/// seam): every branch is reachable from a test without a live store.
pub fn memory_section_from(
    approvals: Option<&[StandingApproval]>,
    session: &str,
    groups: &[String],
    enabled: bool,
    now: i64,
) -> String {
    const HEAD: &str = "\n## Standing approvals (auto-generated, do not edit)\n\n";
    let Some(all) = approvals else {
        return format!(
            "{HEAD}UNMEASURED: the approvals table could not be read, so nothing here is pre-approved. \
             Check `amux approvals ls` before escalating.\n"
        );
    };
    if !enabled {
        return format!(
            "{HEAD}Switched off for this lane ({KILL_SWITCH_KEY}=0). Escalations go to the owner as before.\n"
        );
    }
    let rows: Vec<&StandingApproval> = all
        .iter()
        .filter(|a| a.is_active(now) && scope_applies(&a.scope, session, groups))
        .collect();
    if rows.is_empty() {
        return format!(
            "{HEAD}None recorded for this lane. Stated rather than left silent: an empty section \
             would look the same as a check that never ran.\n"
        );
    }
    let mut out = format!(
        "{HEAD}{COVERED_LINE}\n\n\
         The owner already answered these. `amux alert` and `needsyou` check them for you and \
         answer on his behalf; `amux approvals ls` is the live list. This file is shared by every \
         lane in this directory, so a `worker:` or `group:` row covers only the lanes it names.\n\n\
         | id | category | scope | what is allowed | limits | source |\n|---|---|---|---|---|---|\n"
    );
    for a in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            a.label(),
            a.category,
            a.scope,
            a.allowed.replace('|', "/"),
            a.limits_line().replace('|', "/"),
            if a.source.is_empty() { "-".to_string() } else { a.source.replace('|', "/") },
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The two seeds from AMUX-5270, exactly as the owner will POST them.
    pub(crate) fn seed_bodies() -> [Value; 2] {
        [
            json!({
                "title": "Paid canary/lifecycle probes",
                "allowed": "Paid canary/lifecycle probes: a few cents per engine or platform release, at most 4 per day",
                "category": "budget",
                "limits": "a few cents per engine or platform release",
                "max_per_day": 4,
                "max_amount_usd": 1.0,
                "scope": "global",
                "granted_by": "ethan",
                "source": "gs-4-gke-minimization decision #7, 2026-09-26 (Ethan answered \"defaults\")",
            }),
            json!({
                "title": "P0 remediation with preserved prior state",
                "allowed": "P0 production incident remediation when the prior state is preserved and recoverable (snapshots, both lineages, backups): proceed, verify, then report on the card",
                "category": "prod_data",
                "limits": "only when the prior state is preserved and recoverable",
                "require_terms": ["preserved", "recoverable", "snapshot", "lineage", "backup"],
                "scope": "global",
                "granted_by": "ethan",
                "source": "Ethan 2026-09-27 04:24 (\"this shouldn't need my input\")",
            }),
        ]
    }

    fn seeded() -> Vec<StandingApproval> {
        let [a, b] = seed_bodies();
        let mk = |id: i64, v: &Value| StandingApproval {
            id,
            title: str_field(v, "title"),
            allowed: str_field(v, "allowed"),
            category: str_field(v, "category"),
            limits: str_field(v, "limits"),
            max_per_day: v.get("max_per_day").and_then(Value::as_i64),
            max_amount_usd: v.get("max_amount_usd").and_then(Value::as_f64),
            require_terms: split_terms(&terms_field(v)),
            scope: str_field(v, "scope"),
            granted_by: str_field(v, "granted_by"),
            granted_at: 0,
            source: str_field(v, "source"),
            expires_at: None,
            revoked: false,
            revoked_at: None,
            revoked_by: None,
        };
        vec![mk(1, &a), mk(2, &b)]
    }

    /// The two real escalations of 2026-09-27, as the lanes worded them.
    pub(crate) const CICD_ESCALATION: &str = "P0 prod 500 on duplicate uploads. The canary refused a paid lifecycle probe (a few cents) and needs a go to run it against the new engine release.";
    pub(crate) const MVS_ESCALATION: &str = "MVS WAL divergence during a primary roll; both lineages preserved in GCS. Need go/no-go on a reconciliation.";

    fn m(text: &str, cat: Option<&str>, session: &str, used: i64) -> MatchVerdict {
        match_ask(&seeded(), text, cat, session, &[], 1000, &|_| used)
    }

    #[test]
    fn both_real_escalations_are_covered() {
        let v = m(CICD_ESCALATION, None, "mixpeek-cicd", 0);
        assert_eq!(v.verdict(), "standing_approval_applied", "{v:?}");
        assert_eq!(v.applied().unwrap().id, 1, "the canary ask is the budget approval: {v:?}");

        let v = m(MVS_ESCALATION, Some("decision"), "mvs-infra", 0);
        assert_eq!(v.verdict(), "standing_approval_applied", "{v:?}");
        assert_eq!(v.applied().unwrap().id, 2, "{v:?}");
        assert!(v.instruction().contains("SA-2") && v.instruction().contains("NOT paged"));
    }

    #[test]
    fn controls_do_not_match() {
        for (text, cat) in [
            ("buy the $749 Advertising Week pass", Some("budget")),
            ("email the customer", Some("customer_outbound")),
            ("email the customer about the canary probe results", None),
            ("delete the production customer table to reclaim disk", Some("decision")),
            ("raise the GKE quota, it bills about $200 a month", Some("budget")),
            ("Should we prioritise the Studio redesign over MVS work?", Some("decision")),
            // Shares words with SA-2 but never claims the prior state survives.
            ("P0 production incident: drop the corrupted shard and rebuild it", None),
        ] {
            let v = m(text, cat, "some-lane", 0);
            assert_eq!(v.verdict(), "standing_approval_no_match", "{text:?} -> {v:?}");
        }
    }

    #[test]
    fn an_amount_above_the_cap_goes_to_the_owner() {
        let v = m("run the paid canary lifecycle probe, it costs $749", None, "x", 0);
        assert_eq!(v.verdict(), "standing_approval_cap_reached", "{v:?}");
        let v = m("run the paid canary lifecycle probe, it costs $0.04", None, "x", 0);
        assert_eq!(v.verdict(), "standing_approval_applied", "{v:?}");
    }

    #[test]
    fn per_day_cap_counts_uses() {
        assert_eq!(m(CICD_ESCALATION, None, "x", 3).verdict(), "standing_approval_applied");
        let v = m(CICD_ESCALATION, None, "x", 4);
        assert_eq!(v.verdict(), "standing_approval_cap_reached");
        assert!(v.instruction().contains("4/day"), "{}", v.instruction());
    }

    #[test]
    fn revoked_and_expired_never_match() {
        let mut a = seeded();
        a[0].revoked = true;
        a[1].expires_at = Some(999);
        for t in [CICD_ESCALATION, MVS_ESCALATION] {
            let v = match_ask(&a, t, None, "x", &[], 1000, &|_| 0);
            assert_eq!(v.verdict(), "standing_approval_no_match", "{t}");
        }
    }

    #[test]
    fn scope_resolution() {
        assert!(scope_applies("global", "", &[]));
        assert!(scope_applies("global", "anyone", &[]));
        assert!(scope_applies("worker:mvs-infra", "mvs-infra", &[]));
        assert!(!scope_applies("worker:mvs-infra", "mvs-pitr", &[]));
        assert!(!scope_applies("worker:mvs-infra", "", &[]), "anonymous gets global only");
        let g = vec!["mvs".to_string(), "ops".to_string()];
        assert!(scope_applies("group:MVS", "mvs-pitr", &g));
        assert!(!scope_applies("group:gtm", "mvs-pitr", &g));
        assert!(!scope_applies("group:mvs", "", &g));
        assert!(!scope_applies("bogus", "x", &g));
        assert!(valid_scope("group:ops") && valid_scope("worker:a") && valid_scope("global"));
        assert!(!valid_scope("group:") && !valid_scope("everyone"));

        // A worker-scoped approval covers its worker and nobody else.
        let mut a = seeded();
        a[0].scope = "worker:mixpeek-cicd".into();
        let hit = match_ask(&a, CICD_ESCALATION, None, "mixpeek-cicd", &[], 1000, &|_| 0);
        assert_eq!(hit.verdict(), "standing_approval_applied");
        let miss = match_ask(&a, CICD_ESCALATION, None, "gtm-engine", &[], 1000, &|_| 0);
        assert_ne!(miss.applied().map(|x| x.id), Some(1));
    }

    #[test]
    fn kill_switch_ladder() {
        assert!(enabled_from(None, None), "default on");
        assert!(!enabled_from(None, Some("0")));
        assert!(!enabled_from(Some("off"), Some("1")), "process env wins");
        assert!(enabled_from(Some("1"), Some("0")));
        assert!(enabled_from(Some(" "), None), "blank process value falls through");
    }

    #[test]
    fn dollar_amounts() {
        assert_eq!(max_dollar_amount("buy the $749 pass"), Some(749.0));
        assert_eq!(max_dollar_amount("$1,200 or $0.05."), Some(1200.0));
        assert_eq!(max_dollar_amount("a few cents"), None);
    }

    #[test]
    fn memory_section_every_branch() {
        let a = seeded();
        let s = memory_section_from(Some(&a), "mixpeek-cicd", &[], true, 1000);
        assert!(s.contains("## Standing approvals (auto-generated, do not edit)"));
        assert!(s.contains(COVERED_LINE));
        assert!(s.contains("| SA-1 | budget | global |") && s.contains("max 4/day"), "{s}");
        // Rendered bytes, not source: no line may start indented (AMUX-3810).
        assert!(s.lines().all(|l| !l.starts_with(' ')), "{s}");
        assert!(memory_section_from(None, "x", &[], true, 0).contains("UNMEASURED"));
        assert!(memory_section_from(Some(&a), "x", &[], false, 0).contains("Switched off"));
        assert!(memory_section_from(Some(&[]), "x", &[], true, 0).contains("None recorded"));
    }

    #[test]
    fn owner_only_writes() {
        let mut h = HeaderMap::new();
        assert!(refuse_non_owner(&h).is_none(), "no identity = the owner's dashboard/shell");
        h.insert("x-amux-session", "mixpeek-cicd".parse().unwrap());
        assert!(refuse_non_owner(&h).is_some());
        let mut h2 = HeaderMap::new();
        h2.insert("x-amux-worker", "gs-4".parse().unwrap());
        assert!(refuse_non_owner(&h2).is_some());
        let mut h3 = HeaderMap::new();
        h3.insert("x-amux-session", "  ".parse().unwrap());
        assert!(refuse_non_owner(&h3).is_none(), "blank is absent");
    }

    // ---- through the router, against a hermetic store --------------------

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    pub(crate) fn test_state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("standing-test.db")).unwrap();
        std::mem::forget(dir);
        AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub(crate) async fn call(
        app: &Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let st = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Seeds both approvals into a HERMETIC store via the real POST handler.
    pub(crate) async fn seed(app: &Router) {
        for b in seed_bodies() {
            let (st, v) = call(app, "POST", "/api/approvals/standing", &[], Some(b)).await;
            assert_eq!(st, StatusCode::CREATED, "{v}");
        }
    }

    #[tokio::test]
    async fn api_create_list_revoke_and_owner_gate() {
        let dir = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(dir.path());
        std::env::remove_var(KILL_SWITCH_KEY);
        let app = routes().with_state(test_state());

        // A worker may not create.
        let (st, v) = call(
            &app,
            "POST",
            "/api/approvals/standing",
            &[("x-amux-session", "gs-4")],
            Some(seed_bodies()[0].clone()),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(v["code"], "standing_approval_owner_only");

        seed(&app).await;
        let (st, v) = call(&app, "GET", "/api/approvals/standing", &[("x-amux-session", "gs-4")], None).await;
        assert_eq!(st, StatusCode::OK, "a worker may read");
        assert_eq!(v["count"], 2);
        assert_eq!(v["approvals"][0]["id"], "SA-1");
        assert_eq!(v["approvals"][0]["max_per_day"], 4);

        // A worker may not revoke; the owner may.
        let (st, _) = call(&app, "DELETE", "/api/approvals/standing/SA-1", &[("x-amux-worker", "gs-4")], None).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, v) = call(&app, "DELETE", "/api/approvals/standing/SA-1", &[], None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["approval"]["revoked"], true);
        let (_, v) = call(&app, "GET", "/api/approvals/standing", &[], None).await;
        assert_eq!(v["count"], 1, "revoked drops out of the active list");
        let (_, v) = call(&app, "GET", "/api/approvals/standing?all=1", &[], None).await;
        assert_eq!(v["count"], 2, "but is still recorded");

        // Bad category is refused.
        let (st, _) = call(
            &app,
            "POST",
            "/api/approvals/standing",
            &[],
            Some(json!({"title": "t", "allowed": "a", "category": "vibes"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn evaluate_records_uses_and_the_cap_bites_on_the_fifth() {
        let dir = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(dir.path());
        std::env::remove_var(KILL_SWITCH_KEY);
        let state = test_state();
        let app = routes().with_state(state.clone());
        seed(&app).await;
        for i in 0..4 {
            let v = evaluate(&state, "alert", "mixpeek-cicd", CICD_ESCALATION, None, "", true)
                .await
                .unwrap();
            assert_eq!(v.verdict(), "standing_approval_applied", "use {i}");
        }
        let v = evaluate(&state, "alert", "mixpeek-cicd", CICD_ESCALATION, None, "", true)
            .await
            .unwrap();
        assert_eq!(v.verdict(), "standing_approval_cap_reached");
        // Dry run records nothing.
        let _ = evaluate(&state, "check", "x", MVS_ESCALATION, None, "", false).await;
        let (_, u) = call(&app, "GET", "/api/approvals/standing/uses", &[], None).await;
        assert_eq!(u["total"], 5, "4 applied + 1 cap_reached, dry run not counted: {u}");
        assert_eq!(u["uses"][0]["verdict"], "cap_reached");

        // Kill switch: off means not consulted at all.
        std::env::set_var(KILL_SWITCH_KEY, "0");
        assert!(evaluate(&state, "alert", "x", MVS_ESCALATION, None, "", true).await.is_none());
        std::env::remove_var(KILL_SWITCH_KEY);
    }
}
