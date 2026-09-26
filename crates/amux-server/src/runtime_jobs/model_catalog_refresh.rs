//! Live model catalog refresh tick. The fetch/merge/classify logic lives in
//! `provider::live_catalog`; this file is only the schedule.

use crate::provider::live_catalog;

const JOB: &str = super::registry::ids::MODEL_CATALOG_REFRESH;

/// Every 5 minutes, but each vendor is only ASKED once per
/// `live_catalog::FRESH_SECS` (an hour) after a success, so a healthy fleet
/// still makes one call per vendor per hour (ethos rule 2). The short tick is
/// for the failure case: a probe that times out is retried within minutes
/// instead of leaving that vendor on its cached or static list for an hour
/// (2026-09-26: a timed-out boot probe hid `claude-opus-5-5` for 47 minutes).
/// `spawn_periodic` ticks once immediately, so a fresh boot populates the
/// catalog before the first `/api/models` request in practice.
pub fn spawn() -> super::PeriodicTask {
    super::spawn_periodic(JOB, 300, || async {
        live_catalog::refresh(&crate::config::amux_home()).await;
    })
}
