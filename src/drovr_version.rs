//! drovr version derivation, shared by build.rs (via `#[path]`) and the
//! crate's tests. build.rs bakes the result into `DROVR_VERSION`.

/// Version from a release tag: `drovr-v0.9.3-3` -> `0.9.3-3`.
pub fn from_release_tag(tag: &str) -> Option<String> {
    let version = tag.trim().strip_prefix("drovr-v")?;
    (!version.is_empty()).then(|| version.to_string())
}

/// Version from `git describe --tags --long --match 'drovr-v*' --dirty`:
/// `drovr-v0.9.3-3-1-g91a9d55-dirty` -> `0.9.3-3+1.dirty`. `+N` counts the
/// commits after the tag and is left out when N is 0 on a clean tree.
pub fn from_describe(describe: &str) -> Option<String> {
    let describe = describe.trim();
    let (describe, dirty) = match describe.strip_suffix("-dirty") {
        Some(rest) => (rest, true),
        None => (describe, false),
    };
    // The tag itself contains '-', so split the two known fields off the end.
    let mut parts = describe.rsplitn(3, '-');
    let hash = parts.next()?;
    let ahead: u32 = parts.next()?.parse().ok()?;
    let version = from_release_tag(parts.next()?)?;
    if !hash.starts_with('g') {
        return None;
    }
    Some(match (ahead, dirty) {
        (0, false) => version,
        (ahead, false) => format!("{version}+{ahead}"),
        (ahead, true) => format!("{version}+{ahead}.dirty"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tag_and_describe_render_drovr_versions() {
        assert_eq!(
            from_release_tag("drovr-v0.9.3-3").as_deref(),
            Some("0.9.3-3")
        );
        assert_eq!(from_release_tag("v0.9.3"), None);
        assert_eq!(from_release_tag("drovr-v"), None);
        let cases = [
            ("drovr-v0.9.3-3-0-g91a9d55", Some("0.9.3-3")),
            ("drovr-v0.9.3-3-1-g91a9d55e", Some("0.9.3-3+1")),
            ("drovr-v0.9.3-3-1-g91a9d55-dirty", Some("0.9.3-3+1.dirty")),
            ("drovr-v0.9.3-3-0-g91a9d55-dirty", Some("0.9.3-3+0.dirty")),
            ("drovr-v1.0.0-12-gabc\n", Some("1.0.0+12")),
            ("91a9d55", None),
            ("drovr-v0.9.3-3-x-g91a9d55", None),
        ];
        for (describe, expected) in cases {
            assert_eq!(from_describe(describe).as_deref(), expected, "{describe}");
        }
    }
}
