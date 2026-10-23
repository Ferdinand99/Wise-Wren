//! Notifications about runs and agents (design §9): an update succeeded,
//! failed and was rolled back, failed with nothing changed, or its rollback
//! failed (urgent: "needs you"); and an agent went offline (once per
//! outage) or came back. Every active channel gets one message each, in its
//! language; digests are for "new version available" only.

use time::OffsetDateTime;
use vole_kit::i18n::{self, Lang};
use vole_kit::notify::alert::{Alert, AlertKind, Level};
use vole_kit::state::Core;

use crate::Wren;
use crate::runs::{self, Run};
use crate::updates_notify::kind_label;

/// Active channels: `(id, kind, config, language)`.
async fn channels(core: &Core) -> Vec<(i64, String, String, String)> {
    core.with_conn(|c| {
        c.prepare(
            "SELECT id, kind, config, language FROM notification_channels WHERE active = 1 ORDER BY id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect()
    })
    .await
    .unwrap_or_default()
}

/// Sends the alert `build` makes (in each channel's language) everywhere;
/// how many went out.
pub async fn broadcast(core: &Core, build: impl Fn(Option<&str>) -> Alert) -> usize {
    let base = core.base_url().await;
    let mut sent = 0;
    for (id, kind, config, language) in channels(core).await {
        let Ok(config) = serde_json::from_str(&config) else {
            continue;
        };
        let lang = match Lang::from_exact_code(&language) {
            Some(lang) => lang,
            None => i18n::site_language(core).await.unwrap_or_default(),
        };
        let result =
            vole_kit::notify::send_in(&core.http, &kind, &config, lang, || build(base.as_deref()))
                .await;
        match result {
            Ok(()) => sent += 1,
            Err(err) => tracing::warn!(channel_id = id, %err, "notification not sent"),
        }
    }
    sent
}

fn link(base: Option<&str>, path: &str) -> Option<String> {
    base.map(|b| format!("{}{path}", b.trim_end_matches('/')))
}

#[allow(clippy::too_many_arguments)]
fn alert(
    kind: AlertKind,
    level: Level,
    emoji: &str,
    event: &str,
    title: String,
    detail: Option<String>,
    facts: Vec<(String, String)>,
    url: Option<String>,
    subject: (Option<i64>, String, &str),
) -> Alert {
    Alert {
        kind,
        event: event.into(),
        event_label: i18n::t(match event {
            "agent_offline" | "agent_back" => "notify_msg.event_agent",
            _ => "notify_msg.event_run",
        })
        .to_string(),
        status: event.into(),
        level,
        colour: level.colour(),
        emoji: emoji.into(),
        title,
        message: detail.clone(),
        detail,
        facts,
        link_label: i18n::t("notify_msg.view_run").to_string(),
        subject_id: subject.0,
        subject: subject.1,
        subject_type: subject.2.into(),
        target: String::new(),
        duration: None,
        expires_in_days: None,
        url,
        occurred_at: OffsetDateTime::now_utc(),
    }
}

/// The message about a finished run (`None`: nothing to say, like a
/// cancelled one).
pub fn run_alert(run: &Run, base: Option<&str>) -> Option<Alert> {
    let args = [
        ("name", run.target_name.as_str()),
        ("server", run.server_name.as_str()),
    ];
    let rollback = run.kind == "rollback";
    let (kind, level, emoji, key) = match run.status.as_str() {
        runs::SUCCEEDED if rollback => (
            AlertKind::Recovered,
            Level::Ok,
            "\u{21a9}\u{fe0f}",
            "notify_msg.rollback_done_title",
        ),
        runs::SUCCEEDED => (
            AlertKind::Recovered,
            Level::Ok,
            "\u{2705}",
            "notify_msg.run_ok_title",
        ),
        runs::ROLLED_BACK => (
            AlertKind::Unstable,
            Level::Warning,
            "\u{21a9}\u{fe0f}",
            "notify_msg.run_rolled_back_title",
        ),
        runs::FAILED | runs::REFUSED => (
            AlertKind::Unstable,
            Level::Warning,
            "\u{26a0}\u{fe0f}",
            "notify_msg.run_failed_title",
        ),
        runs::ROLLBACK_FAILED => (
            AlertKind::Problem,
            Level::Critical,
            "\u{1f6a8}",
            "notify_msg.run_needs_you_title",
        ),
        _ => return None,
    };
    let mut facts = vec![
        (
            i18n::t("notify_msg.fact_server").to_string(),
            run.server_name.clone(),
        ),
        (
            i18n::t("notify_msg.fact_kind").to_string(),
            kind_label(&run.target_kind).to_string(),
        ),
    ];
    if !run.shown.is_empty() && !rollback {
        facts.push((
            i18n::t("notify_msg.fact_available").to_string(),
            run.shown.clone(),
        ));
    }
    facts.push((
        i18n::t("notify_msg.fact_trigger").to_string(),
        trigger_label(&run.trigger).to_string(),
    ));
    if run.trigger != runs::BY_AUTO {
        facts.push((
            i18n::t("notify_msg.fact_by").to_string(),
            run.requested_by.clone(),
        ));
    }
    // Every channel says what started the run, also those without facts.
    let started = format!(
        "{}: {}",
        i18n::t("notify_msg.fact_trigger"),
        trigger_label(&run.trigger)
    );
    let detail = Some(if run.detail.is_empty() {
        started
    } else {
        format!(
            "{}
{started}",
            run.detail.chars().take(500).collect::<String>()
        )
    });
    Some(alert(
        kind,
        level,
        emoji,
        &format!("run_{}", run.status),
        i18n::tf(key, &args),
        detail,
        facts,
        link(base, &format!("/runs/{}", run.id)),
        (Some(run.id), run.target_name.clone(), "run"),
    ))
}

/// What started a run, in words.
pub fn trigger_label(trigger: &str) -> &'static str {
    i18n::t(match trigger {
        runs::BY_AUTO => "runs.trigger_auto",
        runs::BY_SCHEDULE => "runs.trigger_schedule",
        _ => "runs.trigger_user",
    })
}

