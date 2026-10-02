//! Build identity helpers.

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// drovr's own version for humans (`--version`, sidebar header), baked by
/// build.rs: the release tag, else `git describe`, else `<crate>+dev`. The
/// herdr `version()` stays the one used by handshakes, status and updates.
pub const DROVR_VERSION: &str = env!("DROVR_VERSION");

/// `drovr --version` line: `drovr 0.9.3-3 (herdr 0.9.3)`.
pub fn version_line() -> String {
    format!("drovr {DROVR_VERSION} (herdr {})", version())
}

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

pub fn version() -> String {
    match channel() {
        "stable" => BASE_VERSION.to_string(),
        channel => match build_id() {
            Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
            None => format!("{BASE_VERSION}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
#[path = "drovr_version.rs"]
mod drovr_version;

#[cfg(test)]
mod tests {
    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!super::version().is_empty());
    }

    #[test]
    fn version_line_names_drovr_then_herdr() {
        let line = super::version_line();
        assert!(line.starts_with("drovr "), "{line}");
        assert!(
            line.ends_with(&format!(" (herdr {})", super::version())),
            "{line}"
        );
    }
}
