//! What agents say about a run, in the reader's language. An agent (0.4.0+)
//! sends a code with its values (`refused.not_allowed {target}`) beside the
//! English; Wren shows the code's own text in the page's or the channel's
//! language. Unknown codes, and older agents' free text, stay as they came.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use vole_kit::i18n;

/// One coded message (a step's note or a run's result).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Note {
    pub code: String,
    pub params: BTreeMap<String, String>,
}

/// The most notes Wren keeps per step, and the longest value.
pub const MAX_NOTES: usize = 50;
pub const MAX_PARAM: usize = 500;

/// The translation key of a code Wren knows.
pub fn key(code: &str) -> Option<&'static str> {
    Some(match code {
        "refused.not_allowed" => "agent_msg.refused_not_allowed",
        "refused.updates_required" => "agent_msg.refused_updates_required",
        "refused.run_old" => "agent_msg.refused_run_old",
        "refused.rollback_unknown" => "agent_msg.refused_rollback_unknown",
        "refused.rollback_used" => "agent_msg.refused_rollback_used",
        "refused.too_soon" => "agent_msg.refused_too_soon",
        "refused.ledger_write" => "agent_msg.refused_ledger_write",
        "refused.not_under_roots" => "agent_msg.refused_not_under_roots",
        "refused.not_mounted" => "agent_msg.refused_not_mounted",
        "refused.file_outside_roots" => "agent_msg.refused_file_outside_roots",
        "refused.no_script_stack" => "agent_msg.refused_no_script_stack",
        "refused.gone" => "agent_msg.refused_gone",
        "refused.changed" => "agent_msg.refused_changed",
        "done.already_on" => "agent_msg.already_on",
        "failed.no_snapshot" => "agent_msg.no_snapshot",
        "failed.unreachable" => "agent_msg.unreachable",
        "path.not_under_roots" => "agent_msg.path_not_under_roots",
        "path.root" => "agent_msg.path_root",
        "path.shared" => "agent_msg.path_shared",
        "path.volume" => "agent_msg.path_volume",
        "snapshot.none" => "agent_msg.snapshot_none",
        _ => return None,
    })
}

/// Refusals that come from the agent's own `agent.toml`: the run shows on
/// its page, but no notification goes out (Wren blocks these approvals).
pub const CONFIG_REFUSALS: [&str; 6] = [
    "refused.not_allowed",
    "refused.updates_required",
    "refused.not_under_roots",
    "refused.not_mounted",
    "refused.file_outside_roots",
    "refused.no_script_stack",
];

/// The same, from an agent before 0.4.0 that sent English only.
pub fn is_config_refusal(code: &str, detail: &str) -> bool {
    if !code.is_empty() {
        return CONFIG_REFUSALS.contains(&code);
    }
    detail.starts_with("refused by agent config")
        || detail.contains("is not under the agent's roots")
        || detail.contains("is not mounted in the agent at the same path")
        || detail.contains("has no script stack named")
        || detail.contains("a Compose file is outside the agent's roots")
}

/// A code's text in the current language (`None`: a code Wren doesn't know).
pub fn coded(code: &str, params: &BTreeMap<String, String>) -> Option<String> {
    let key = key(code)?;
    let args: Vec<(&str, &str)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    Some(i18n::tf(key, &args))
}

/// A run's result: the code's text, else the English as the agent sent it.
pub fn detail(code: &str, params: &str, english: &str) -> String {
    let params: BTreeMap<String, String> = serde_json::from_str(params).unwrap_or_default();
    coded(code, &params).unwrap_or_else(|| english.to_string())
}

/// Notes as Wren stores them: known codes only, values clipped.
pub fn clean(notes: &[Note]) -> String {
    let kept: Vec<Note> = notes
        .iter()
        .filter(|n| key(&n.code).is_some())
        .take(MAX_NOTES)
        .map(|n| Note {
            code: n.code.clone(),
            params: n
                .params
                .iter()
                .take(8)
                .map(|(k, v)| (clip(k, 32), clip(v, MAX_PARAM)))
                .collect(),
        })
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&kept).unwrap_or_default()
    }
}

fn clip(s: &str, n: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(n).collect()
}

/// A step's stored notes: whether nothing was snapshotted, and every note
/// in the current language.
pub fn notes(stored: &str) -> (bool, Vec<String>) {
    let list: Vec<Note> = serde_json::from_str(stored).unwrap_or_default();
    let none = list.iter().any(|n| n.code == "snapshot.none");
    let texts = list
        .iter()
        .filter(|n| n.code != "snapshot.none")
        .filter_map(|n| coded(&n.code, &n.params))
        .collect();
    (none, texts)
}

/// Why a run took no data snapshot, in the current language (`None` when
/// it took one or the agent didn't say).
pub fn no_snapshot(stored: &str) -> Option<Vec<String>> {
    let (none, reasons) = notes(stored);
    none.then_some(reasons)
}