/// The message about a run that goes on without a maintenance window
/// because Vigilant Vole couldn't be reached.
pub fn no_window_alert(run: &Run, why: &str, base: Option<&str>) -> Alert {
    let args = [
        ("name", run.target_name.as_str()),
        ("server", run.server_name.as_str()),
    ];
    alert(
        AlertKind::Unstable,
        Level::Warning,
        "\u{1f6a7}",
        "run_no_window",
        i18n::tf("notify_msg.no_window_title", &args),
        Some(why.chars().take(500).collect()),
        vec![
            (
                i18n::t("notify_msg.fact_server").to_string(),
                run.server_name.clone(),
            ),
            (
                i18n::t("notify_msg.fact_trigger").to_string(),
                trigger_label(&run.trigger).to_string(),
            ),
        ],
        link(base, &format!("/runs/{}", run.id)),
        (Some(run.id), run.target_name.clone(), "run"),
    )
}

pub async fn send_no_window(wren: &Wren, run: &Run, why: &str) {
    let (run, why) = (run.clone(), why.to_string());
    broadcast(&wren.core, move |base| no_window_alert(&run, &why, base)).await;
}

/// Sends the result of a run.
pub async fn send_result(wren: &Wren, run: &Run) {
    if run_alert(run, None).is_none() {
        return;
    }
    let run = run.clone();
    broadcast(&wren.core, move |base| {
        run_alert(&run, base).expect("checked above")
    })
    .await;
}

/// The message about an agent going offline (`back`: coming back).
pub fn agent_alert(server_id: i64, server: &str, back: bool, base: Option<&str>) -> Alert {
    let (kind, level, emoji, key, event) = if back {
        (
            AlertKind::Recovered,
            Level::Ok,
            "\u{2705}",
            "notify_msg.agent_back_title",
            "agent_back",
        )
    } else {
        (
            AlertKind::Problem,
            Level::Critical,
            "\u{1f534}",
            "notify_msg.agent_offline_title",
            "agent_offline",
        )
    };
    alert(
        kind,
        level,
        emoji,
        event,
        i18n::tf(key, &[("server", server)]),
        Some(
            i18n::t(if back {
                "notify_msg.agent_back_body"
            } else {
                "notify_msg.agent_offline_body"
            })
            .to_string(),
        ),
        Vec::new(),
        link(base, &format!("/servers/{server_id}")),
        (Some(server_id), server.to_string(), "server"),
    )
}

/// Servers whose agent went offline (not told yet) and ones back online
/// (told they were offline); both are marked before anything is sent.
pub fn take_agent_changes(c: &rusqlite::Connection) -> rusqlite::Result<Vec<(i64, String, bool)>> {
    let window = crate::db::ONLINE_WINDOW_SECONDS;
    let mut out: Vec<(i64, String, bool)> = c
        .prepare(&format!(
            "SELECT id, name, 0 FROM servers WHERE token_hash IS NOT NULL AND last_seen_at IS NOT NULL
                 AND last_seen_at < datetime('now', '-{window} seconds') AND offline_notified_at IS NULL
             UNION ALL
             SELECT id, name, 1 FROM servers WHERE offline_notified_at IS NOT NULL
                 AND last_seen_at >= datetime('now', '-{window} seconds')"
        ))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    out.sort();
    for (id, _, back) in &out {
        c.execute(
            if *back {
                "UPDATE servers SET offline_notified_at = NULL WHERE id = ?1"
            } else {
                "UPDATE servers SET offline_notified_at = datetime('now') WHERE id = ?1"
            },
            [id],
        )?;
    }
    Ok(out)
}

/// Notifies about agents that went offline or came back.
pub async fn check_agents(wren: &Wren) {
    let changes = match wren.core.with_conn(|c| take_agent_changes(c)).await {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(%err, "can't read the agents' state");
            return;
        }
    };
    for (id, name, back) in changes {
        tracing::info!(
            server_id = id,
            back,
            "agent {}",
            if back { "back" } else { "offline" }
        );
        broadcast(&wren.core, |base| agent_alert(id, &name, back, base)).await;
    }
}
