//! Named clearance signals (AMUX-5237).
//!
//! A card can wait on a NAMED signal, and any lane can raise that signal with
//! one call. Raising it records who raised it and when, clears the wait on
//! every card parked on that name, and wakes each owning lane through the
//! normal steering queue with a short message naming the signal, the raiser
//! and the note.
//!
//! WHY THIS EXISTS. Two measured stalls in the 2026-09-26 fleet review
//! (docs/fleet-stall-review-2026-09-26.md, finding 4):
//!
//! - gs-3-bucket-objects waited on mvs-infra's "30-minute clearance" by
//!   regex-polling mvs-infra's peek history, and the regex fired a false
//!   positive. A pane is prose and a clearance is a fact; scraping one for the
//!   other has no correct implementation.
//! - mixpeek-frustrations held ~145 cards for a peer approval that
//!   mixpeek-general had already given, in prose, in a message nothing parsed.
//!
//! Both are the same shape: a lane that genuinely cannot proceed until another
//! actor does something, and no durable place for "it is done" to land.
//!
//! THE WAIT IS STORED IN `blocked_on`, AS `signal:<name>`. No new column. The
//! field already means "this card is waiting for something", and every
//! consumer that matters already honours it: board-drive pickup and backlog
//! drain skip it, `ready` excludes it, a blocked `doing` card is parked to
//! backlog so it stops holding WIP. A second field would have needed every one
//! of those consumers taught about it, and the one that was missed would have
//! dispatched a waiting card. The only consumer that must treat it differently
//! is blocker recovery, which exists to talk a lane OUT of free-text peer
//! waits; a named signal has its own clearing mechanism, so recovery skips it
//! (see `board_drive::blocker_recoveries_with_policy`).
//!
//! THE WAIT IS ALWAYS ON THE WAITER'S OWN CARD. Workers must not create
//! cross-worker `depends_on` edges (docs/worker-owned-board-contract.md), and
//! this does not reopen that door: a worker may park only its own card on a
//! signal (enforced in `board::patch_item`). The raiser names a SIGNAL, never a
//! card, so raising cannot touch any card nobody chose to park on that name.
//!
//! A signal is for an event another actor produces (a clearance, an approval,
//! an external window closing). It is not a way to wait on missing
//! implementation you could build yourself; the board contract still says to
//! own that.

use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::db::board_store as bs;
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};

/// The `blocked_on` prefix that marks a named signal wait.
pub(crate) const SIGNAL_PREFIX: &str = "signal:";

/// Kill switch for the wake message (worker > group > global, process env
/// wins). Default ON. Scoped to the RECIPIENT lane: the owner of a lane that
/// should not be woken turns it off there. Clearing the wait is not gated:
/// that is the meaning of raising the signal, and a wait that a raise cannot
/// clear would leave the card parked forever.
pub(crate) const WAKE_KEY: &str = "AMUX_SIGNAL_WAKE";

/// The steering guard wake messages carry. Non-empty on purpose: it makes the
/// wake an AUTOMATED producer, so the steering chokepoint refuses it into a
/// paused or isolated lane exactly as it refuses every other automation.
pub(crate) const WAKE_GUARD: &str = "signal-wake";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_signals))
        .route("/{name}", get(get_signal).post(raise_signal))
}

