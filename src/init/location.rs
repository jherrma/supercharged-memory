//! Where a live database (or the binary it is configured from) should not live.
//!
//! A pure classifier over a path and a few environment variables, so every
//! pattern is a table row rather than a claim. It only ever WARNS: the user may
//! have reasons, and `-y` proceeds with a flagged path once the agent has relayed
//! the warning.

use std::collections::HashMap;
use std::path::Path;

/// Why a place is a poor home for a live database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// Synced by the named service. Cloud sync corrupts a live SQLite file.
    Cloud(&'static str),
    /// A Downloads directory: cleaned out by hand and by tools.
    Downloads,
    /// A temporary directory: emptied on reboot or by the system.
    Temp,
}

/// The variables the classifier reads, whichever are set.
pub const VARS: [&str; 7] = [
    "XDG_DOWNLOAD_DIR",
    "TMPDIR",
    "TEMP",
    "TMP",
    // Windows sets these to the OneDrive roots. Known Folder Move puts
    // Documents and Desktop under them, whatever the folder is called.
    "OneDrive",
    "OneDriveCommercial",
    "OneDriveConsumer",
];

/// What the classifier needs from the machine.
pub struct Context<'a> {
    pub home: &'a Path,
    /// Paths are compared with `\` read as `/`, as Windows writes them.
    pub windows: bool,
    /// Any of `VARS` that are set.
    pub vars: &'a HashMap<String, String>,
}

/// The first reason `path` is a poor home, or `None`. Case-insensitive
/// throughout: macOS and Windows file systems are, and a warning that misses
/// `~/dropbox` helps nobody.
pub fn classify(path: &Path, ctx: &Context) -> Option<Place> {
    let path = normalise(&path.to_string_lossy(), ctx.windows);
    let under_var = |var: &str| {
        ctx.vars
            .get(var)
            .is_some_and(|v| under(&path, &normalise(v, ctx.windows)))
    };

    if ["OneDrive", "OneDriveCommercial", "OneDriveConsumer"]
        .into_iter()
        .any(under_var)
    {
        return Some(Place::Cloud("OneDrive"));
    }
    let parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if let Some(service) = cloud_service(&parts) {
        return Some(Place::Cloud(service));
    }

    let downloads = normalise(&ctx.home.join("Downloads").to_string_lossy(), ctx.windows);
    if under(&path, &downloads) || under_var("XDG_DOWNLOAD_DIR") {
        return Some(Place::Downloads);
    }

    if ["/tmp", "/var/tmp", "/private/tmp"]
        .into_iter()
        .any(|t| under(&path, t))
        || ["TMPDIR", "TEMP", "TMP"].into_iter().any(under_var)
    {
        return Some(Place::Temp);
    }
    None
}

/// The sync service a folder name belongs to. Names are matched whole, or by
/// prefix where the service adds a suffix of its own (`Dropbox (Firma)`,
/// `OneDrive - Contoso`).
fn cloud_service(parts: &[&str]) -> Option<&'static str> {
    for (i, part) in parts.iter().enumerate() {
        let next = parts.get(i + 1).copied();
        let service = match *part {
            p if p.starts_with("dropbox") => "Dropbox",
            p if p.starts_with("onedrive") => "OneDrive",
            "library" if next == Some("mobile documents") => "iCloud",
            "library" if next == Some("cloudstorage") && parts.len() > i + 2 => {
                "Library/CloudStorage"
            }
            "iclouddrive" | "icloud drive" => "iCloud",
            p if p.starts_with("google drive") || p.starts_with("googledrive") => "Google Drive",
            p if p.starts_with("nextcloud") => "Nextcloud",
            p if p.starts_with("owncloud") => "ownCloud",
            "sync" => "Syncthing",
            "box" | "box sync" => "Box",
            "mega" | "megasync" => "MEGA",
            p if p.starts_with("pcloud") => "pCloud",
            _ => continue,
        };
        return Some(service);
    }
    None
}

/// Lowercased, `/`-separated, without a trailing separator or Windows' verbatim
/// prefix -- one spelling per place, so prefixes compare as strings.
fn normalise(path: &str, windows: bool) -> String {
    let path = crate::claude_md::mixed_path(path, windows).to_lowercase();
    path.trim_end_matches('/').to_string()
}

