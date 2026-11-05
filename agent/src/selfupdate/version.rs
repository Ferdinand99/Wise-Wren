//! Which version and which image reference an agent update may use:
//! only a newer, plain `major.minor.patch` release, from the official
//! repository, through a tag that moves with the version.

use super::SELF_UPDATE_REPO;
use crate::jobs::repo_of;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Semver {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

/// One number of a version: ASCII digits only, no leading zero.
fn number(part: &str) -> Option<u64> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if part.len() > 1 && part.starts_with('0') {
        return None;
    }
    part.parse().ok()
}

/// `major.minor.patch` and nothing else (no `v`, pre-release or build part).
pub fn parse(v: &str) -> Option<Semver> {
    let mut parts = v.split('.');
    let (major, minor, patch) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    Some(Semver {
        major: number(major)?,
        minor: number(minor)?,
        patch: number(patch)?,
    })
}

/// `to` is a release newer than `current`.
pub fn target_ok(to: &str, current: &str) -> Result<Semver, &'static str> {
    match (parse(to), parse(current)) {
        (Some(t), Some(c)) if t > c => Ok(t),
        _ => Err("selfupdate.bad_version"),
    }
}

/// The reference is the official agent repository (any case; a digest-only
/// reference names no repository).
pub fn repo_rule(reference: &str) -> Result<(), &'static str> {
    match repo_of(reference) {
        Some(repo) if repo.to_lowercase() == SELF_UPDATE_REPO => Ok(()),
        _ => Err("selfupdate.foreign_repo"),
    }
}

/// The reference's tag moves with `version`: none, `latest`, `major` or
/// `major.minor`. A digest or any other tag pins a version, and Wren never
/// moves a tag that promises one.
pub fn tag_rule(reference: &str, version: &Semver) -> Result<(), &'static str> {
    if reference.contains('@') {
        return Err("selfupdate.pinned");
    }
    let Some(repo) = repo_of(reference) else {
        return Err("selfupdate.pinned");
    };
    let Some(tag) = reference[repo.len()..].strip_prefix(':') else {
        return Ok(());
    };
    let (major, minor) = (version.major.to_string(), version.minor.to_string());
    if tag == "latest" || tag == major || tag == format!("{major}.{minor}") {
        Ok(())
    } else {
        Err("selfupdate.pinned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: &str = SELF_UPDATE_REPO;

    #[test]
    fn versions_parse_strictly() {
        assert_eq!(
            parse("0.10.1"),
            Some(Semver {
                major: 0,
                minor: 10,
                patch: 1
            })
        );
        for bad in [
            "v0.10.1",
            "0.10",
            "0.10.1-rc.1",
            "0.10.1+b",
            "edge",
            "",
            "0.10.x",
            "00.1.0",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_newer_releases_are_targets() {
        assert!(target_ok("0.10.1", "0.10.0").is_ok());
        assert!(target_ok("1.0.0", "0.10.3").is_ok());
        for (to, cur) in [
            ("0.10.0", "0.10.0"),
            ("0.9.9", "0.10.0"),
            ("0.10.1-rc.1", "0.10.0"),
            ("edge", "0.10.0"),
        ] {
            assert_eq!(target_ok(to, cur), Err("selfupdate.bad_version"), "{to}");
        }
    }

    #[test]
    fn moving_tags_cover_the_version() {
        let cur = parse("0.10.0").unwrap();
        for ok in [":latest", ":0", ":0.10"] {
            assert_eq!(tag_rule(&format!("{R}{ok}"), &cur), Ok(()), "{ok}");
        }
        for pinned in [
            ":0.10.0",
            ":0.9",
            ":1",
            ":edge",
            "@sha256:0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert_eq!(
                tag_rule(&format!("{R}{pinned}"), &cur),
                Err("selfupdate.pinned"),
                "{pinned}"
            );
        }
    }

    #[test]
    fn a_tag_must_also_cover_the_version_it_moves_to() {
        // Preflight checks the tag against the target as well as the current
        // version: `:0.10` must not be moved to 0.11 content.
        let to = parse("0.11.0").unwrap();
        assert_eq!(
            tag_rule(&format!("{R}:0.10"), &to),
            Err("selfupdate.pinned")
        );
        assert_eq!(tag_rule(&format!("{R}:0.11"), &to), Ok(()));
        assert_eq!(tag_rule(&format!("{R}:0"), &to), Ok(()));
        let to = parse("1.0.0").unwrap();
        assert_eq!(tag_rule(&format!("{R}:0"), &to), Err("selfupdate.pinned"));
        assert_eq!(tag_rule(&format!("{R}:latest"), &to), Ok(()));
    }

    #[test]
    fn an_untagged_reference_counts_as_latest() {
        let cur = parse("0.10.0").unwrap();
        assert_eq!(tag_rule(R, &cur), Ok(()));
        assert_eq!(repo_rule(&format!("{}:latest", R.to_uppercase())), Ok(()));
        assert_eq!(repo_rule(R), Ok(()));
        for foreign in [
            "wise-wren-agent:dev".to_string(),
            "ghcr.io/someone/wise-wren-agent:latest".to_string(),
            format!("{R}-fork:latest"),
            format!("{R}@sha256:{}", "0".repeat(64)),
            "sha256:abc".to_string(),
        ] {
            assert_eq!(
                repo_rule(&foreign),
                Err("selfupdate.foreign_repo"),
                "{foreign}"
            );
        }
    }
}