/// A signal name: 1-64 chars of `[a-z0-9._-]`, starting with a letter or
/// digit. Lowercase only, so `MVS-Reflip-Clear` and `mvs-reflip-clear` cannot
/// be two signals that each think the other was never raised.
pub(crate) fn valid_signal_name(name: &str) -> bool {
    let n = name.len();
    (1..=64).contains(&n)
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// Normalise a caller-supplied name: trimmed and lowercased. Validation runs
/// on the result, so the canonical form is the only one ever stored.
pub(crate) fn normalize_signal_name(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// What a `blocked_on` value says about signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SignalWait {
    /// Ordinary free-text block (or empty): not a signal wait.
    NotASignal,
    /// Starts with `signal:` but the name is unusable. Refused at write time,
    /// because a malformed name is a wait no raise could ever clear.
    Invalid(String),
    /// A well-formed wait on this canonical name.
    Valid(String),
}

pub(crate) fn parse_signal_wait(blocked_on: &str) -> SignalWait {
    let t = blocked_on.trim();
    let Some(rest) = t
        .get(..SIGNAL_PREFIX.len())
        .filter(|p| p.eq_ignore_ascii_case(SIGNAL_PREFIX))
        .map(|_| &t[SIGNAL_PREFIX.len()..])
    else {
        return SignalWait::NotASignal;
    };
    let name = normalize_signal_name(rest);
    if valid_signal_name(&name) {
        SignalWait::Valid(name)
    } else {
        SignalWait::Invalid(name)
    }
}

/// The signal a card is waiting on, if its `blocked_on` is a valid signal wait.
pub(crate) fn signal_wait_name(blocked_on: Option<&str>) -> Option<String> {
    match parse_signal_wait(blocked_on.unwrap_or("")) {
        SignalWait::Valid(name) => Some(name),
        _ => None,
    }
}

/// The own-card rule, pure. `caller` is the verified worker lane ("" for the
/// owner or a verified local member, who may park any card). A worker may
/// park only a card on its own board.
pub(crate) fn wait_owner_refusal(caller: &str, card_session: Option<&str>) -> Option<Value> {
    if caller.is_empty() || card_session == Some(caller) {
        return None;
    }
    Some(json!({
        "error": "a worker may wait on a signal only on its OWN card",
        "code": "signal_wait_not_own_card",
        "caller": caller,
        "card_owner": card_session,
        "how_to_fix": "park the card on your own board that needs the signal (amux signal wait <your-card> <name>). \
                       Waiting on another worker's card is the cross-worker dependency edge the board contract forbids.",
    }))
}

/// The wake message a lane receives. Short on purpose: it has to be read
/// mid-flight by a lane that was parked, and it names the three facts that
/// lane needs (which signal, who, what they said) plus the cards it frees.
pub(crate) fn wake_message(
    signal: &str,
    raiser: &str,
    raised_at_iso: &str,
    note: &str,
    cards: &[(String, String)],
) -> String {
    let who = if raiser.is_empty() {
        "the owner"
    } else {
        raiser
    };
    let note_line = if note.trim().is_empty() {
        "No note was given.".to_string()
    } else {
        format!("Note: {}", note.trim())
    };
    let list = cards
        .iter()
        .map(|(id, title)| format!("- {id}: {title}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "[amux signal] `{signal}` was raised by {who} at {raised_at_iso}.\n{note_line}\n\n\
         The wait on {n} of your card(s) is cleared (blocked_on emptied):\n{list}\n\n\
         Pick the work back up (amux board doing <ID>). If this raise is not the event you were \
         waiting for, park the card again with amux signal wait <ID> {signal}.",
        n = cards.len(),
    )
}

/// Resolve [`WAKE_KEY`] for one recipient lane. Same ladder as
/// `board_drive::dispatch_backlog_when_idle_in`: process env wins, then
/// worker > group > global scope files, default ON.
pub(crate) fn wake_enabled_in(home: &std::path::Path, lane: &str, process: Option<&str>) -> bool {
    fn is_off(v: &str) -> bool {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    }
    if let Some(v) = process.filter(|v| !v.trim().is_empty()) {
        return !is_off(v);
    }
    crate::api::session_verbs::scoped_setting_in(home, lane, WAKE_KEY)
        .as_deref()
        .map(|v| !is_off(v))
        .unwrap_or(true)
}

fn wake_enabled(lane: &str) -> bool {
    let process = std::env::var(WAKE_KEY).ok();
    wake_enabled_in(&crate::api::session_verbs::home(), lane, process.as_deref())
}

fn actor_of(headers: &HeaderMap) -> String {
    super::org::local_member_actor(headers)
        .map(str::to_string)
        .unwrap_or_else(|| crate::api::groups::hdr_worker(headers))
        .trim()
        .chars()
        .take(64)
        .collect()
}

fn reply(code: StatusCode, v: Value) -> Response {
    (code, Json(v)).into_response()
}

fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| ts.to_string())
}

pub(crate) fn ensure_signal_table(conn: &Connection) -> rusqlite::Result<()> {
    // The migration creates it; this keeps a store opened by an older binary's
    // test fixture (or a DB restored from before 0088) from 500ing the verb.
    conn.execute_batch(include_str!("../../migrations/0088_board_signals.sql"))
}

