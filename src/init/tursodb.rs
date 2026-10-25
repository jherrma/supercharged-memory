//! tursodb and the `turso` MCP server: optional, and only ever detected.
//!
//! With them the agent can run broad ad-hoc reads; without them it has the CLI's
//! fixed retrieval commands. Nothing here writes: `~/.claude.json` is read, never
//! written, and the registration is printed for the agent to run once the user
//! agrees.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The tursodb CLI as found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tursodb {
    pub path: PathBuf,
    /// The first line of `tursodb --version`, if it answered.
    pub version: Option<String>,
}

/// `TURSO_BIN` when that file exists, else `tursodb` on `PATH`.
pub fn find(turso_bin: &Path, path_var: Option<&OsStr>, windows: bool) -> Option<PathBuf> {
    if turso_bin.is_file() {
        return Some(turso_bin.to_path_buf());
    }
    let name = if windows { "tursodb.exe" } else { "tursodb" };
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Ask a found tursodb for its version. Read-only, like everything here.
pub fn version(path: &Path) -> Option<String> {
    let out = std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// What is wrong with an existing registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// Its arguments do not name this database. A registration on another path
    /// re-creates a file there the moment the MCP server starts.
    OtherDatabase,
    /// No `--experimental-multiprocess-wal`: the server takes an exclusive lock
    /// and every command of this binary is refused while it runs.
    NoMultiprocessWal,
    /// Windows without `--vfs experimental_win_iocp`: the default IO backend
    /// refuses multiprocess WAL outright.
    NoWindowsVfs,
    /// Without `--readonly` the MCP's write tools work, and "writes go through
    /// the CLI" stops being enforced.
    NotReadonly,
}

impl Problem {
    pub fn describe(self) -> &'static str {
        match self {
            Problem::OtherDatabase => {
                "its arguments do not name this database — a registration on another path \
                 re-creates a file there when the server starts"
            }
            Problem::NoMultiprocessWal => {
                "no --experimental-multiprocess-wal — the server takes an exclusive lock and \
                 every command of this binary is refused while it runs"
            }
            Problem::NoWindowsVfs => {
                "no --vfs experimental_win_iocp — Windows' default IO backend refuses \
                 multiprocess WAL"
            }
            Problem::NotReadonly => {
                "no --readonly — the MCP's write tools work, so \"writes go through the CLI\" \
                 is only advisory"
            }
        }
    }
}

/// The user-scope `turso` registration in `~/.claude.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mcp {
    /// No such file, or no `mcpServers.turso` in it.
    NotRegistered,
    /// Registered; an empty list means exactly as recommended.
    Registered(Vec<Problem>),
    /// The file could not be read, or is not shaped the way Claude Code writes
    /// it. Never reported as "not registered": that would invite a second,
    /// conflicting registration.
    Unknown(String),
}

/// Judge the registration in `claude_json` (`None` = no file, `Err` = the file
/// could not be read) against the database `db`.
pub fn check(claude_json: Option<&Result<String, String>>, db: &Path, windows: bool) -> Mcp {
    let text = match claude_json {
        None => return Mcp::NotRegistered,
        Some(Err(why)) => return Mcp::Unknown(format!("it cannot be read: {why}")),
        Some(Ok(text)) => text,
    };
    let json: serde_json::Value = match serde_json::from_str(text) {
        Ok(json) => json,
        Err(e) => return Mcp::Unknown(format!("it is not valid JSON: {e}")),
    };
    let Some(top) = json.as_object() else {
        return Mcp::Unknown("it is not a JSON object".into());
    };
    let Some(servers) = top.get("mcpServers") else {
        return Mcp::NotRegistered;
    };
    let Some(servers) = servers.as_object() else {
        return Mcp::Unknown("its mcpServers is not an object".into());
    };
    let Some(turso) = servers.get("turso") else {
        return Mcp::NotRegistered;
    };
    let args: Option<Vec<&str>> = turso
        .get("args")
        .and_then(|a| a.as_array())
        .and_then(|a| a.iter().map(|v| v.as_str()).collect());
    let Some(args) = args else {
        return Mcp::Unknown("mcpServers.turso has no list of string arguments".into());
    };

    let same_path = |arg: &str| {
        let db = db.to_string_lossy();
        if windows {
            // Either slash, any case: the same file to Windows.
            arg.replace('\\', "/").to_lowercase() == db.replace('\\', "/").to_lowercase()
        } else {
            arg == db
        }
    };
    let mut problems = Vec::new();
    if !args.iter().any(|a| same_path(a)) {
        problems.push(Problem::OtherDatabase);
    }
    if !args.contains(&"--experimental-multiprocess-wal") {
        problems.push(Problem::NoMultiprocessWal);
    }
    let vfs = args
        .windows(2)
        .any(|w| w == ["--vfs", "experimental_win_iocp"])
        || args.contains(&"--vfs=experimental_win_iocp");
    if windows && !vfs {
        problems.push(Problem::NoWindowsVfs);
    }
    if !args.contains(&"--readonly") {
        problems.push(Problem::NotReadonly);
    }
    Mcp::Registered(problems)
}

