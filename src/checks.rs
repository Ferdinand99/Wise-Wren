//! Check runs (design §4): what is due, the registry checks with one answer
//! per image reference per run, script stacks' version sources, release
//! notes for available updates, then the "new version" notifications.
//! Checks never change anything on a server.
//!
//! A task runs [`run_due`] every [`TICK`] and whenever an inventory arrives
//! or someone presses "Check now" (`Wren::checks`).

use std::time::Duration;

use crate::Wren;
use crate::inventory::SCRIPT;
use crate::registry::{Checker, ImageRef};
use crate::targets::{self, Filter, UPDATE};
use crate::versions::{self, Source};

/// How often the scheduler looks for due targets.
pub const TICK: Duration = Duration::from_secs(60);

/// What one run did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub targets: usize,
    pub images: usize,
    pub registry_requests: usize,
    pub notifications: usize,
}

/// Checks every due target. Runs one at a time.
pub async fn run_due(wren: &Wren) -> Report {
    let _running = wren.check_lock.lock().await;
    match run(wren).await {
        Ok(report) => {
            if report.targets > 0 {
                tracing::info!(
                    targets = report.targets,
                    images = report.images,
                    registry_requests = report.registry_requests,
                    notifications = report.notifications,
                    "check run done"
                );
            }
            report
        }
        Err(err) => {
            tracing::warn!(%err, "check run failed");
            Report::default()
        }
    }
}

async fn run(wren: &Wren) -> Result<Report, vole_kit::http::ApiError> {
    let core = &wren.core;
    let due = core.with_conn(|c| targets::due(c)).await?;
    if due.is_empty() {
        return Ok(Report::default());
    }
    let ids = due.clone();
    let refs = core
        .with_conn(move |c| targets::image_refs(c, &ids))
        .await?;
    let creds = crate::registries::credentials(core).await;
    let mut checker = Checker::new(core.http.clone(), creds);
    for image in &refs {
        let Ok(parsed) = ImageRef::parse(image) else {
            continue;
        };
        let result = checker.digest(&parsed).await;
        if let Err(err) = &result {
            tracing::debug!(image, %err, "registry check failed");
        }
        let key = parsed.canonical();
        core.with_conn(move |c| targets::set_remote(c, &key, &result))
            .await?;
    }

    // Script stacks with a GitHub or URL version source.
    let ids = due.clone();
    let scripts: Vec<(i64, String)> = core
        .with_conn(move |c| {
            let mut out = Vec::new();
            for id in ids {
                if let Some(t) = targets::get(c, id)?
                    && t.kind == SCRIPT
                    && !t.version_source.is_empty()
                {
                    out.push((t.id, t.version_source));
                }
            }
            Ok(out)
        })
        .await?;
    for (id, raw) in scripts {
        let source: Source = serde_json::from_str(&raw).unwrap_or_default();
        let result = if !source.github.is_empty() {
            wren.github.latest_release(&core.http, &source.github).await
        } else if !source.url.is_empty() {
            versions::from_url(&source, wren.fetch_allow()).await
        } else {
            continue;
        };
        core.with_conn(move |c| targets::set_latest(c, id, &result))
            .await?;
    }

    let ids = due.clone();
    core.with_conn(move |c| {
        targets::mark_checked(c, &ids)?;
        targets::refresh_all(c)
    })
    .await?;

    release_notes(wren, &due).await?;
    let notifications = crate::updates_notify::send_new(wren).await;
    let _ = wren.changes.send(());
    Ok(Report {
        targets: due.len(),
        images: refs.len(),
        registry_requests: checker.requests,
        notifications,
    })
}

/// Release notes for the checked targets that have an update and a GitHub
/// source label (script stacks with a GitHub source have theirs already).
async fn release_notes(wren: &Wren, ids: &[i64]) -> Result<(), vole_kit::http::ApiError> {
    let core = &wren.core;
    let wanted: Vec<i64> = ids.to_vec();
    let rows = core
        .with_conn(move |c| {
            let all = targets::list(c, Filter::default())?;
            Ok(all
                .into_iter()
                .filter(|t| wanted.contains(&t.id) && t.status == UPDATE)
                .collect::<Vec<_>>())
        })
        .await?;
    for t in rows {
        if t.kind == SCRIPT && !t.latest_url.is_empty() {
            continue;
        }
        let Some(repo) = versions::github_repo(&t.source_url) else {
            continue;
        };
        if let Ok(release) = wren.github.latest_release(&core.http, &repo).await {
            let id = t.id;
            core.with_conn(move |c| targets::set_notes(c, id, &release.url, &release.title))
                .await?;
        }
    }
    Ok(())
}

/// Runs the checks every [`TICK`] and when woken.
pub fn spawn_scheduler(wren: Wren) {
    tokio::spawn(async move {
        loop {
            run_due(&wren).await;
            tokio::select! {
                () = tokio::time::sleep(TICK) => {}
                () = wren.checks.notified() => {
                    // Let a burst (an inventory after "Check now") settle.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
}
