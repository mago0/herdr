//! Build identity helpers.

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

/// Marks a build of this fork, so that it is not taken for the upstream release with the same
/// number. It is semver build metadata: it does not change which version is newer.
const FORK_MARK: &str = "fork";

/// The version this build shows and reports.
pub fn version() -> String {
    format!("{}+{FORK_MARK}", release_version())
}

/// The version of the upstream release this build is made from. Use it where a version names a
/// published release, such as the server to install on a remote machine.
pub fn release_version() -> String {
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
mod tests {
    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!super::version().is_empty());
    }

    #[test]
    fn the_version_of_a_fork_build_is_marked_and_its_release_version_is_not() {
        assert_eq!(
            super::version(),
            format!("{}+fork", super::release_version())
        );
        assert!(super::release_version().starts_with(super::BASE_VERSION));
        assert!(!super::release_version().contains("fork"));
    }
}
