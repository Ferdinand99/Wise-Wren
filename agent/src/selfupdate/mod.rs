//! The agent updates itself (design: agent self-update). Wren sends an
//! `agent_update` job with a released version and its digest; this module
//! holds the names, labels, deadlines and codes the parts of that job share.
//! [`version`] has the rules for which version and which image reference
//! may be updated.

pub mod state;
pub mod version;

/// The one repository the agent updates itself from.
#[cfg(not(feature = "selfupdate-test"))]
pub const SELF_UPDATE_REPO: &str = "ghcr.io/ferdinand99/wise-wren-agent";
/// The repository of a test build: a local registry, or the one named when
/// it was built (`WW_SU_TEST_REPO`).
#[cfg(feature = "selfupdate-test")]
pub const SELF_UPDATE_REPO: &str = match option_env!("WW_SU_TEST_REPO") {
    Some(repo) => repo,
    None => "localhost:5057/ww-test-su/agent",
};

/// Label on the helper container that carries out the update.
pub const HELPER_LABEL: &str = "tools.hedgerow.wren.selfupdate";
/// Label that carries the run id on the helper container.
pub const RUN_LABEL: &str = "tools.hedgerow.wren.run";
/// The run's state, in the agent's config folder.
pub const STATE_FILE: &str = "selfupdate.json";
/// The marker the new agent writes when it is confirmed.
pub const CONFIRM_FILE: &str = "selfupdate.confirm";
/// The config folder's backups of what the helper may restore.
pub const BACKUP_DIR: &str = "selfupdate";

/// Seconds the helper container gets to start.
pub const HELPER_START: u64 = 120;
/// Seconds the old agent's container gets to stop.
pub const STOP: u64 = 60;
/// Seconds the new agent's container gets to be created and started.
pub const CREATE_START: u64 = 120;
/// The least time the new agent must stay healthy before it counts.
pub const MIN_STABLE: u64 = 60;
/// The longest a whole run may take (30 minutes).
pub const HARD: u64 = 1800;
/// Seconds the helper checks that the old agent runs again after a rollback.
pub const ROLLBACK_CHECK: u64 = 30;
/// Seconds after which a finished run's leftovers are swept.
pub const SWEEP: u64 = 600;

/// The helper container of run `run`.
pub fn helper_name(run: i64) -> String {
    format!("wren-agent-selfupdate-{run}")
}

/// The name the old agent's container keeps while the new one runs.
pub fn old_name(container: &str, run: i64) -> String {
    format!("{container}-wren-old-{run}")
}

/// How long the new agent must stay healthy: the agent's own
/// `min_health_seconds`, but at least [`MIN_STABLE`].
pub fn stable_seconds(min_health: u64) -> u64 {
    min_health.max(MIN_STABLE)
}

/// A deadline in seconds as this build runs it: as given in a release; a
/// test build scales it by `WW_SU_TEST_TIME_PCT` percent (read when called,
/// 100 when unset), to at least one second.
#[cfg(not(feature = "selfupdate-test"))]
pub fn scaled(seconds: u64) -> u64 {
    seconds
}

/// See the release version above.
#[cfg(feature = "selfupdate-test")]
pub fn scaled(seconds: u64) -> u64 {
    let pct = std::env::var("WW_SU_TEST_TIME_PCT").ok();
    scale(seconds, pct.as_deref())
}

#[cfg(feature = "selfupdate-test")]
fn scale(seconds: u64, pct: Option<&str>) -> u64 {
    let pct = pct
        .and_then(|p| p.trim().parse::<u64>().ok())
        .unwrap_or(100);
    (seconds.saturating_mul(pct) / 100).max(1)
}

/// Every code a self-update note carries (English in `jobs::english`; Wren
/// has them in every language).
pub const CODES: [&str; 17] = [
    "selfupdate.off",
    "selfupdate.no_own_id",
    "selfupdate.not_container",
    "selfupdate.foreign_repo",
    "selfupdate.pinned",
    "selfupdate.autoremove",
    "selfupdate.network_mode",
    "selfupdate.config_ro",
    "selfupdate.bad_version",
    "selfupdate.bad_digest",
    "selfupdate.busy",
    "selfupdate.label_mismatch",
    "selfupdate.not_release",
    "selfupdate.helper_failed",
    "selfupdate.done",
    "selfupdate.rolled_back",
    "selfupdate.rollback_failed",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_run() {
        assert_eq!(helper_name(57), "wren-agent-selfupdate-57");
        assert_eq!(
            old_name("Wise-Wren-Agent", 57),
            "Wise-Wren-Agent-wren-old-57"
        );
    }

    #[test]
    fn the_new_agent_stays_healthy_for_at_least_a_minute() {
        assert_eq!(stable_seconds(10), 60);
        assert_eq!(stable_seconds(60), 60);
        assert_eq!(stable_seconds(180), 180);
    }

    #[cfg(not(feature = "selfupdate-test"))]
    #[test]
    fn a_release_never_scales_deadlines() {
        assert_eq!(scaled(1800), 1800);
        assert_eq!(scaled(1), 1);
    }

    #[cfg(feature = "selfupdate-test")]
    #[test]
    fn a_test_build_scales_deadlines_but_never_to_zero() {
        assert_eq!(scale(300, None), 300);
        assert_eq!(scale(300, Some("10")), 30);
        assert_eq!(scale(5, Some("1")), 1);
        assert_eq!(scale(300, Some("junk")), 300);
    }

    #[test]
    fn the_codes_are_unique() {
        let mut all = CODES.to_vec();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 17);
    }
}
