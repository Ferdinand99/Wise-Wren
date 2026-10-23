//! "New version available" (design §9): one notification per target and
//! available version, through every active channel, written in the
//! channel's language. A channel with "digest" on gets one message per
//! check run instead. The version is marked as announced before anything is
//! sent, so it is never announced twice (a failed send is logged, not
//! retried).

use time::OffsetDateTime;
use vole_kit::i18n::{self, Lang};
use vole_kit::notify::alert::{Alert, AlertKind, Level};

use crate::Wren;
use crate::targets::{self, Target};

/// At most this many targets are listed in a digest.
pub const DIGEST_FACTS: usize = 15;
/// Up arrow: something new, nothing wrong.
const EMOJI: &str = "\u{2b06}\u{fe0f}";

/// An active channel: `(id, kind, config, language, digest)`.
type ChannelRow = (i64, String, String, String, bool);

/// Sends what is new since the last run; how many messages went out.
pub async fn send_new(wren: &Wren) -> usize {
    let core = &wren.core;
    let new = match core.with_conn(targets::take_new_updates).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(%err, "can't read the new updates");
            return 0;
        }
    };
    if new.is_empty() {
        return 0;
    }
    let channels: Vec<ChannelRow> = core
        .with_conn(|c| {
            c.prepare(
                "SELECT id, kind, config, language, digest FROM notification_channels WHERE active = 1 ORDER BY id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect()
        })
        .await
        .unwrap_or_default();
    let base = core.base_url().await;
    let mut sent = 0;
    for (id, kind, config, language, digest) in channels {
        let Ok(config) = serde_json::from_str(&config) else {
            continue;
        };
        let lang = match Lang::from_exact_code(&language) {
            Some(lang) => lang,
            None => i18n::site_language(core).await.unwrap_or_default(),
        };
        let batches: Vec<Vec<&Target>> = if digest {
            vec![new.iter().collect()]
        } else {
            new.iter().map(|t| vec![t]).collect()
        };
        for batch in batches {
            let base = base.clone();
            let result = vole_kit::notify::send_in(&core.http, &kind, &config, lang, || {
                alert(&batch, base.as_deref())
            })
            .await;
            match result {
                Ok(()) => sent += 1,
                Err(err) => tracing::warn!(channel_id = id, %err, "notification not sent"),
            }
        }
    }
    sent
}

fn link(base: Option<&str>, path: &str) -> Option<String> {
    base.map(|b| format!("{}{path}", b.trim_end_matches('/')))
}

/// A target's kind in the reader's words.
pub fn kind_label(kind: &str) -> &'static str {
    i18n::t(match kind {
        crate::inventory::UNRAID => "targets.kind_unraid",
        crate::inventory::COMPOSE => "targets.kind_compose",
        crate::inventory::SCRIPT => "targets.kind_script",
        _ => "targets.kind_container",
    })
}

/// The message for one target, or a digest for several.
pub fn alert(targets: &[&Target], base: Option<&str>) -> Alert {
    let kind = AlertKind::Expiring;
    let level = Level::Info;
    let (title, detail, facts, url, subject_id, subject) = match targets {
        [t] => {
            let title = i18n::tf(
                "notify_msg.update_title",
                &[("name", &t.name), ("server", &t.server_name)],
            );
            let mut facts = vec![
                (
                    i18n::t("notify_msg.fact_server").to_string(),
                    t.server_name.clone(),
                ),
                (
                    i18n::t("notify_msg.fact_kind").to_string(),
                    kind_label(&t.kind).to_string(),
                ),
            ];
            if !t.image.is_empty() {
                facts.push((
                    i18n::t("notify_msg.fact_image").to_string(),
                    t.image.clone(),
                ));
            }
            facts.push((
                i18n::t("notify_msg.fact_available").to_string(),
                t.available.clone(),
            ));
            if !t.notes_url.is_empty() {
                facts.push((
                    i18n::t("notify_msg.fact_notes").to_string(),
                    t.notes_url.clone(),
                ));
            }
            (
                title.clone(),
                Some(i18n::t("notify_msg.update_body").to_string()),
                facts,
                link(base, &format!("/targets/{}", t.id)),
                Some(t.id),
                t.name.clone(),
            )
        }
        many => {
            let count = many.len().to_string();
            let title = i18n::tf(
                "notify_msg.digest_title",
                &[("count", &count), ("product", crate::product::NAME)],
            );
            let mut facts: Vec<(String, String)> = many
                .iter()
                .take(DIGEST_FACTS)
                .map(|t| {
                    (
                        format!("{} · {}", t.server_name, t.name),
                        t.available.clone(),
                    )
                })
                .collect();
            if many.len() > DIGEST_FACTS {
                facts.push(("…".into(), (many.len() - DIGEST_FACTS).to_string()));
            }
            (
                title.clone(),
                None,
                facts,
                link(base, "/updates"),
                None,
                title,
            )
        }
    };
    Alert {
        kind,
        event: "update_available".into(),
        event_label: i18n::t("notify_msg.event_update").to_string(),
        status: "update_available".into(),
        level,
        colour: level.colour(),
        emoji: EMOJI.into(),
        title,
        message: detail.clone(),
        detail,
        facts,
        link_label: i18n::t("notify_msg.view_updates").to_string(),
        subject_id,
        subject,
        subject_type: "target".into(),
        target: String::new(),
        duration: None,
        expires_in_days: None,
        url,
        occurred_at: OffsetDateTime::now_utc(),
    }
}
