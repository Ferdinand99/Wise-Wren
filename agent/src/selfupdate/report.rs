//! What the agent tells Wren about updating itself: whether it can, and why
//! not. The heartbeat carries a [`Report`] every round; Wren shows the card
//! and offers an update only while `blocked` is empty. The report names the
//! container and its image reference, never its environment.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use super::state::{self, Phase};
use super::version::{parse, repo_rule, tag_rule};
use super::{STATE_FILE, config_dir};
use crate::config::Config;
use crate::docker::get_json;
use crate::heartbeat::VERSION;

/// This agent's own container, as Docker describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Own {
    /// The full container id.
    pub id: String,
    /// The container's name, without the leading `/`.
    pub name: String,
    /// `.Config.Image`: the reference the container was created from.
    pub reference: String,
    /// The image the container runs (`sha256:…`).
    pub image_id: String,
    /// The whole `GET /containers/<id>/json` answer. It holds the
    /// environment: never log it, never report it.
    pub inspect: Value,
}

impl Own {
    pub fn from_inspect(v: Value) -> Own {
        let text = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        Own {
            id: text(&v, "Id"),
            name: text(&v, "Name").trim_start_matches('/').to_string(),
            reference: v
                .get("Config")
                .map(|c| text(c, "Image"))
                .unwrap_or_default(),
            image_id: text(&v, "Image"),
            inspect: v,
        }
    }
}

/// The mount of the agent's container whose `Destination` is the config
/// folder, as the helper and the new container mount it
/// (`{"Type","Source","Target"}`); `None` when the folder is not a mount
/// (it would be gone with the container).
pub fn config_mount(inspect: &Value, config_dir: &Path) -> Option<Value> {
    inspect
        .get("Mounts")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|m| {
            let text = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("");
            if Path::new(text("Destination")) != config_dir {
                return None;
            }
            let source = match text("Type") {
                "bind" => text("Source"),
                "volume" => text("Name"),
                _ => return None,
            };
            let kind = text("Type");
            (!source.is_empty()).then(|| {
                serde_json::json!({"Type": kind, "Source": source, "Target": text("Destination")})
            })
        })
}

/// The folder can be written to: a file is made in it and removed again.
pub fn writable(dir: &Path) -> bool {
    let probe = dir.join(".wren-write-test");
    let made = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    made
}

/// The first thing that stops this agent from updating itself, as a code
/// (`""` when nothing does). `current` is this agent's version.
pub fn blocked(
    enabled: bool,
    own_id: &str,
    own: Option<&Own>,
    config_dir: &Path,
    current: &str,
) -> &'static str {
    if !enabled {
        return "off";
    }
    if own_id.is_empty() {
        return "no_own_id";
    }
    let Some(own) = own else {
        return "not_container";
    };
    if repo_rule(&own.reference).is_err() {
        return "foreign_repo";
    }
    // A version that is not a plain release cannot be shown to be covered by
    // a moving tag.
    let pinned = parse(current).is_none_or(|v| tag_rule(&own.reference, &v).is_err());
    if pinned {
        return "pinned";
    }
    let host = own.inspect.get("HostConfig").unwrap_or(&Value::Null);
    if host.get("AutoRemove").and_then(Value::as_bool) == Some(true) {
        return "autoremove";
    }
    let network = host
        .get("NetworkMode")
        .and_then(Value::as_str)
        .unwrap_or("");
    if network.starts_with("container:") {
        return "network_mode";
    }
    if config_mount(&own.inspect, config_dir).is_none() || !writable(config_dir) {
        return "config_ro";
    }
    ""
}

/// This agent's own container (`None` when Docker can't say, or this is not
/// a container).
pub async fn own(socket: &str) -> Option<Own> {
    let id = crate::jobs::own_id();
    if id.is_empty() {
        return None;
    }
    get_json(socket, &format!("/containers/{id}/json"))
        .await
        .ok()
        .map(Own::from_inspect)
}

/// A run that is not over, or whose end Wren has not heard yet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pending {
    pub run_id: i64,
    /// The state file's phase; `probation` while the new agent runs and has
    /// to confirm.
    pub phase: String,
    pub to_version: String,
}

