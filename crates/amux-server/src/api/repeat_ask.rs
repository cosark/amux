//! Flag an owner message that repeats an earlier ask to the same lane (F8(e),
//! AMUX-5241).
//!
//! INCIDENT. Ethan sent mixpeek-ops-server "can you organize the op tasks by
//! type like gtm is a category ..." as MSG-68912 on 2026-09-25 00:16Z and
//! again, byte for byte, as MSG-69179 on 2026-09-26 14:02Z, because the first
//! result was not findable. The lane received the second copy as a fresh
//! request, with nothing telling it the work had been asked for before, so the
//! likely outcome was the same work done twice and the same unfindable result.
//!
//! When an owner message is a near-duplicate of one sent to the same lane in
//! the last 14 days, the DELIVERED text gets a one-line note naming the earlier
//! message and asking the lane to say where that result lives and what is
//! still missing. The history row records the original text plus `repeat_of`
//! (migration 0088), so the Messages tab can show it.
//!
//! Similarity is Jaccard over normalized word sets. It is deliberately simple:
//! the incident was an exact repeat, and a paraphrase that scores below the
//! threshold costs nothing (the message is delivered exactly as before).
//!
//! Kill switch: `AMUX_REPEAT_ASK_NOTE=0` (process env, then worker > group >
//! global).
use std::collections::BTreeSet;

pub(crate) const GATE_KEY: &str = "AMUX_REPEAT_ASK_NOTE";
/// How far back an earlier ask counts.
pub(crate) const WINDOW_MS: i64 = 14 * 24 * 3_600_000;
/// Younger than this is a retry or a double send, which the duplicate-delivery
/// detector already announces; it is not a request the lane failed to answer.
pub(crate) const MIN_AGE_MS: i64 = 30 * 60_000;
/// Jaccard at or above this is a repeat.
pub(crate) const THRESHOLD: f64 = 0.7;
/// "continue", "done" and "yes" are repeated constantly and are never repeat
/// asks. Six distinct content words is the smallest request worth flagging.
pub(crate) const MIN_TOKENS: usize = 6;
/// How many recent owner rows to compare against. A lane receives far fewer
/// owner messages than this in 14 days; the cap bounds the read.
pub(crate) const MAX_CANDIDATES: i64 = 400;

const STOPWORDS: [&str; 24] = [
    "the", "and", "for", "you", "that", "this", "with", "can", "are", "its", "it's", "all", "but",
    "not", "was", "have", "has", "our", "any", "what", "from", "into", "then", "now",
];

/// The dashboard prefixes a local clock stamp (`[08:16 PM] ...`). It differs
/// on every send of the same text and must not count against a repeat.
fn strip_clock_stamp(text: &str) -> &str {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let stamp = &rest[..end];
            let looks_like_clock = stamp.len() <= 12
                && stamp.contains(':')
                && stamp
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b':' | b' ' | b'A' | b'P' | b'M'));
            if looks_like_clock {
                return &rest[end + 1..];
            }
        }
    }
    t
}

pub(crate) fn tokens(text: &str) -> BTreeSet<String> {
    strip_clock_stamp(text)
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(w))
        .map(str::to_owned)
        .collect()
}

pub(crate) fn similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

/// An earlier owner message to the same lane: `(id, ts_ms, text)`.
pub(crate) type Candidate = (i64, i64, String);

/// The earlier ask this message repeats, if any: `(id, ts_ms, score)`. The most
/// recent qualifying message wins, because that is the result the owner was
/// most recently waiting on.
pub(crate) fn find_repeat(
    text: &str,
    candidates: &[Candidate],
    now_ms: i64,
) -> Option<(i64, i64, f64)> {
    let mine = tokens(text);
    if mine.len() < MIN_TOKENS {
        return None;
    }
    candidates
        .iter()
        .filter(|(_, ts, _)| {
            let age = now_ms - ts;
            (MIN_AGE_MS..=WINDOW_MS).contains(&age)
        })
        .filter_map(|(id, ts, t)| {
            let score = similarity(&mine, &tokens(t));
            (score >= THRESHOLD).then_some((*id, *ts, score))
        })
        .max_by_key(|(_, ts, _)| *ts)
}

/// The line prepended to the delivered text.
pub(crate) fn note(id: i64, ts_ms: i64) -> String {
    let date = chrono::DateTime::from_timestamp_millis(ts_ms)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "an earlier date".into());
    format!(
        "[amux: this repeats MSG-{id} from {date}. Before redoing the work, say where the \
         result of MSG-{id} lives and what is still missing.]\n\n"
    )
}

