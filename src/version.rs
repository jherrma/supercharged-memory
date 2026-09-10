//! `--version` output: release version, the commit it was built from, the build
//! date, and the linked Turso SDK version. All four are stamped by build.rs.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMIT: &str = env!("SM_COMMIT");
pub const BUILD_DATE: &str = env!("SM_BUILD_DATE");
pub const TURSO_VERSION: &str = env!("SM_TURSO_VERSION");

/// Multi-line version string. UPDATE.md reads the commit line to detect a
/// repository that moved without its binary.
pub fn long_version() -> String {
    format!(
        "{VERSION}\n  commit:    {COMMIT}\n  built:     {BUILD_DATE}\n  turso sdk: {TURSO_VERSION}"
    )
}

/// The same string, leaked once so clap can hold a `&'static str`.
pub fn long_version_static() -> &'static str {
    static LONG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LONG.get_or_init(long_version).as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_is_stamped() {
        for (name, value) in [
            ("version", VERSION),
            ("commit", COMMIT),
            ("build date", BUILD_DATE),
            ("turso version", TURSO_VERSION),
        ] {
            assert!(!value.is_empty(), "{name} was not stamped by build.rs");
        }
    }

    #[test]
    fn long_version_carries_the_commit_update_md_greps_for() {
        assert!(long_version().contains(COMMIT));
    }
}
