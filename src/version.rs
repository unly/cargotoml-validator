use semver::{Comparator, Op, Version, VersionReq};

/// Returns the newest stable release if it is semver-incompatible with
/// `requirement`, i.e. upgrading would require editing `Cargo.toml`.
///
/// Cargo's compatibility rules apply: `1.x` and `2.x` are incompatible, but so
/// are `0.1.x` and `0.2.x`, and every `0.0.x` release.
pub fn newer_incompatible(
    requirement: &str,
    versions: &[Version],
) -> Result<Option<Version>, semver::Error> {
    let requirement = VersionReq::parse(requirement)?;
    let Some(base) = lower_bound(&requirement) else {
        return Ok(None);
    };

    Ok(latest_stable(versions).filter(|latest| is_newer_incompatible(base, latest)))
}

pub fn latest_stable(versions: &[Version]) -> Option<Version> {
    versions.iter().filter(|v| v.pre.is_empty()).max().cloned()
}

/// The highest comparator that sets a lower bound; upper-bound-only
/// comparators (`<`, `<=`) say nothing about the version in use.
fn lower_bound(requirement: &VersionReq) -> Option<&Comparator> {
    requirement
        .comparators
        .iter()
        .filter(|c| !matches!(c.op, Op::Less | Op::LessEq))
        .max_by_key(|c| (c.major, c.minor.unwrap_or(0), c.patch.unwrap_or(0)))
}

fn is_newer_incompatible(base: &Comparator, latest: &Version) -> bool {
    if latest.major != base.major {
        return latest.major > base.major;
    }
    if base.major > 0 {
        return false;
    }

    // 0.y.z: the minor version is the compatibility boundary.
    let Some(minor) = base.minor else {
        return false;
    };
    if latest.minor != minor {
        return latest.minor > minor;
    }
    if minor > 0 {
        return false;
    }

    // 0.0.z: every patch release is its own boundary.
    base.patch.is_some_and(|patch| latest.patch > patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(list: &[&str]) -> Vec<Version> {
        list.iter().map(|v| Version::parse(v).unwrap()).collect()
    }

    fn check(requirement: &str, available: &[&str]) -> Option<String> {
        newer_incompatible(requirement, &versions(available))
            .unwrap()
            .map(|v| v.to_string())
    }

    #[test]
    fn detects_new_major_above_one() {
        assert_eq!(
            check("1.2", &["1.2.0", "1.9.3", "2.0.1"]),
            Some("2.0.1".into())
        );
    }

    #[test]
    fn compatible_updates_are_not_reported() {
        assert_eq!(check("1.2", &["1.2.0", "1.9.3"]), None);
        assert_eq!(check("^1", &["1.0.0", "1.99.0"]), None);
    }

    #[test]
    fn zero_minor_is_a_breaking_boundary() {
        assert_eq!(check("0.3", &["0.3.9", "0.4.0"]), Some("0.4.0".into()));
        assert_eq!(check("0.3.1", &["0.3.1", "0.3.9"]), None);
    }

    #[test]
    fn bare_zero_matches_every_zero_release() {
        assert_eq!(check("0", &["0.1.0", "0.9.0"]), None);
        assert_eq!(check("0", &["0.9.0", "1.0.0"]), Some("1.0.0".into()));
    }

    #[test]
    fn zero_zero_patch_is_a_breaking_boundary() {
        assert_eq!(check("0.0.3", &["0.0.3", "0.0.5"]), Some("0.0.5".into()));
        assert_eq!(check("0.0", &["0.0.3", "0.0.5"]), None);
        assert_eq!(check("0.0.5", &["0.0.5"]), None);
    }

    #[test]
    fn prereleases_are_ignored() {
        assert_eq!(check("1", &["1.0.0", "2.0.0-rc.1"]), None);
    }

    #[test]
    fn requirement_ahead_of_registry_is_not_reported() {
        assert_eq!(check("3", &["1.0.0", "2.0.0"]), None);
    }

    #[test]
    fn ranges_use_the_lower_bound() {
        assert_eq!(
            check(">=1.2, <2", &["1.5.0", "2.3.0"]),
            Some("2.3.0".into())
        );
        assert_eq!(check(">=2.1, <3", &["2.5.0"]), None);
    }

    #[test]
    fn upper_bound_only_and_wildcard_requirements_are_skipped() {
        assert_eq!(check("<2", &["3.0.0"]), None);
        assert_eq!(check("*", &["3.0.0"]), None);
    }

    #[test]
    fn exact_and_tilde_requirements_are_supported() {
        assert_eq!(check("=1.2.3", &["1.2.3", "2.0.0"]), Some("2.0.0".into()));
        assert_eq!(check("~1.2", &["1.2.9", "1.3.0"]), None);
    }

    #[test]
    fn no_versions_yields_nothing() {
        assert_eq!(check("1", &[]), None);
    }

    #[test]
    fn invalid_requirement_is_an_error() {
        assert!(newer_incompatible("not a version", &[]).is_err());
    }
}