/// `path` is `dir` or inside it. An empty `dir` is no directory at all.
fn under(path: &str, dir: &str) -> bool {
    !dir.is_empty()
        && path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether a file name carries a version (`supercharged-memory_0.3.0_linux_amd64`):
/// a binary renamed on the next download would leave its block and `.env`
/// pointing at a path that no longer exists.
pub fn name_carries_version(name: &str) -> bool {
    name.as_bytes()
        .windows(3)
        .any(|w| w[0].is_ascii_digit() && w[1] == b'.' && w[2].is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// (path, windows, home, variables, the expected finding)
    type Case = (
        &'static str,
        bool,
        &'static str,
        &'static [(&'static str, &'static str)],
        Option<Place>,
    );

    const UNIX: &str = "/home/u";
    const MAC: &str = "/Users/u";
    const WIN: &str = r"C:\Users\me";

    #[test]
    fn every_pattern_is_flagged() {
        let cases: &[Case] = &[
            // Cloud-synced, by folder name, in any case.
            (
                "/home/u/Dropbox/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("Dropbox")),
            ),
            (
                "/home/u/Dropbox (Firma)/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("Dropbox")),
            ),
            (
                "/home/u/DROPBOX/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("Dropbox")),
            ),
            (
                "/Users/u/OneDrive/mem.db",
                false,
                MAC,
                &[],
                Some(Place::Cloud("OneDrive")),
            ),
            (
                "/home/u/OneDrive - Contoso/x/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("OneDrive")),
            ),
            (
                r"C:\Users\me\onedrive\Documents\mem.db",
                true,
                WIN,
                &[],
                Some(Place::Cloud("OneDrive")),
            ),
            (
                "/Users/u/Library/Mobile Documents/com~apple~CloudDocs/mem.db",
                false,
                MAC,
                &[],
                Some(Place::Cloud("iCloud")),
            ),
            (
                "/Users/u/library/mobile documents/x/mem.db",
                false,
                MAC,
                &[],
                Some(Place::Cloud("iCloud")),
            ),
            (
                r"C:\Users\me\iCloudDrive\mem.db",
                true,
                WIN,
                &[],
                Some(Place::Cloud("iCloud")),
            ),
            (
                "/Users/u/Library/CloudStorage/GoogleDrive-a/My Drive/mem.db",
                false,
                MAC,
                &[],
                Some(Place::Cloud("Library/CloudStorage")),
            ),
            (
                "/home/u/Google Drive/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("Google Drive")),
            ),
            (
                "/home/u/GoogleDrive/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("Google Drive")),
            ),
            (
                "/home/u/Nextcloud/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("Nextcloud")),
            ),
            (
                "/home/u/ownCloud/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("ownCloud")),
            ),
            (
                "/home/u/Sync/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("Syncthing")),
            ),
            (
                "/home/u/Documents/sync/notes/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("Syncthing")),
            ),
            (
                "/Users/u/Box/mem.db",
                false,
                MAC,
                &[],
                Some(Place::Cloud("Box")),
            ),
            (
                "/Users/u/Box Sync/mem.db",
                false,
                MAC,
                &[],
                Some(Place::Cloud("Box")),
            ),
            (
                "/home/u/MEGA/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("MEGA")),
            ),
            (
                "/home/u/MEGAsync/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("MEGA")),
            ),
            (
                "/home/u/pCloudDrive/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Cloud("pCloud")),
            ),
            (
                "/Users/u/pCloud Drive/mem.db",
                false,
                MAC,
                &[],
                Some(Place::Cloud("pCloud")),
            ),
            // Windows: anything under a OneDrive root, whatever the folder is
            // called -- a Known-Folder-Moved Documents or Desktop included.
            (
                r"D:\Corp Root\Documents\mem.db",
                true,
                WIN,
                &[("OneDriveCommercial", r"D:\Corp Root")],
                Some(Place::Cloud("OneDrive")),
            ),
            (
                r"d:\corp root\Desktop\mem.db",
                true,
                WIN,
                &[("OneDrive", r"D:\Corp Root\")],
                Some(Place::Cloud("OneDrive")),
            ),
            (
                r"E:\Personal\mem.db",
                true,
                WIN,
                &[("OneDriveConsumer", r"E:\Personal")],
                Some(Place::Cloud("OneDrive")),
            ),
            // Downloads.
            (
                "/home/u/Downloads/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Downloads),
            ),
            (
                "/home/u/downloads/x/mem.db",
                false,
                UNIX,
                &[],
                Some(Place::Downloads),
            ),
            (
                "/data/dl/mem.db",
                false,
                UNIX,
                &[("XDG_DOWNLOAD_DIR", "/data/dl")],
                Some(Place::Downloads),
            ),
            (
                r"C:\Users\me\Downloads\mem.db",
                true,
                WIN,
                &[],
                Some(Place::Downloads),
            ),
            // The binary's own path is canonical, and canonical on Windows is
            // the verbatim form.
            (
                r"\\?\C:\Users\me\Downloads\supercharged-memory.exe",
                true,
                WIN,
                &[],
                Some(Place::Downloads),
            ),
            // Temporary directories.
            ("/tmp/mem.db", false, UNIX, &[], Some(Place::Temp)),
            ("/tmp", false, UNIX, &[], Some(Place::Temp)),
            ("/var/tmp/x/mem.db", false, UNIX, &[], Some(Place::Temp)),
            (
                "/var/folders/ab/T/mem.db",
                false,
                MAC,
                &[("TMPDIR", "/var/folders/ab/T/")],
                Some(Place::Temp),
            ),
            (
                r"C:\Users\me\AppData\Local\Temp\mem.db",
                true,
                WIN,
                &[("TEMP", r"C:\Users\me\AppData\Local\Temp")],
                Some(Place::Temp),
            ),
            (
                r"c:\users\ME\appdata\local\temp\mem.db",
                true,
                WIN,
                &[("TMP", r"C:\Users\me\AppData\Local\Temp")],
                Some(Place::Temp),
            ),
        ];
        check(cases);
    }

    #[test]
    fn ordinary_places_are_not_flagged() {
        let cases: &[Case] = &[
            ("/home/u/Documents/notes/mem.db", false, UNIX, &[], None),
            ("/home/u/.local/share/turso/mem.db", false, UNIX, &[], None),
            ("/home/u/.local/share", false, UNIX, &[], None),
            // A name that merely contains a pattern is not that folder.
            ("/home/u/syncthing-notes/mem.db", false, UNIX, &[], None),
            ("/home/u/Downloads-archive/mem.db", false, UNIX, &[], None),
            ("/home/u/tmp/mem.db", false, UNIX, &[], None),
            ("/home/u/boxes/mem.db", false, UNIX, &[], None),
            (
                r"C:\Users\me\AppData\Local\turso\mem.db",
                true,
                WIN,
                &[],
                None,
            ),
            // A OneDrive variable only covers what is under it.
            (
                r"C:\Users\me\Documents\mem.db",
                true,
                WIN,
                &[("OneDrive", r"C:\Users\me\OneDrive")],
                None,
            ),
            (
                "/data/dlx/mem.db",
                false,
                UNIX,
                &[("XDG_DOWNLOAD_DIR", "/data/dl")],
                None,
            ),
            // An empty variable is no prefix at all.
            ("/home/u/mem.db", false, UNIX, &[("TMPDIR", "")], None),
        ];
        check(cases);
    }

    fn check(cases: &[Case]) {
        for (path, windows, home, pairs, want) in cases {
            let vars = vars(pairs);
            let ctx = Context {
                home: Path::new(home),
                windows: *windows,
                vars: &vars,
            };
            assert_eq!(
                classify(Path::new(path), &ctx),
                *want,
                "{path} (windows={windows}, vars={pairs:?})"
            );
        }
    }

    #[test]
    fn a_version_in_the_file_name_is_noticed() {
        for yes in [
            "supercharged-memory_0.3.0_linux_amd64",
            "supercharged-memory-0.3.0.exe",
            "supercharged-memory_1.10.2",
        ] {
            assert!(name_carries_version(yes), "{yes}");
        }
        for no in [
            "supercharged-memory",
            "supercharged-memory.exe",
            "supercharged-memory-linux",
            "sm2",
        ] {
            assert!(!name_carries_version(no), "{no}");
        }
    }
}