/// Cards currently parked on `name`: live (not deleted, archived or terminal).
fn waiting_cards(conn: &Connection, name: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM issues WHERE lower(trim(blocked_on)) = ?1 AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND status NOT IN ('done','verified','discarded') \
         ORDER BY id",
    )?;
    let rows = stmt.query_map([format!("{SIGNAL_PREFIX}{name}")], |r| {
        r.get::<_, String>(0)
    })?;
    rows.collect()
}

/// Every live signal wait, grouped by name.
fn all_waits(conn: &Connection) -> rusqlite::Result<BTreeMap<String, Vec<Value>>> {
    let mut stmt = conn.prepare(
        "SELECT id, COALESCE(session,''), title, blocked_on FROM issues \
         WHERE lower(trim(blocked_on)) LIKE 'signal:%' AND deleted IS NULL \
         AND COALESCE(archived,0)=0 AND status NOT IN ('done','verified','discarded') ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in rows {
        let (id, session, title, blocked) = row?;
        if let Some(name) = signal_wait_name(Some(&blocked)) {
            out.entry(name)
                .or_default()
                .push(json!({"card": id, "session": session, "title": title}));
        }
    }
    Ok(out)
}

fn raise_rows(conn: &Connection, name: Option<&str>, limit: i64) -> rusqlite::Result<Vec<Value>> {
    let sql =
        "SELECT id, name, raised_by, raised_at, COALESCE(note,''), cleared FROM board_signals \
               WHERE (?1 IS NULL OR name = ?1) ORDER BY raised_at DESC, id DESC LIMIT ?2";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![name, limit], |r| {
        let cleared: String = r.get(5)?;
        let raised_at: i64 = r.get(3)?;
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "name": r.get::<_, String>(1)?,
            "raised_by": r.get::<_, String>(2)?,
            "raised_at": raised_at,
            "raised_at_iso": iso(raised_at),
            "note": r.get::<_, String>(4)?,
            "cleared": serde_json::from_str::<Value>(&cleared).unwrap_or(Value::Null),
        }))
    })?;
    rows.collect()
}

#[derive(serde::Deserialize, Default)]
pub struct ListQuery {
    limit: Option<i64>,
}

/// `GET /api/signals` — recent raises (newest first) and every live wait.
async fn list_signals(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let res = state
        .store
        .read_async(move |conn| {
            ensure_signal_table(conn)?;
            let total: i64 =
                conn.query_row("SELECT COUNT(*) FROM board_signals", [], |r| r.get(0))?;
            Ok((raise_rows(conn, None, limit)?, total, all_waits(conn)?))
        })
        .await;
    match res {
        Ok((raises, total, waits)) => reply(
            StatusCode::OK,
            // `raises_total` is the population `raises` was cut from, so a
            // capped list says it is capped.
            json!({"raises": raises, "raises_total": total, "limit": limit, "waits": waits}),
        ),
        Err(e) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": e.to_string()}),
        ),
    }
}

/// `GET /api/signals/{name}` — the raise history for one name and the cards
/// currently waiting on it. `amux signal wait` reads this to warn when the
/// signal was already raised (the event may have happened before the wait).
async fn get_signal(State(state): State<AppState>, Path(raw): Path<String>) -> Response {
    let name = normalize_signal_name(&raw);
    if !valid_signal_name(&name) {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid signal name", "name": raw, "rule": "1-64 chars of [a-z0-9._-], starting with a letter or digit"}),
        );
    }
    let n = name.clone();
    let res = state
        .store
        .read_async(move |conn| {
            ensure_signal_table(conn)?;
            let raises = raise_rows(conn, Some(&n), 20)?;
            let waits = all_waits(conn)?.remove(&n).unwrap_or_default();
            Ok((raises, waits))
        })
        .await;
    match res {
        Ok((raises, waits)) => reply(
            StatusCode::OK,
            json!({"name": name, "last_raised": raises.first().cloned(), "raises": raises, "waiting": waits}),
        ),
        Err(e) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": e.to_string()}),
        ),
    }
}

/// What the raise transaction produced, for the delivery step after it.
struct Raised {
    id: i64,
    at: i64,
    /// (card id, title, owning lane)
    cleared: Vec<(String, String, String)>,
}

