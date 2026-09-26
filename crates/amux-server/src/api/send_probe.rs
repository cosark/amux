//! Delivery probes only reach designated probe lanes (F8(d), AMUX-5241).
//!
//! INCIDENT. On 2026-09-24 at 16:19:43Z the text
//! `[send-pipeline-test] Delivery verification at ... Respond with:
//! DELIVERY_CONFIRMED` was typed into the primis pane, a customer-facing lane
//! that was holding a draft to an external contact. Nothing in the repo sends
//! it: the request log shows a headerless `curl/8.7.1` POST to
//! `/api/sessions/primis/send` with no `record_history`, so it also left no
//! Messages row. The transcript that issued it was an interactive session
//! doing an RCA on primis' send path, which picked "an idle worker" as its
//! test target after probing `lc1-solo-haiku` fifteen seconds earlier.
//!
//! Nothing stopped a probe aimed at a real worker, so the guard lives at the
//! send endpoint rather than in whichever script or session sends next.
//!
//! A probe is a message whose FIRST token is a bracketed tag naming a test,
//! for example `[send-pipeline-test]`, `[delivery-probe]`, `[canary]`. A probe
//! target is a lane whose name starts with a probe prefix, or that sets
//! `CC_PROBE_TARGET=1` (worker, group or global scope, so a whole validation
//! group can opt in at once).
//!
//! Kill switch: `AMUX_PROBE_GUARD=0` (process env, then the TARGET's worker >
//! group > global scope).

pub(crate) const GATE_KEY: &str = "AMUX_PROBE_GUARD";
pub(crate) const TARGET_FLAG: &str = "CC_PROBE_TARGET";
/// Lane-name prefixes that mark a lane as a place probes may go.
pub(crate) const PROBE_PREFIXES: [&str; 3] = ["probe-", "test-", "e2e-"];
/// Words in a leading tag that make a message a probe.
const PROBE_WORDS: [&str; 6] = ["probe", "test", "tests", "canary", "smoke", "selftest"];

/// The leading probe tag, if the message is a probe. Only a tag with no spaces
/// counts, so amux's own stamps (`[amux-origin: x ...]`, `[amux auto-pickup]`)
/// and `[no-board]` never match.
pub(crate) fn probe_tag(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('[')?;
    let end = rest.find(']')?;
    let tag = &rest[..end];
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return None;
    }
    tag.split(['-', '_', '.', ':'])
        .any(|w| PROBE_WORDS.contains(&w.to_ascii_lowercase().as_str()))
        .then_some(tag)
}

/// May a probe be delivered to this lane? `flag` is the lane's resolved
/// `CC_PROBE_TARGET`.
pub(crate) fn is_probe_target(name: &str, flag: Option<&str>) -> bool {
    PROBE_PREFIXES.iter().any(|p| name.starts_with(p))
        || flag.is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_incident_probe_is_a_probe() {
        let text = "[send-pipeline-test] Delivery verification at 2026-09-24T16:19:43Z. \
                    Respond with: DELIVERY_CONFIRMED";
        assert_eq!(probe_tag(text), Some("send-pipeline-test"));
        assert_eq!(probe_tag("  [delivery-probe] ping"), Some("delivery-probe"));
        assert_eq!(probe_tag("[canary] x"), Some("canary"));
        assert_eq!(probe_tag("[SMOKE_TEST] x"), Some("SMOKE_TEST"));
    }

    #[test]
    fn ordinary_and_amux_stamped_messages_are_not_probes() {
        for text in [
            "[amux-origin: backend — server-verified] run the tests please",
            "[amux auto-pickup] Claimed PRIMI-266",
            "[no-board] quick question",
            "[BACKE-4062] update on the fork",
            "please run the test suite [test]",
            "[testing-notes] is not a probe word",
            "[] empty",
            "no tag at all",
        ] {
            assert_eq!(probe_tag(text), None, "{text}");
        }
    }

    #[tokio::test]
    async fn the_send_endpoint_refuses_a_probe_to_a_real_worker_and_admits_one_to_a_probe_lane() {
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::api::AppState {
            store: std::sync::Arc::new(crate::db::Store::open(&tmp.path().join("t.db")).unwrap()),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let headers = axum::http::HeaderMap::new();
        let probe = "[send-pipeline-test] Delivery verification. Respond with: DELIVERY_CONFIRMED";
        let refused = crate::api::session_verbs::probe_refusal(
            &state,
            "real-worker-probe-guard-fixture",
            &headers,
            probe,
        )
        .await
        .expect("a probe to a real worker is refused");
        assert_eq!(refused.status(), axum::http::StatusCode::FORBIDDEN);
        assert!(crate::api::session_verbs::probe_refusal(
            &state,
            "probe-guard-lane",
            &headers,
            probe
        )
        .await
        .is_none());
        assert!(crate::api::session_verbs::probe_refusal(
            &state,
            "real-worker-probe-guard-fixture",
            &headers,
            "ordinary owner message"
        )
        .await
        .is_none());
        let conn = state.store.read().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_events WHERE type='send.probe_refused'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "the refusal is queryable");
    }

    #[test]
    fn only_designated_lanes_accept_probes() {
        assert!(!is_probe_target("primis", None));
        assert!(!is_probe_target("lc1-solo-haiku", None));
        assert!(!is_probe_target("primis", Some("0")));
        assert!(is_probe_target("lc1-solo-haiku", Some("1")));
        assert!(is_probe_target("probe-send-pipeline", None));
        assert!(is_probe_target("e2e-board", None));
        assert!(is_probe_target("test-lifecycle", Some("")));
    }
}