/// The registration to recommend, for the agent to run once the user agrees.
/// `tursodb` is `None` when it is not installed yet.
pub fn register_command(tursodb: Option<&Path>, db: &Path, windows: bool) -> String {
    use crate::claude_md::shell_word;
    let bin = tursodb
        .map(|p| shell_word(&p.to_string_lossy()))
        .unwrap_or_else(|| "<path to tursodb>".into());
    let mut cmd = format!(
        "claude mcp add turso --scope user -- {bin} {} --mcp --readonly \
         --experimental-multiprocess-wal",
        shell_word(&db.to_string_lossy())
    );
    if windows {
        cmd.push_str(" --vfs experimental_win_iocp");
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const DB: &str = "/home/u/.local/share/turso/supercharged-memory.db";

    fn registration(args: &[&str]) -> Result<String, String> {
        Ok(serde_json::json!({
            "numStartups": 3,
            "mcpServers": {
                "turso": {
                    "type": "stdio",
                    "command": "/home/u/.turso/tursodb",
                    "args": args,
                    "env": {}
                }
            }
        })
        .to_string())
    }

    fn checked(file: Option<Result<String, String>>, windows: bool) -> Mcp {
        check(file.as_ref(), Path::new(DB), windows)
    }

    #[test]
    fn no_file_or_no_entry_is_not_registered() {
        assert_eq!(checked(None, false), Mcp::NotRegistered);
        assert_eq!(
            checked(Some(Ok("{\"numStartups\": 1}".into())), false),
            Mcp::NotRegistered
        );
        assert_eq!(
            checked(
                Some(Ok("{\"mcpServers\": {\"other\": {\"args\": []}}}".into())),
                false
            ),
            Mcp::NotRegistered
        );
    }

    #[test]
    fn a_registration_as_recommended_has_no_problem() {
        let args = [DB, "--mcp", "--readonly", "--experimental-multiprocess-wal"];
        assert_eq!(
            checked(Some(registration(&args)), false),
            Mcp::Registered(vec![])
        );
        let mut windows = args.to_vec();
        windows.extend(["--vfs", "experimental_win_iocp"]);
        assert_eq!(
            checked(Some(registration(&windows)), true),
            Mcp::Registered(vec![])
        );
    }

    #[test]
    fn each_problem_is_named() {
        let cases: &[(&[&str], bool, &[Problem])] = &[
            (
                &[DB, "--mcp", "--readonly"],
                false,
                &[Problem::NoMultiprocessWal],
            ),
            (
                &[
                    "/elsewhere/mem.db",
                    "--mcp",
                    "--readonly",
                    "--experimental-multiprocess-wal",
                ],
                false,
                &[Problem::OtherDatabase],
            ),
            // This machine's own registration, as Claude Code stored it.
            (
                &[DB, "--mcp", "--experimental-multiprocess-wal"],
                false,
                &[Problem::NotReadonly],
            ),
            (
                &[DB, "--mcp", "--readonly", "--experimental-multiprocess-wal"],
                true,
                &[Problem::NoWindowsVfs],
            ),
            (
                &["/x.db", "--mcp"],
                true,
                &[
                    Problem::OtherDatabase,
                    Problem::NoMultiprocessWal,
                    Problem::NoWindowsVfs,
                    Problem::NotReadonly,
                ],
            ),
        ];
        for (args, windows, want) in cases {
            assert_eq!(
                checked(Some(registration(args)), *windows),
                Mcp::Registered(want.to_vec()),
                "{args:?} windows={windows}"
            );
        }
        for p in [
            Problem::OtherDatabase,
            Problem::NoMultiprocessWal,
            Problem::NoWindowsVfs,
            Problem::NotReadonly,
        ] {
            assert!(!p.describe().is_empty(), "{p:?}");
        }
    }

    #[test]
    fn an_unreadable_or_unrecognised_file_is_unknown_never_not_registered() {
        for file in [
            Err("Permission denied".to_string()),
            Ok("{ not json".to_string()),
            Ok("[1, 2]".to_string()),
            Ok("{\"mcpServers\": [\"turso\"]}".to_string()),
            Ok("{\"mcpServers\": {\"turso\": \"tursodb\"}}".to_string()),
            Ok("{\"mcpServers\": {\"turso\": {\"command\": \"tursodb\"}}}".to_string()),
            Ok("{\"mcpServers\": {\"turso\": {\"args\": [1, 2]}}}".to_string()),
        ] {
            assert!(
                matches!(checked(Some(file.clone()), false), Mcp::Unknown(_)),
                "{file:?}"
            );
        }
    }

    #[test]
    fn the_recommended_registration_is_readonly_with_multiprocess_wal() {
        assert_eq!(
            register_command(
                Some(Path::new("/home/u/.turso/tursodb")),
                Path::new(DB),
                false
            ),
            format!(
                "claude mcp add turso --scope user -- /home/u/.turso/tursodb {DB} --mcp \
                 --readonly --experimental-multiprocess-wal"
            )
        );
        assert_eq!(
            register_command(
                Some(Path::new("C:/Users/me/.turso/tursodb.exe")),
                Path::new("C:/Users/me/My Data/mem.db"),
                true
            ),
            "claude mcp add turso --scope user -- C:/Users/me/.turso/tursodb.exe \
             \"C:/Users/me/My Data/mem.db\" --mcp --readonly --experimental-multiprocess-wal \
             --vfs experimental_win_iocp"
        );
        assert!(
            register_command(None, Path::new(DB), false).contains("-- <path to tursodb> "),
            "{}",
            register_command(None, Path::new(DB), false)
        );
    }

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sm-tursodb-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tursodb_is_found_at_turso_bin_or_on_path() {
        let dir = tmpdir("find");
        let pinned = dir.join("pinned/tursodb");
        let on_path = dir.join("bin/tursodb");
        fs::create_dir_all(pinned.parent().unwrap()).unwrap();
        fs::create_dir_all(on_path.parent().unwrap()).unwrap();
        fs::write(&on_path, b"").unwrap();
        let path_var = std::env::join_paths([dir.join("empty"), dir.join("bin")]).unwrap();

        // TURSO_BIN does not exist: PATH decides.
        assert_eq!(find(&pinned, Some(&path_var), false), Some(on_path.clone()));
        // TURSO_BIN exists: it wins.
        fs::write(&pinned, b"").unwrap();
        assert_eq!(find(&pinned, Some(&path_var), false), Some(pinned.clone()));
        // Neither.
        fs::remove_file(&pinned).unwrap();
        assert_eq!(find(&pinned, None, false), None);
        assert_eq!(
            find(
                &pinned,
                Some(&std::env::join_paths([dir.join("empty")]).unwrap()),
                false
            ),
            None
        );
    }
}