/// `POST /api/signals/{name}` with `{"note": "..."}` — raise the signal.
async fn raise_signal(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let name = normalize_signal_name(&raw);
    if !valid_signal_name(&name) {
        return reply(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid signal name", "name": raw, "rule": "1-64 chars of [a-z0-9._-], starting with a letter or digit"}),
        );
    }
    let note: String = body
        .as_ref()
        .and_then(|Json(b)| b.get("note"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(2000)
        .collect();
    let raiser = actor_of(&headers);
    let raised = match raise_in_store(&state.store, &name, &raiser, &note).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "amux::signals", signal = %name, raiser = %raiser,
                verdict = "signal_raise_failed", error = %e, "signal raise could not be recorded");
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            );
        }
    };

    // Group the freed cards by lane: one wake per lane per raise, never one
    // per card, so a lane with ten parked cards gets one message.
    let mut by_lane: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (id, title, lane) in &raised.cleared {
        by_lane
            .entry(lane.clone())
            .or_default()
            .push((id.clone(), title.clone()));
    }
    let at_iso = iso(raised.at);
    let mut woken = Vec::new();
    for (lane, cards) in &by_lane {
        let verdict: String = if lane.is_empty() {
            "no_owner".into()
        } else if lane == &raiser {
            // The raiser already knows; waking it would spend a turn on news.
            "raiser_is_owner".into()
        } else if !wake_enabled(lane) {
            "wake_disabled".into()
        } else {
            let text = wake_message(&name, &raiser, &at_iso, &note, cards);
            // Stable id per (raise, lane): a retried delivery or a re-run of
            // this handler for the same raise cannot queue a second copy.
            let stable = format!("signal-wake:{}:{lane}", raised.id);
            match crate::api::session_verbs::steer_enqueue_idempotent(
                &state, lane, &text, WAKE_GUARD, &raiser, &stable,
            )
            .await
            {
                Ok(_) => "queued".into(),
                Err(reason) => format!("refused:{reason}"),
            }
        };
        tracing::info!(target: "amux::signals", signal = %name, raise_id = raised.id,
            lane = %lane, cards = cards.len(), verdict = %verdict, measured = true,
            n_considered = cards.len(), "signal wake");
        woken.push(json!({
            "session": lane,
            "cards": cards.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            "delivery": verdict,
        }));
    }
    tracing::info!(target: "amux::signals", signal = %name, raise_id = raised.id,
        raiser = %raiser, cleared = raised.cleared.len(), lanes = by_lane.len(),
        verdict = if raised.cleared.is_empty() { "raised_no_waiters" } else { "raised_and_cleared" },
        measured = true, n_considered = raised.cleared.len(), "signal raised");
    reply(
        StatusCode::OK,
        json!({
            "ok": true,
            "id": raised.id,
            "name": name,
            "raised_by": if raiser.is_empty() { Value::Null } else { json!(raiser) },
            "raised_at": raised.at,
            "raised_at_iso": at_iso,
            "note": note,
            "cleared": raised.cleared.iter().map(|(id, _, lane)| json!({"card": id, "session": lane})).collect::<Vec<_>>(),
            "woken": woken,
        }),
    )
}