/// Is this owner message a repeat ask? Gate, lookup and log in one place, so
/// the send path only has to prepend the note and mark the row.
pub(crate) async fn lookup(
    state: &super::AppState,
    session: &str,
    text: &str,
) -> Option<(i64, i64)> {
    if !super::session_verbs::scoped_gate_on(session, GATE_KEY) {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let rows = state
        .store
        .read()
        .ok()
        .and_then(|c| candidates(&c, session, now_ms).ok());
    let Some(rows) = rows else {
        tracing::warn!(
            session,
            measured = false,
            verdict = "repeat_ask_unmeasured",
            "could not read earlier owner messages; delivering without a repeat check"
        );
        return None;
    };
    let found = find_repeat(text, &rows, now_ms);
    if let Some((id, ts, score)) = found {
        tracing::info!(
            session,
            repeat_of = id,
            age_h = (now_ms - ts) / 3_600_000,
            score,
            measured = true,
            n_considered = rows.len(),
            verdict = "repeat_ask_noted",
            "owner message repeats an earlier ask to this lane; delivered with a note naming it"
        );
    }
    found.map(|(id, ts, _)| (id, ts))
}

/// Owner messages to `session` inside the window, newest first.
pub(crate) fn candidates(
    conn: &rusqlite::Connection,
    session: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, text FROM cmd_history WHERE session=?1 AND ts>?2 \
         AND type IN ('user','direct','steering') ORDER BY ts DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![session, now_ms - WINDOW_MS, MAX_CANDIDATES],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;
    const ASK: &str = "can you organize the op tasks by type like gtm is a category and make it \
                       easy to disable enable each there shoukd be a standard class so we can add \
                       more extend etc";

    #[test]
    fn the_incident_repeat_is_found_through_the_clock_stamps() {
        // MSG-68912 and MSG-69179 as stored: same text, different clock stamps,
        // about 38 hours apart.
        let first = (68912, 1_000 * HOUR, format!("[08:16 PM] {ASK}"));
        let now = 1_000 * HOUR + 38 * HOUR;
        let got = find_repeat(&format!("[10:02 AM] {ASK}"), &[first], now).unwrap();
        assert_eq!((got.0, got.1), (68912, 1_000 * HOUR));
        assert!((got.2 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_light_rewording_still_counts_and_a_different_ask_does_not() {
        let prior = vec![
            (1, 0, ASK.to_string()),
            (
                2,
                HOUR,
                "whats the audit of all ops server responsibilities and the list of scheduled runs"
                    .to_string(),
            ),
        ];
        let now = 10 * HOUR;
        let reworded = "can you organize the op tasks by type, like gtm is a category, and make it \
                        easy to enable and disable each one; there should be a standard class so we \
                        can add more and extend";
        assert_eq!(find_repeat(reworded, &prior, now).map(|r| r.0), Some(1));
        assert_eq!(
            find_repeat(
                "the founder email should explain the starting namespace in one sentence",
                &prior,
                now
            ),
            None
        );
    }

    #[test]
    fn short_messages_retries_and_old_asks_are_not_repeats() {
        let now = 20 * 24 * HOUR;
        let prior = vec![(1, now - 2 * HOUR, "continue".to_string())];
        assert_eq!(find_repeat("continue", &prior, now), None, "too short");
        let retry = vec![(2, now - 5 * 60_000, ASK.to_string())];
        assert_eq!(find_repeat(ASK, &retry, now), None, "a retry minutes later");
        let stale = vec![(3, now - 15 * 24 * HOUR, ASK.to_string())];
        assert_eq!(find_repeat(ASK, &stale, now), None, "outside 14 days");
    }

    #[test]
    fn the_most_recent_repeat_wins_and_the_note_names_it() {
        let now = 100 * HOUR;
        let prior = vec![
            (10, 10 * HOUR, ASK.to_string()),
            (20, 50 * HOUR, ASK.to_string()),
        ];
        let (id, ts, _) = find_repeat(ASK, &prior, now).unwrap();
        assert_eq!(id, 20);
        let n = note(id, 1_790_000_000_000);
        assert!(
            n.starts_with("[amux: this repeats MSG-20 from 2026-"),
            "{n}"
        );
        assert!(n.contains("what is still missing"));
        assert!(n.ends_with("\n\n"));
        let _ = ts;
    }

    #[tokio::test]
    async fn a_repeat_is_found_in_the_live_ledger_and_marked_on_the_new_row() {
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::api::AppState {
            store: std::sync::Arc::new(crate::db::Store::open(&tmp.path().join("t.db")).unwrap()),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let lane = "repeat-ask-fixture";
        let then = chrono::Utc::now().timestamp_millis() - 38 * HOUR;
        state
            .store
            .write(move |c| {
                c.execute(
                    "INSERT INTO cmd_history(id,text,type,session,ts,origin,delivery) VALUES(68912,?1,'user',?2,?3,'','direct')",
                    rusqlite::params![format!("[08:16 PM] {ASK}"), "repeat-ask-fixture", then],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let found = lookup(&state, lane, &format!("[10:02 AM] {ASK}")).await;
        assert_eq!(found, Some((68912, then)));
        let row = crate::api::session_verbs::cmd_hist_record_with_id(
            &state,
            lane,
            &format!("[10:02 AM] {ASK}"),
            "user",
            "",
            true,
            crate::api::session_verbs::DeliveryMeta {
                repeat_of: found.map(|f| f.0),
                ..crate::api::session_verbs::DeliveryMeta::direct()
            },
        )
        .await;
        let conn = state.store.read().unwrap();
        let (text, repeat_of): (String, Option<i64>) = conn
            .query_row(
                "SELECT text, repeat_of FROM cmd_history WHERE id=?1",
                [row],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(repeat_of, Some(68912));
        assert!(
            !text.contains("[amux: this repeats"),
            "the row keeps the owner's words"
        );
    }

    #[test]
    fn candidates_reads_only_owner_rows_for_the_lane() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE cmd_history(id INTEGER PRIMARY KEY, text TEXT, type TEXT, session TEXT, ts INTEGER);
             INSERT INTO cmd_history VALUES (1,'a','user','ops',100),(2,'b','session','ops',100),
               (3,'c','pickup','ops',100),(4,'d','user','other',100),(5,'e','direct','ops',50);",
        )
        .unwrap();
        let got = candidates(&conn, "ops", 200).unwrap();
        assert_eq!(got.iter().map(|c| c.0).collect::<Vec<_>>(), vec![1, 5]);
    }
}