/// The self-update part of the heartbeat.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    /// This agent knows about self-updates (older agents send no report).
    pub supported: bool,
    /// The local switch (`[self_update] enabled`) is on.
    pub enabled: bool,
    /// Why this agent cannot update itself ([`blocked`]); empty when it can.
    pub blocked: String,
    /// The agent's own container name.
    pub container: String,
    /// The image reference the container was created from.
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

/// The run in the state file that still matters to Wren, if any.
fn pending(config_dir: &Path) -> Option<Pending> {
    let s = state::load(&config_dir.join(STATE_FILE)).ok()??;
    if s.phase.terminal() && s.reported {
        return None;
    }
    let probation =
        matches!(s.phase, Phase::NewStarted | Phase::Confirmed) && s.to_version == VERSION;
    let phase = if probation {
        "probation".to_string()
    } else {
        serde_json::to_value(s.phase)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    };
    Some(Pending {
        run_id: s.run_id,
        phase,
        to_version: s.to_version,
    })
}

/// Reads Docker and the state file for this heartbeat.
pub async fn report(config: &Config) -> Report {
    let dir = config_dir(config);
    let own = own(&config.docker_socket).await;
    let blocked = blocked(
        config.self_update.enabled,
        &crate::jobs::own_id(),
        own.as_ref(),
        dir,
        VERSION,
    );
    Report {
        supported: true,
        enabled: config.self_update.enabled,
        blocked: blocked.to_string(),
        container: own.as_ref().map(|o| o.name.clone()).unwrap_or_default(),
        reference: own.map(|o| o.reference).unwrap_or_default(),
        pending: pending(dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selfupdate::SELF_UPDATE_REPO;
    use crate::selfupdate::state::StateFile;

    fn own_with(image: &str, host: Value, dir: &Path) -> Own {
        Own::from_inspect(serde_json::json!({
            "Id": "a".repeat(64), "Name": "/wren-agent", "Image": "sha256:old",
            "Config": {"Image": image, "Env": ["WW_AGENT_TOKEN=wwa_secret"]},
            "HostConfig": host,
            "Mounts": [{"Type": "bind", "Source": "/mnt/cfg", "Destination": dir}]
        }))
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wren-su-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn blocked_says_the_first_reason() {
        let dir = temp("w");
        let r = SELF_UPDATE_REPO;
        let moving = format!("{r}:latest");
        let ok = own_with(&moving, serde_json::json!({"NetworkMode": "bridge"}), &dir);
        let id = "a".repeat(64);
        assert_eq!(blocked(false, &id, Some(&ok), &dir, "0.10.0"), "off");
        assert_eq!(blocked(true, "", None, &dir, "0.10.0"), "no_own_id");
        assert_eq!(blocked(true, &id, None, &dir, "0.10.0"), "not_container");
        let foreign = own_with("wise-wren-agent:dev", serde_json::json!({}), &dir);
        assert_eq!(
            blocked(true, &id, Some(&foreign), &dir, "0.10.0"),
            "foreign_repo"
        );
        let pinned = own_with(&format!("{r}:0.10.0"), serde_json::json!({}), &dir);
        assert_eq!(blocked(true, &id, Some(&pinned), &dir, "0.10.0"), "pinned");
        let by_digest = own_with(
            &format!("{r}@sha256:{}", "0".repeat(64)),
            serde_json::json!({}),
            &dir,
        );
        assert_eq!(
            blocked(true, &id, Some(&by_digest), &dir, "0.10.0"),
            "pinned"
        );
        let auto = own_with(
            &format!("{r}:0"),
            serde_json::json!({"AutoRemove": true}),
            &dir,
        );
        assert_eq!(
            blocked(true, &id, Some(&auto), &dir, "0.10.0"),
            "autoremove"
        );
        let shared = own_with(
            &format!("{r}:0"),
            serde_json::json!({"NetworkMode": "container:vpn"}),
            &dir,
        );
        assert_eq!(
            blocked(true, &id, Some(&shared), &dir, "0.10.0"),
            "network_mode"
        );
        assert_eq!(
            blocked(
                true,
                &id,
                Some(&ok),
                Path::new("/nonexistent/wren"),
                "0.10.0"
            ),
            "config_ro"
        );
        assert_eq!(blocked(true, &id, Some(&ok), &dir, "0.10.0"), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_folder_that_is_no_mount_blocks() {
        // The preflight refuses it (the helper could not mount the folder),
        // so the card must not say "ready".
        let dir = temp("nomount");
        let mut own = own_with(
            &format!("{}:latest", SELF_UPDATE_REPO),
            serde_json::json!({}),
            &dir,
        );
        own.inspect["Mounts"] = serde_json::json!([]);
        let id = "a".repeat(64);
        assert_eq!(blocked(true, &id, Some(&own), &dir, "0.10.0"), "config_ro");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_config_mount_is_the_one_at_the_config_folder() {
        let inspect = serde_json::json!({"Mounts": [
            {"Type": "bind", "Source": "/var/run/docker.sock", "Destination": "/var/run/docker.sock"},
            {"Type": "bind", "Source": "/mnt/user/appdata/wren", "Destination": "/config"},
        ]});
        assert_eq!(
            config_mount(&inspect, Path::new("/config")),
            Some(
                serde_json::json!({"Type": "bind", "Source": "/mnt/user/appdata/wren", "Target": "/config"})
            )
        );
        let volume = serde_json::json!({"Mounts": [
            {"Type": "volume", "Name": "wren-config", "Source": "/var/lib/docker/volumes/x/_data", "Destination": "/config"},
        ]});
        assert_eq!(
            config_mount(&volume, Path::new("/config")),
            Some(
                serde_json::json!({"Type": "volume", "Source": "wren-config", "Target": "/config"})
            )
        );
        assert_eq!(config_mount(&inspect, Path::new("/other")), None);
    }

    #[test]
    fn the_own_container_is_read_from_inspect() {
        let dir = Path::new("/config");
        let own = own_with("img:1", serde_json::json!({}), dir);
        assert_eq!(
            (
                own.name.as_str(),
                own.reference.as_str(),
                own.image_id.as_str()
            ),
            ("wren-agent", "img:1", "sha256:old")
        );
        assert_eq!(own.id, "a".repeat(64));
    }

    #[test]
    fn writable_leaves_nothing_behind() {
        let dir = temp("writable");
        assert!(writable(&dir));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert!(!writable(&dir.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_heartbeat_carries_the_report_only_when_set() {
        let mut beat = crate::heartbeat::Heartbeat::default();
        assert!(
            serde_json::to_value(&beat)
                .unwrap()
                .get("self_update")
                .is_none()
        );
        beat.self_update = Some(Report {
            supported: true,
            enabled: true,
            container: "wren-agent".into(),
            ..Report::default()
        });
        let v = serde_json::to_value(&beat).unwrap();
        assert_eq!(v["self_update"]["supported"], true);
        assert_eq!(v["self_update"]["blocked"], "");
        assert!(v["self_update"].get("pending").is_none());
        // Names and reference only: no environment goes to Wren.
        assert!(!v.to_string().contains("wwa_secret"));
    }

    #[test]
    fn pending_follows_the_state_file() {
        let dir = temp("pending");
        let path = dir.join(STATE_FILE);
        assert_eq!(pending(&dir), None, "no file, no run");
        let mut s = StateFile {
            format: state::FORMAT,
            run_id: 57,
            to_version: VERSION.to_string(),
            phase: Phase::OldStopped,
            ..StateFile::default()
        };
        let phase = |s: &StateFile| {
            state::save(&path, s).unwrap();
            pending(&dir)
        };
        assert_eq!(
            phase(&s),
            Some(Pending {
                run_id: 57,
                phase: "old_stopped".into(),
                to_version: VERSION.into()
            })
        );
        // The new agent (this version) waiting to be confirmed is on probation.
        for p in [Phase::NewStarted, Phase::Confirmed] {
            s.phase = p;
            assert_eq!(phase(&s).unwrap().phase, "probation", "{p:?}");
        }
        // The old agent, watching the new one start, is not.
        s.to_version = "99.0.0".into();
        s.phase = Phase::NewStarted;
        assert_eq!(phase(&s).unwrap().phase, "new_started");
        // A finished run counts until it is reported.
        s.phase = Phase::RolledBack;
        assert_eq!(phase(&s).unwrap().phase, "rolled_back");
        s.reported = true;
        assert_eq!(phase(&s), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