/// The raise transaction: record the raise, then clear every card parked on
/// the name, in ONE write so a card cannot be cleared by a raise that was
/// never recorded (or recorded by one that cleared nothing it should have).
async fn raise_in_store(
    store: &crate::db::SharedStore,
    name: &str,
    raiser: &str,
    note: &str,
) -> anyhow::Result<Raised> {
    let slot: Arc<Mutex<Option<Raised>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let (name_w, raiser_w, note_w) = (name.to_string(), raiser.to_string(), note.to_string());
    store
        .write_async(move |conn| {
            ensure_signal_table(conn)?;
            let now = chrono::Utc::now().timestamp();
            let stamp = chrono::Local::now().format("%H:%M").to_string();
            let who = if raiser_w.is_empty() { "api-anonymous" } else { raiser_w.as_str() };
            let mut events = Vec::new();
            let mut cleared = Vec::new();
            for id in waiting_cards(conn, &name_w)? {
                let Some(mut row) = bs::get_issue(conn, &id)? else { continue };
                row.blocked_on = None;
                row.updated = now;
                row.rev += 1;
                row.version += 1;
                let note_part = if note_w.is_empty() { String::new() } else { format!(": {note_w}") };
                row.log = Some(bs::append_log(
                    row.log.as_deref(),
                    &stamp,
                    &format!("signal {name_w} raised by {who}{note_part}; wait cleared"),
                ));
                bs::save_patched(conn, &mut row)?;
                events.push(PendingEvent {
                    entity_type: EntityType::Task,
                    entity_id: row.id.clone(),
                    mutation: MutationKind::Updated,
                    payload: Some(row.snapshot()),
                });
                cleared.push((row.id.clone(), row.title.clone(), row.session.clone().unwrap_or_default()));
            }
            let cleared_json = json!(cleared
                .iter()
                .map(|(id, _, lane)| json!({"card": id, "session": lane}))
                .collect::<Vec<_>>())
            .to_string();
            conn.execute(
                "INSERT INTO board_signals (name, raised_by, raised_at, note, cleared) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![name_w, who, now, note_w, cleared_json],
            )?;
            let id = conn.last_insert_rowid();
            *slot_w.lock().unwrap_or_else(|p| p.into_inner()) = Some(Raised { id, at: now, cleared });
            Ok(WriteOutcome { applied: true, events })
        })
        .await?;
    let raised = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
    raised.ok_or_else(|| anyhow::anyhow!("signal raise wrote nothing"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn names_are_lowercase_slugs() {
        assert!(valid_signal_name("mvs-reflip-clear"));
        assert!(valid_signal_name("gs3.approval_2"));
        assert!(!valid_signal_name(""));
        assert!(!valid_signal_name("-leading-dash"));
        assert!(!valid_signal_name("has space"));
        assert!(!valid_signal_name("Upper"));
        assert!(!valid_signal_name(&"a".repeat(65)));
        assert_eq!(
            normalize_signal_name("  MVS-Reflip-Clear "),
            "mvs-reflip-clear"
        );
    }

    /// The pre-fix shape of `blocked_on` is free text, and it must stay
    /// exactly that: only the explicit prefix turns a block into a signal wait.
    #[test]
    fn only_the_prefix_makes_a_signal_wait() {
        assert_eq!(
            parse_signal_wait("waiting on the KubeRay answer"),
            SignalWait::NotASignal
        );
        assert_eq!(
            parse_signal_wait("mvs-infra 30-minute clearance"),
            SignalWait::NotASignal
        );
        assert_eq!(parse_signal_wait(""), SignalWait::NotASignal);
        assert_eq!(
            parse_signal_wait("signal:mvs-reflip-clear"),
            SignalWait::Valid("mvs-reflip-clear".into())
        );
        assert_eq!(
            parse_signal_wait(" Signal: MVS-Reflip-Clear "),
            SignalWait::Valid("mvs-reflip-clear".into())
        );
        assert!(matches!(
            parse_signal_wait("signal:"),
            SignalWait::Invalid(_)
        ));
        assert!(matches!(
            parse_signal_wait("signal:two words"),
            SignalWait::Invalid(_)
        ));
        assert_eq!(signal_wait_name(Some("signal:x")), Some("x".into()));
        assert_eq!(signal_wait_name(None), None);
    }

    #[test]
    fn a_worker_may_park_only_its_own_card() {
        assert!(
            wait_owner_refusal("", Some("backend")).is_none(),
            "owner may park any card"
        );
        assert!(wait_owner_refusal("gs-3", Some("gs-3")).is_none());
        let refused = wait_owner_refusal("gs-3", Some("mvs-infra")).expect("refused");
        assert_eq!(refused["code"], "signal_wait_not_own_card");
        assert!(
            wait_owner_refusal("gs-3", None).is_some(),
            "an unowned card is not yours"
        );
    }

    #[test]
    fn the_wake_names_signal_raiser_note_and_cards() {
        let m = wake_message(
            "mvs-reflip-clear",
            "mvs-infra",
            "2026-09-26T14:02:11Z",
            "shards 3-7 are back, 30 min soak passed",
            &[("GS-12".into(), "promote bucket objects".into())],
        );
        assert!(m.contains("`mvs-reflip-clear`"));
        assert!(m.contains("raised by mvs-infra at 2026-09-26T14:02:11Z"));
        assert!(m.contains("Note: shards 3-7 are back"));
        assert!(m.contains("- GS-12: promote bucket objects"));
        assert!(!m.contains('\u{2014}'), "no em-dashes in produced text");
        let bare = wake_message("x", "", "t", "  ", &[]);
        assert!(bare.contains("raised by the owner"));
        assert!(bare.contains("No note was given."));
    }

    #[test]
    fn the_wake_switch_resolves_worker_over_global_and_defaults_on() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        assert!(wake_enabled_in(home, "lane-a", None), "default ON");
        std::fs::write(home.join("amux.env"), "AMUX_SIGNAL_WAKE=0\n").unwrap();
        assert!(!wake_enabled_in(home, "lane-a", None), "global off");
        std::fs::write(home.join("sessions/lane-a.env"), "AMUX_SIGNAL_WAKE=1\n").unwrap();
        assert!(wake_enabled_in(home, "lane-a", None), "worker beats global");
        assert!(
            !wake_enabled_in(home, "lane-a", Some("off")),
            "process env wins"
        );
    }

    pub(crate) fn fixture() -> (AppState, crate::db::SharedStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            Arc::new(crate::db::Store::open(&dir.path().join("signals.db")).expect("open store"));
        std::mem::forget(dir);
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "signals-test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, store)
    }

    pub(crate) fn seed(
        store: &crate::db::SharedStore,
        session: &str,
        status: &str,
        blocked_on: Option<&str>,
    ) -> String {
        let slot = Arc::new(Mutex::new(None));
        let slot_w = slot.clone();
        let (session, status, blocked) = (
            session.to_string(),
            status.to_string(),
            blocked_on.map(str::to_string),
        );
        store
            .write(move |conn| {
                let mut row = bs::create_issue(
                    conn,
                    &bs::NewIssue {
                        acceptance_criteria: None,
                        next_action: None,
                        title: format!("card for {session}"),
                        desc: "fixture".into(),
                        status,
                        session: Some(session),
                        item_type: "code".into(),
                        creator: "test".into(),
                        owner_type: "agent".into(),
                        due: None,
                        due_time: None,
                        reviewer: None,
                        shepherd: None,
                        gate: vec![],
                        depends_on: vec![],
                        tags: vec![],
                        ask_type: None,
                        ask_question: None,
                        ask_unblocks: None,
                        ask_actor: None,
                        source: Some("test".into()),
                        requested_by: None,
                        callback_session: None,
                        callback_prompt: None,
                    },
                    1_700_000_000,
                )?;
                row.blocked_on = blocked;
                bs::save_patched(conn, &mut row)?;
                *slot_w.lock().unwrap() = Some(row.id);
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .expect("seed");
        let id = slot.lock().unwrap().clone().unwrap();
        id
    }

    pub(crate) fn row(store: &crate::db::SharedStore, id: &str) -> bs::IssueRow {
        bs::get_issue(&store.read().unwrap(), id).unwrap().unwrap()
    }

    async fn patch(
        state: &AppState,
        id: &str,
        caller: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut h = HeaderMap::new();
        if let Some(c) = caller {
            h.insert("x-amux-session", c.parse().unwrap());
        }
        let resp = crate::api::board::patch_item(
            State(state.clone()),
            Path(id.to_string()),
            h,
            Json(body),
        )
        .await;
        let code = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// The wait goes through the ordinary board PATCH, so the rules live there:
    /// own card only, a valid name, and the canonical spelling is what is stored.
    #[tokio::test]
    async fn a_signal_wait_is_own_card_only_valid_and_canonical() {
        let (state, store) = fixture();
        let mine = seed(&store, "gs-3-bucket-objects", "backlog", None);
        let theirs = seed(&store, "mvs-infra", "backlog", None);

        let (code, body) = patch(
            &state,
            &theirs,
            Some("gs-3-bucket-objects"),
            json!({"blocked_on": "signal:mvs-reflip-clear"}),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "signal_wait_not_own_card");
        assert_eq!(
            row(&store, &theirs).blocked_on,
            None,
            "refused write stored nothing"
        );

        let (code, body) = patch(
            &state,
            &mine,
            Some("gs-3-bucket-objects"),
            json!({"blocked_on": "signal:not valid"}),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "invalid_signal_name");

        let (code, body) = patch(
            &state,
            &mine,
            Some("gs-3-bucket-objects"),
            json!({"blocked_on": " Signal:MVS-Reflip-Clear "}),
        )
        .await;
        assert!(code.is_success(), "{code} {body}");
        assert_eq!(
            row(&store, &mine).blocked_on.as_deref(),
            Some("signal:mvs-reflip-clear")
        );

        // The owner (no worker header) may park any card; free text is untouched.
        let (code, _) = patch(
            &state,
            &theirs,
            None,
            json!({"blocked_on": "waiting on KubeRay"}),
        )
        .await;
        assert!(code.is_success());
        assert_eq!(
            row(&store, &theirs).blocked_on.as_deref(),
            Some("waiting on KubeRay")
        );
    }

    /// End to end over the store: only live cards parked on THIS name clear;
    /// free-text blocks, other names and terminal cards are untouched, and the
    /// raise is recorded with its raiser, note and cleared set.
    #[tokio::test]
    async fn a_raise_clears_exactly_the_live_waiters_and_records_itself() {
        let (state, store) = fixture();
        let waiting = seed(
            &store,
            "gs-3-bucket-objects",
            "backlog",
            Some("signal:mvs-reflip-clear"),
        );
        let waiting2 = seed(
            &store,
            "mixpeek-frustrations",
            "todo",
            Some("signal:mvs-reflip-clear"),
        );
        let other = seed(
            &store,
            "gs-3-bucket-objects",
            "backlog",
            Some("signal:another"),
        );
        let prose = seed(
            &store,
            "gs-3-bucket-objects",
            "backlog",
            Some("mvs-reflip-clear from mvs-infra"),
        );
        let closed = seed(
            &store,
            "gs-3-bucket-objects",
            "done",
            Some("signal:mvs-reflip-clear"),
        );

        let resp = raise_signal(
            State(state.clone()),
            Path("MVS-Reflip-Clear".into()),
            {
                let mut h = HeaderMap::new();
                h.insert("x-amux-session", "mvs-infra".parse().unwrap());
                h
            },
            Some(Json(json!({"note": "30 min soak passed"}))),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["name"], "mvs-reflip-clear");
        assert_eq!(body["raised_by"], "mvs-infra");
        assert_eq!(body["cleared"].as_array().unwrap().len(), 2, "{body}");
        // Two lanes, one wake entry each. No env file exists in the test home,
        // so the steering chokepoint refuses delivery; the verdict SAYS so
        // rather than claiming a wake that did not happen.
        let woken = body["woken"].as_array().unwrap();
        assert_eq!(woken.len(), 2, "{body}");
        for w in woken {
            let d = w["delivery"].as_str().unwrap();
            assert!(
                d == "queued" || d == "wake_disabled" || d.starts_with("refused:"),
                "every lane gets a stated delivery verdict: {w}"
            );
        }

        assert_eq!(row(&store, &waiting).blocked_on, None);
        assert_eq!(row(&store, &waiting2).blocked_on, None);
        assert!(row(&store, &waiting)
            .log
            .unwrap_or_default()
            .contains("signal mvs-reflip-clear raised by mvs-infra: 30 min soak passed"));
        assert_eq!(
            row(&store, &other).blocked_on.as_deref(),
            Some("signal:another")
        );
        assert_eq!(
            row(&store, &prose).blocked_on.as_deref(),
            Some("mvs-reflip-clear from mvs-infra")
        );
        assert_eq!(
            row(&store, &closed).blocked_on.as_deref(),
            Some("signal:mvs-reflip-clear")
        );

        // A second raise finds nobody waiting: no clears, no wakes, so a raise
        // repeated in a loop can never re-wake anyone.
        let again = raise_in_store(&store, "mvs-reflip-clear", "mvs-infra", "")
            .await
            .unwrap();
        assert!(again.cleared.is_empty());

        let conn = store.read().unwrap();
        let rows = raise_rows(&conn, Some("mvs-reflip-clear"), 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["note"], "30 min soak passed");
        assert_eq!(rows[1]["raised_by"], "mvs-infra");
        assert_eq!(
            all_waits(&conn).unwrap().get("another").map(Vec::len),
            Some(1)
        );
    }
}
