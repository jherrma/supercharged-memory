//! The CLI surface: every subcommand the Python scripts exposed, plus dispatch.
//!
//! Each variant carries the constraint its implementation must honour, so the brief
//! is next to the signature.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};

use crate::config::{self, Config, ConfigError, Env};
use crate::error::{EXIT_OK, EXIT_USAGE, Result};
use crate::remember;
use crate::store;
use crate::{
    backfill, backup, coworkers, embed, eval, find_existing, init, instructions, recall, restore,
    rows, runbook, sleep, status, update, version,
};

#[derive(Parser, Debug)]
#[command(
    name = "supercharged-memory",
    about = "Long-term memory for AI coding agents",
    version = version::VERSION,
    long_version = version::long_version_static(),
    // Unknown flags are rejected, never ignored: a typo'd flag that is silently
    // dropped turns a refusal into a no-op that looks like success.
    disable_help_subcommand = false,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// clap's view of `--table`, kept separate from the domain enum so the CLI can
/// change wording without touching the search path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TableArg {
    Semantic,
    Episodic,
    Both,
}

/// `--table` where exactly one table is meant (the write side), as opposed to
/// `recall`'s three-way choice that also allows both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OneTable {
    Semantic,
    Episodic,
}

impl From<OneTable> for store::Table {
    fn from(t: OneTable) -> Self {
        match t {
            OneTable::Semantic => store::Table::Semantic,
            OneTable::Episodic => store::Table::Episodic,
        }
    }
}

/// `list` / `show --table`: any one of the tables that hold rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReadTable {
    Semantic,
    Episodic,
    Instructions,
}

impl From<ReadTable> for store::Table {
    fn from(t: ReadTable) -> Self {
        match t {
            ReadTable::Semantic => store::Table::Semantic,
            ReadTable::Episodic => store::Table::Episodic,
            ReadTable::Instructions => store::Table::Instructions,
        }
    }
}

/// `embed --table`: one table that stores an embedding, or all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EmbedTable {
    Semantic,
    Episodic,
    Instructions,
    All,
}

impl EmbedTable {
    fn tables(self) -> Vec<store::Table> {
        match self {
            EmbedTable::Semantic => vec![store::Table::Semantic],
            EmbedTable::Episodic => vec![store::Table::Episodic],
            EmbedTable::Instructions => vec![store::Table::Instructions],
            EmbedTable::All => store::Table::EMBEDDED.to_vec(),
        }
    }
}

impl From<TableArg> for recall::TableArg {
    fn from(t: TableArg) -> Self {
        match t {
            TableArg::Semantic => recall::TableArg::Semantic,
            TableArg::Episodic => recall::TableArg::Episodic,
            TableArg::Both => recall::TableArg::Both,
        }
    }
}

// One Command is built per process, and it is the process's entire argument
// state -- there is no array of them and nothing is cloned per row, so the size
// of the largest variant costs nothing worth boxing for.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Report database health: MISSING | EMPTY | DEGRADED n | ERROR | READY n.
    ///
    /// The vocabulary is a contract: CLAUDE.md.template tells the agent to react
    /// to each state differently, and MISSING must never be read as lost data.
    Status,

    /// Search memory, or load a session-start slot.
    ///
    /// `score = vector_distance_cos - RECALL_ALPHA * kw`, lowest first. A PURE
    /// READER: no hit counters, no last-accessed column. Ranking by true
    /// "hotness" would make every query a write, and that trade was refused.
    Recall {
        /// What to search for. Omit it when using one of the session-start slots.
        query: Option<String>,

        /// Which memories to search.
        #[arg(long, value_enum, default_value_t = TableArg::Both)]
        table: TableArg,

        /// Narrow to one tracking-tool work item id.
        #[arg(long)]
        project: Option<String>,

        /// How many hits per table.
        #[arg(long, default_value_t = 5)]
        k: u32,

        /// Scope to memories visible to this coworker persona.
        #[arg(long)]
        coworker: Option<String>,

        /// Load the must-always-apply rules (pure SQL; works without Ollama).
        #[arg(long)]
        baseline: bool,

        /// Load the topic index (pure SQL; load every session, like --baseline).
        /// With a word, only the topics whose name or keywords contain it,
        /// ignoring case.
        #[arg(long, value_name = "WORD", num_args = 0..=1, default_missing_value = "")]
        topics: Option<String>,

        /// List memory databases and backups found outside the configured path.
        #[arg(long)]
        candidates: bool,

        /// Print the total memory count and nothing else.
        #[arg(long)]
        count: bool,
    },

    /// Store one memory, or supersede existing ones.
    ///
    /// Owns every mechanical guard that keeps the corpus clean: near-duplicate
    /// refusal, the 2000-char cap, one embedding model per database, the
    /// baseline confirmation, the --created-at format.
    Remember {
        /// Timeless fact (semantic) or time-anchored event (episodic).
        #[arg(long, value_enum)]
        table: OneTable,

        /// The memory itself. NEVER PII — anonymize; store pointers instead.
        #[arg(long)]
        text: String,

        /// Short headline. Shown by recall and used to group a sleep pass.
        #[arg(long)]
        topic: Option<String>,

        /// Tracking-tool work item id. Omit if the memory is not ticket-bound.
        #[arg(long)]
        project: Option<String>,

        /// Appended INTO the text, so they are embedded and LIKE-searchable —
        /// and so they count against the 2000-char cap.
        #[arg(long)]
        keywords: Option<String>,

        /// Where the memory came from (user-stated, self-observed, migration…).
        #[arg(long)]
        source: Option<String>,

        /// The agent model that wrote it.
        #[arg(long)]
        model: Option<String>,

        /// Path this memory was derived from; the audit trail for an import.
        #[arg(long = "file-reference")]
        file_reference: Option<String>,

        /// 'YYYY-MM-DD HH:MM:SS', or a bare 'YYYY-MM-DD' (stored as that day at
        /// 00:00:00). Anything else is refused; omit the flag to date the row now.
        #[arg(long = "created-at")]
        created_at: Option<String>,

        /// Semantic only: baseline, user, feedback, project, reference, pattern.
        #[arg(long)]
        category: Option<String>,

        /// Episodic only: project_start, bug_fix, feature_complete, decision,
        /// milestone, incident, note.
        #[arg(long = "event-type")]
        event_type: Option<String>,

        /// Episodic only: routine, notable, major.
        #[arg(long)]
        importance: Option<String>,

        /// Required to store a 'baseline' memory: those load every session.
        #[arg(long = "confirm-baseline")]
        confirm_baseline: bool,

        /// Store even if a near-duplicate already exists.
        #[arg(long)]
        force: bool,

        /// Comma-separated id(s) of the semantic row(s) this replaces: inserts the
        /// new row and marks all of them superseded by it, in one transaction.
        #[arg(long)]
        supersedes: Option<String>,

        /// Comma-separated coworker name(s) to scope this memory to.
        #[arg(long)]
        coworker: Option<String>,
    },

    /// Store an instruction the user wants followed on demand; revise or retire one.
    ///
    /// Directives, not memories: never loaded at session start, never read by
    /// sleep. Shares remember's guards -- empty text, the 2000-char cap with
    /// the keywords in it, metadata caps, one embedding model across every
    /// table, the near-duplicate check -- and stores without an embedding when
    /// none is available. --replaces retires the old row in the same
    /// transaction; --retire soft-deletes one. Nothing is ever deleted.
    #[command(group(ArgGroup::new("retiring").args(["retire"])))]
    Instruct {
        /// The name the user says ("use instruction <topic>"). Several current
        /// instructions may share one.
        #[arg(long, required_unless_present = "retiring")]
        topic: Option<String>,

        /// The directive itself. NEVER PII.
        #[arg(long, required_unless_present = "retiring")]
        text: Option<String>,

        /// Appended INTO the text, so they are embedded and LIKE-searchable —
        /// and so they count against the 2000-char cap.
        #[arg(long)]
        keywords: Option<String>,

        /// Where it came from (user-stated, …).
        #[arg(long)]
        source: Option<String>,

        /// The agent model that wrote it.
        #[arg(long)]
        model: Option<String>,

        /// Tracking-tool work item id. Omit for a global instruction.
        #[arg(long)]
        project: Option<String>,

        /// Path this instruction was derived from.
        #[arg(long = "file-reference")]
        file_reference: Option<String>,

        /// Store even if a near-duplicate already exists.
        #[arg(long)]
        force: bool,

        /// Id of the current instruction this one replaces: it is retired in
        /// the same transaction as the insert.
        #[arg(long, value_name = "ID")]
        replaces: Option<i64>,

        /// Soft-delete one current instruction, with no replacement: sets
        /// retired_at, never deletes.
        #[arg(
            long,
            value_name = "ID",
            conflicts_with_all = [
                "topic", "text", "keywords", "source", "model", "project",
                "file_reference", "force", "replaces",
            ]
        )]
        retire: Option<i64>,
    },

    /// Look up instructions: a search, one by --get <id|topic>, or --list.
    ///
    /// A PURE READER, never loaded at session start. The search ranks like
    /// recall's, over current instructions only; --get prints the full text,
    /// newest first; --list prints id, topic and date, never the text.
    #[command(
        group(ArgGroup::new("lookup").required(true).args(["query", "get", "list"])),
        group(ArgGroup::new("searching").args(["query"]))
    )]
    Instructions {
        /// What to search for.
        query: Option<String>,

        /// The current instructions with this id or topic, full text, newest
        /// first. A number is an id first, then a topic; a topic is matched
        /// whole, ignoring ASCII case.
        #[arg(long, value_name = "ID|TOPIC")]
        get: Option<String>,

        /// Id, topic and date of every current instruction.
        #[arg(long)]
        list: bool,

        /// How many hits (default 5).
        #[arg(long, requires = "searching", value_parser = clap::value_parser!(u32).range(1..))]
        k: Option<u32>,

        /// Narrow the search to one tracking-tool work item id.
        #[arg(long, requires = "searching")]
        project: Option<String>,
    },

    /// Metadata of the rows matching the filters, oldest first; never the text.
    ///
    /// A fixed set of filters, all of which must hold: the reads the runbooks
    /// need without a SQL client. An empty result is an answer, not an error.
    /// Pure reader: no Ollama, nothing written.
    List {
        /// Which table.
        #[arg(long, value_enum)]
        table: ReadTable,

        /// Only these ids (comma-separated).
        #[arg(long, value_name = "ID,ID,...")]
        ids: Option<String>,

        /// Current rows: not superseded and not retired (instructions: not
        /// retired; every episodic row is current).
        #[arg(long, conflicts_with = "not_current")]
        current: bool,

        /// Superseded or retired rows.
        #[arg(long = "not-current")]
        not_current: bool,

        /// Episodic rows no sleep pass has sifted yet.
        #[arg(long)]
        unprocessed: bool,

        /// Exactly this topic.
        #[arg(long)]
        topic: Option<String>,

        /// Exactly this category (semantic only).
        #[arg(long)]
        category: Option<String>,

        /// Exactly this source.
        #[arg(long)]
        source: Option<String>,

        /// Exactly this work item id.
        #[arg(long)]
        project: Option<String>,

        /// Created at or after this: `YYYY-MM-DD HH:MM:SS` or `YYYY-MM-DD`.
        #[arg(long, value_name = "DATE")]
        since: Option<String>,

        /// Add the first N characters of the text.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        preview: Option<u32>,

        /// Add the `Keywords:` line `remember` appended to the text.
        #[arg(long)]
        keywords: bool,
    },

    /// Print whole rows, text included and embedding never.
    ///
    /// In the order given, current or not. An id that is not in the table is
    /// named on stderr and the exit is non-zero, so a partial read cannot pass
    /// for a complete one.
    Show {
        /// Which table.
        #[arg(long, value_enum)]
        table: ReadTable,

        /// The ids, comma-separated.
        #[arg(value_name = "ID,ID,...")]
        ids: String,
    },

    /// Fill in the embeddings of rows stored without one.
    ///
    /// Active rows only, embedded from their stored text. Refuses without Ollama
    /// and the model, or when the database holds another model. Each batch
    /// commits on its own, so an interrupted run resumes where it stopped; a
    /// run that leaves no active row unembedded switches EMBEDDINGS=off in the
    /// .env back on.
    Embed {
        /// Embed the active rows that have no embedding. Required: it is the
        /// only mode.
        #[arg(long, required = true)]
        missing: bool,

        /// Which table to fill.
        #[arg(long, value_enum, default_value_t = EmbedTable::All)]
        table: EmbedTable,

        /// Stop after this many rows, across tables.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        limit: Option<u32>,

        /// Rows per transaction.
        #[arg(
            long,
            default_value_t = embed::missing::DEFAULT_BATCH,
            value_parser = clap::value_parser!(u32).range(1..)
        )]
        batch: u32,
    },

    /// Coworker personas: add, appraise, set trust, retire, reactivate; list and load.
    ///
    /// Exactly one mode per run. An appraisal supersedes the current one in
    /// the same transaction, so one is current per coworker. --retire sets
    /// active=0; nothing is ever deleted. Trust levels: supervised, trusted,
    /// autonomous; none loosens a baseline rule. --load prints the coworker,
    /// its current appraisal and the current feedback tagged to it since then;
    /// a retired coworker's load says so on its first line.
    #[command(
        group(
            ArgGroup::new("mode")
                .required(true)
                .args(["add", "appraise", "set_trust", "retire", "reactivate", "list", "load"])
        ),
        // One-member groups: clap counts a required ARGUMENT as satisfied when
        // it conflicts with one that is present, and every other mode
        // conflicts with it through `mode`.
        group(ArgGroup::new("adding").args(["add"])),
        group(ArgGroup::new("appraising").args(["appraise"]))
    )]
    Coworkers {
        /// Add a coworker (with --name, --expertise, --personality). It starts
        /// active and supervised.
        #[arg(long, requires_all = ["name", "expertise", "personality"])]
        add: bool,

        /// --add: the name, unique forever (at most 64 characters, no comma).
        #[arg(long, requires = "adding")]
        name: Option<String>,

        /// --add: what they are good at (at most 3000 characters).
        #[arg(long, requires = "adding")]
        expertise: Option<String>,

        /// --add: tone, biases, what they push back on (at most 3000 characters).
        #[arg(long, requires = "adding")]
        personality: Option<String>,

        /// Appraise a coworker (with --trust and --text): supersedes the
        /// current appraisal and sets the trust level, in one transaction.
        #[arg(long, value_name = "NAME", requires_all = ["trust", "text"])]
        appraise: Option<String>,

        /// --appraise: supervised, trusted or autonomous.
        #[arg(long, value_name = "LEVEL", requires = "appraising")]
        trust: Option<String>,

        /// --appraise: the distilled appraisal (at most 2000 characters).
        #[arg(long, requires = "appraising")]
        text: Option<String>,

        /// Set a coworker's trust level without an appraisal.
        #[arg(long = "set-trust", num_args = 2, value_names = ["NAME", "LEVEL"])]
        set_trust: Option<Vec<String>>,

        /// Retire a coworker: sets active=0, never deletes.
        #[arg(long, value_name = "NAME")]
        retire: Option<String>,

        /// Make a retired coworker active again.
        #[arg(long, value_name = "NAME")]
        reactivate: Option<String>,

        /// Id, name, expertise, trust level and active flag of every coworker.
        #[arg(long)]
        list: bool,

        /// The coworker, its current appraisal, and the current feedback
        /// tagged to it since that appraisal (or since it was added).
        #[arg(long, value_name = "NAME")]
        load: Option<String>,
    },

    /// Sleep-pass primitives: mark-processed, retire, rebuild-topics, purge, and read-only reports.
    ///
    /// Deliberately dumb -- judgment stays with the agent driving the sleep
    /// runbooks. Exactly one mode per run. --purge is the only hard-delete path
    /// in the system and refuses without --confirm-purge, without a backup
    /// newer than the database, and for any row that is current, unknown or
    /// still pointed at. The reports print JSON and never write. None of it
    /// touches the instructions table.
    #[command(
        group(
            ArgGroup::new("mode")
                .required(true)
                .args([
                    "mark_processed",
                    "retire",
                    "rebuild_topics",
                    "purge",
                    "cluster",
                    "verify_candidates",
                    "contradiction_candidates",
                    "staleness",
                ])
        ),
        group(
            ArgGroup::new("report")
                .multiple(true)
                .args(["cluster", "verify_candidates", "contradiction_candidates", "staleness"])
        ),
        group(
            ArgGroup::new("vectors")
                .multiple(true)
                .args(["cluster", "contradiction_candidates"])
        ),
        // One-member groups, because clap counts a required ARGUMENT as
        // satisfied when it conflicts with one that is present -- and every
        // other mode conflicts with it through `mode`. A required GROUP gets no
        // such exemption, so `--retire 1 --confirm-purge` is still an error.
        group(ArgGroup::new("purging").args(["purge"])),
        group(ArgGroup::new("listing").args(["verify_candidates"]))
    )]
    Sleep {
        /// Stamp processed_at on these episodic rows, kept and discarded alike.
        /// Refused whole if one id is not an episodic row.
        #[arg(long = "mark-processed", value_name = "ID[,ID...]")]
        mark_processed: Option<String>,

        /// Soft-delete one current semantic memory: sets retired_at, never deletes.
        #[arg(long, value_name = "ID")]
        retire: Option<i64>,

        /// Replace the topic index wholesale from a JSON array on stdin,
        /// [{"topic": "...", "keywords": "..."}]: a full replace, never a merge.
        /// Refused over 500 characters in total.
        #[arg(long = "rebuild-topics")]
        rebuild_topics: bool,

        /// HARD-DELETE these superseded or retired semantic rows (deep sleep
        /// only). Pass every id in ONE call: a purge is a write, so a second
        /// call fails the backup check.
        #[arg(long, value_name = "ID[,ID...]")]
        purge: Option<String>,

        /// The acknowledgement that --purge is irreversible.
        #[arg(long = "confirm-purge", requires = "purging")]
        confirm_purge: bool,

        /// Print clusters of rows whose stored vectors lie within --threshold, as JSON.
        #[arg(long)]
        cluster: bool,

        /// Print the active rows quoting a checkable artifact, oldest first, as JSON.
        #[arg(long = "verify-candidates")]
        verify_candidates: bool,

        /// Print pairs of rows close enough to possibly contradict, as JSON.
        #[arg(long = "contradiction-candidates")]
        contradiction_candidates: bool,

        /// Print the age profile of the active rows, and the unembedded count, as JSON.
        #[arg(long)]
        staleness: bool,

        /// The table a report covers (default semantic).
        #[arg(long, value_enum, requires = "report")]
        table: Option<OneTable>,

        /// Maximum cosine distance within a cluster, above 0 and at most 2.
        /// Default 0.22 semantic / 0.25 episodic; 0.15 for contradictions.
        #[arg(long, requires = "vectors", value_parser = sleep::cluster::parse_threshold)]
        threshold: Option<f64>,

        /// --verify-candidates: list at most this many (0 = all).
        #[arg(long, requires = "listing")]
        limit: Option<u32>,
    },

    /// Bulk-import a directory of files, one memory per file.
    ///
    /// Shares the write path and every refusal with `remember`.
    ///
    /// Each file becomes ONE memory of at most 2000 characters; topic = the file
    /// name without its last suffix, keywords = that name's words, source =
    /// `backfill`. A file that is too long, empty, not UTF-8 or a near-duplicate
    /// of a stored memory is reported and skipped; the rest of the run goes on.
    /// The glob matches file names at any depth, ignoring case. NEVER import
    /// files containing PII.
    Backfill {
        /// The directory to read, searched recursively.
        #[arg(long)]
        dir: PathBuf,

        /// File-name pattern (`*` and `?`), matched without regard to case.
        #[arg(long, default_value = "*.md")]
        glob: String,

        /// Where the memories go.
        #[arg(long, value_enum, default_value = "semantic")]
        table: OneTable,

        /// Semantic category. Not baseline: that needs the user's confirmation,
        /// so it goes through `remember --confirm-baseline`.
        #[arg(long, default_value = "reference")]
        category: String,

        /// Tracking-tool work item id; omit if the files are not ticket-bound.
        #[arg(long)]
        project: Option<String>,

        /// The agent model doing the import.
        #[arg(long, default_value = "unknown")]
        model: String,
    },

    /// Restore a dump into a fresh database file, verifying row counts.
    ///
    /// Reads tursodb's `.dump` and this binary's own dumps, .sql or .sql.gz.
    /// Every table the dump creates or writes is counted against the dump
    /// before the file takes its name; any mismatch, rejected statement or
    /// cut-off dump exits non-zero and leaves nothing at --out. Never over the
    /// configured database, not even with --force. A restore that is not
    /// counted is not a restore.
    Restore {
        /// The dump to restore. Default: the newest dump in BACKUP_DIR.
        #[arg(long, value_name = "FILE")]
        dump: Option<PathBuf>,

        /// The database file to create. It must not exist yet.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,

        /// Replace an existing file at --out, once the restore is verified.
        /// This discards everything that file holds.
        #[arg(long)]
        force: bool,
    },

    /// Write a verified, gzipped SQL dump to BACKUP_DIR; keep 3 daily, 4 weekly.
    ///
    /// stdout is the dump's absolute path and nothing else, printed once the
    /// dump is written, verified and in place; everything else goes to stderr,
    /// and a failure prints nothing on stdout. One read snapshot, so writers
    /// are never blocked. Needs no tursodb.
    Backup,

    /// Probe for file-based Claude memory to migrate (read-only, no DB, no Ollama).
    ///
    /// Scans CLAUDE_CONFIG_DIR (default ~/.claude): CLAUDE.md outside the managed
    /// block, memory/ and projects/*/memory/, recursively, `.md` files of any case.
    /// Prints JSON. Every skipped path lands in a reported field, and
    /// `nothing_to_do` is the one field to gate a migration offer on.
    FindExistingMemory {
        /// Print only `found` or `nothing` -- `nothing` exactly when the JSON
        /// would say `nothing_to_do: true`. Exits 0 either way.
        #[arg(long)]
        quiet: bool,
    },

    /// First-run setup: create the database, write the .env, install the instructions.
    ///
    /// A dry run unless -y: prints the plan, ending `Ask the user to confirm`,
    /// and changes nothing. Never writes an existing database, never overwrites
    /// a differing .env value, never rewrites CLAUDE.md text -- it appends the
    /// instructions only when their headline is absent. Ollama and tursodb are
    /// optional; each missing one is listed with what the user loses.
    Init {
        /// The database file: adopted if it is a memory database, created from
        /// the embedded schema if absent. Default: the configured path.
        #[arg(long = "db-path", value_name = "FILE")]
        db_path: Option<PathBuf>,

        /// Where `backup` writes its dumps. Default: the configured directory.
        #[arg(long = "backup-dir", value_name = "DIR")]
        backup_dir: Option<PathBuf>,

        /// How much the agent stores as episodic memory: every-prompt,
        /// major-actions, major-events (default) or manual.
        #[arg(long = "episodic-mode", value_name = "MODE")]
        episodic_mode: Option<String>,

        /// Apply the plan. Also the acknowledgement of every missing dependency
        /// the plan lists.
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },

    /// After the binary was replaced: the notes, schema migrations and CLAUDE.md changes since.
    ///
    /// A dry run unless -y: compares the version the .env records with this
    /// binary's, prints every migration note in between, the schema migrations
    /// the database lacks (asked of the database itself, never read from the
    /// .env) and what to change in CLAUDE.md, ending `Ask the user to confirm`.
    /// -y backs up the database before the first migration, applies each in a
    /// transaction of its own, and records the version only once all succeeded.
    /// Never writes CLAUDE.md. Refuses a downgrade.
    Update {
        /// Apply: back up, migrate, record this binary's version in the .env.
        #[arg(short = 'y', long = "yes")]
        yes: bool,

        /// Print the managed block this binary would write, and nothing else:
        /// what replaces the text between CLAUDE.md's markers.
        #[arg(long = "print-block", conflicts_with = "yes")]
        print_block: bool,

        /// The episodic mode to render with, when the block's own policy line
        /// cannot be read: every-prompt, major-actions, major-events or manual.
        /// Ask the user; never assume one.
        #[arg(long = "episodic-mode", value_name = "MODE")]
        episodic_mode: Option<String>,
    },

    /// Recall-quality harness: validate the eval set, score the ranking, sweep RECALL_ALPHA.
    ///
    /// Exactly one mode per run. eval_cases are AUTHORED and cannot be
    /// regenerated, so nothing here deletes one: --retire-case sets
    /// retired_at. --validate needs no embeddings and exits 1 while a case
    /// points at a purged, superseded or reused row; --report, --sweep and
    /// --variants need Ollama and an embedded corpus and refuse without them.
    /// --report appends one eval_runs row; --sweep only reports and never
    /// changes RECALL_ALPHA. --add-case computes the stamps from the targets:
    /// they are never typed.
    #[command(
        group(
            ArgGroup::new("mode")
                .required(true)
                .args([
                    "validate",
                    "report",
                    "sweep",
                    "variants",
                    "import",
                    "repoint",
                    "retire_case",
                    "add_case",
                    "last_run",
                ])
        ),
        // One-member groups, because clap counts a required ARGUMENT as
        // satisfied when it conflicts with one that is present.
        group(ArgGroup::new("adding").args(["add_case"])),
        group(ArgGroup::new("repointing").args(["repoint"]))
    )]
    Eval {
        /// Check every live case still points at current rows whose created_at
        /// matches its stamp. Exit 1 when one does not; 0 with nothing to check.
        #[arg(long)]
        validate: bool,

        /// Score the shipped ranking at the configured RECALL_ALPHA, record the
        /// run and diff it against the last one.
        #[arg(long)]
        report: bool,

        /// Score the shipped ranking at each of these alphas (comma-separated)
        /// and print the plateau. Never records, never changes the value.
        #[arg(long, value_name = "A,B,C", value_parser = eval::score::parse_alphas)]
        sweep: Option<eval::score::Alphas>,

        /// Developer comparison: the frozen legacy ranker, pure cosine,
        /// IDF-lexicographic and the shipped blend.
        #[arg(long)]
        variants: bool,

        /// Migrate a legacy eval.jsonl / history.jsonl directory into the
        /// database. Idempotent; refused whole when anything in it is invalid.
        #[arg(long, value_name = "DIR")]
        import: Option<PathBuf>,

        /// Point a case at a new target (a current row) and refresh its stamp.
        #[arg(long, num_args = 2, value_names = ["CASE", "NEW_ID"])]
        repoint: Option<Vec<String>>,

        /// --repoint: the target to replace, required when the case has more
        /// than one.
        #[arg(long, value_name = "OLD_ID", requires = "repointing")]
        from: Option<i64>,

        /// Retire a case: sets retired_at, never deletes.
        #[arg(long = "retire-case", value_name = "CASE")]
        retire_case: Option<String>,

        /// Author a case (with --id, --class, --table, --query, --expect). The
        /// stamps are read from the targets.
        #[arg(
            long = "add-case",
            requires_all = ["id", "class", "table", "query", "expect"]
        )]
        add_case: bool,

        /// --add-case: the case's label, unique forever.
        #[arg(long, requires = "adding")]
        id: Option<String>,

        /// --add-case: its class, at most 32 characters, one already in use
        /// unless no case exists.
        #[arg(long, requires = "adding")]
        class: Option<String>,

        /// --add-case: the table the targets are in.
        #[arg(long, value_enum, requires = "adding")]
        table: Option<OneTable>,

        /// --add-case: the query a real user would type, 1 to 512 characters.
        #[arg(long, requires = "adding")]
        query: Option<String>,

        /// --add-case: the target ids, comma-separated, all current.
        #[arg(long, value_name = "ID[,ID...]", requires = "adding")]
        expect: Option<String>,

        /// The newest eval_runs.ran_at, or `none`.
        #[arg(long = "last-run")]
        last_run: bool,
    },

    /// Print every setting, its value and where it came from: env, .env or default.
    ///
    /// Read-only. Precedence is real environment > the .env beside the binary >
    /// built-in default; a real variable that overrides a differing .env value
    /// says so.
    Config,

    /// Print a procedure the instructions point at, or list them.
    ///
    /// The runbooks are compiled in; nothing is read from disk. With no name,
    /// one line per runbook. `{{EXE}}` in a text is replaced by this binary's
    /// path, so a printed command line runs as it stands. Needs no database.
    Runbook {
        /// The runbook to print: setup, update, sleep, deep-sleep, migrate or
        /// new-coworker.
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
}

impl Command {
    /// The subcommand's name as typed. Kept next to the enum so a rename cannot
    /// silently diverge from what the runbooks call it.
    #[cfg(test)]
    pub fn name(&self) -> &'static str {
        match self {
            Command::Status => "status",
            Command::Recall { .. } => "recall",
            Command::Remember { .. } => "remember",
            Command::Instruct { .. } => "instruct",
            Command::Instructions { .. } => "instructions",
            Command::List { .. } => "list",
            Command::Show { .. } => "show",
            Command::Embed { .. } => "embed",
            Command::Coworkers { .. } => "coworkers",
            Command::Sleep { .. } => "sleep",
            Command::Backfill { .. } => "backfill",
            Command::Restore { .. } => "restore",
            Command::Backup => "backup",
            Command::FindExistingMemory { .. } => "find-existing-memory",
            Command::Init { .. } => "init",
            Command::Update { .. } => "update",
            Command::Eval { .. } => "eval",
            Command::Config => "config",
            Command::Runbook { .. } => "runbook",
        }
    }

    fn run(&self, cfg: &Config) -> Result<()> {
        match self {
            Command::Status => status::run(cfg),
            Command::Config => config::run(cfg),
            Command::Runbook { name } => runbook::run(cfg, name.as_deref()),
            Command::Recall {
                query,
                table,
                project,
                k,
                coworker,
                baseline,
                topics,
                candidates,
                count,
            } => recall::run(
                cfg,
                &recall::Args {
                    query: query.clone(),
                    table: (*table).into(),
                    project: project.clone(),
                    k: *k,
                    coworker: coworker.clone(),
                    baseline: *baseline,
                    topics: topics.clone(),
                    candidates: *candidates,
                    count: *count,
                },
            ),
            Command::Remember {
                table,
                text,
                topic,
                project,
                keywords,
                source,
                model,
                file_reference,
                created_at,
                category,
                event_type,
                importance,
                confirm_baseline,
                force,
                supersedes,
                coworker,
            } => remember::run(
                cfg,
                &remember::Args {
                    table: Some((*table).into()),
                    text: text.clone(),
                    topic: topic.clone(),
                    project: project.clone(),
                    keywords: keywords.clone(),
                    source: source.clone(),
                    model: model.clone(),
                    file_reference: file_reference.clone(),
                    created_at: created_at.clone(),
                    category: category.clone(),
                    event_type: event_type.clone(),
                    importance: importance.clone(),
                    confirm_baseline: *confirm_baseline,
                    force: *force,
                    supersedes: supersedes.clone(),
                    coworker: coworker.clone(),
                },
            ),
            Command::Backfill {
                dir,
                glob,
                table,
                category,
                project,
                model,
            } => backfill::run(
                cfg,
                &backfill::Args {
                    dir: dir.clone(),
                    glob: glob.clone(),
                    table: (*table).into(),
                    category: category.clone(),
                    project: project.clone(),
                    model: model.clone(),
                },
            ),
            Command::List {
                table,
                ids,
                current,
                not_current,
                unprocessed,
                topic,
                category,
                source,
                project,
                since,
                preview,
                keywords,
            } => rows::run_list(
                cfg,
                &rows::ListArgs {
                    table: Some((*table).into()),
                    ids: ids.clone(),
                    current: *current,
                    not_current: *not_current,
                    unprocessed: *unprocessed,
                    topic: topic.clone(),
                    category: category.clone(),
                    source: source.clone(),
                    project: project.clone(),
                    since: since.clone(),
                    preview: preview.map(|n| n as usize),
                    keywords: *keywords,
                },
            ),
            Command::Show { table, ids } => rows::run_show(cfg, (*table).into(), ids),
            Command::Instruct { .. } => instructions::write::run(cfg, &self.instruct_mode()),
            Command::Instructions { .. } => instructions::run(cfg, &self.lookup()),
            Command::Embed {
                missing: _,
                table,
                limit,
                batch,
            } => embed::missing::run(
                cfg,
                &embed::missing::Args {
                    tables: table.tables(),
                    limit: *limit,
                    batch: *batch,
                },
            ),
            Command::Sleep { .. } => sleep::run(cfg, &self.sleep_mode()),
            Command::Coworkers { .. } => coworkers::run(cfg, &self.coworkers_mode()),
            Command::Eval { .. } => eval::run(cfg, &self.eval_mode()),
            Command::Backup => backup::run(cfg),
            Command::Restore { dump, out, force } => restore::run(
                cfg,
                &restore::Args {
                    dump: dump.clone(),
                    out: out.clone(),
                    force: *force,
                },
            ),
            Command::FindExistingMemory { quiet } => find_existing::run(cfg, *quiet),
            Command::Init {
                db_path,
                backup_dir,
                episodic_mode,
                yes,
            } => init::run(
                cfg,
                &init::Args {
                    db_path: db_path.clone(),
                    backup_dir: backup_dir.clone(),
                    episodic_mode: episodic_mode.clone(),
                    yes: *yes,
                },
            ),
            Command::Update {
                yes,
                print_block,
                episodic_mode,
            } => update::run(
                cfg,
                &update::Args {
                    yes: *yes,
                    print_block: *print_block,
                    episodic_mode: episodic_mode.clone(),
                },
            ),
        }
    }
}

impl Command {
    /// What an `instruct` invocation asks for: a store, or a retirement.
    fn instruct_mode(&self) -> instructions::write::Mode {
        let Command::Instruct {
            topic,
            text,
            keywords,
            source,
            model,
            project,
            file_reference,
            force,
            replaces,
            retire,
        } = self
        else {
            unreachable!("instruct_mode is only called for instruct")
        };
        match retire {
            Some(id) => instructions::write::Mode::Retire(*id),
            None => instructions::write::Mode::Store(instructions::write::Args {
                topic: topic.clone(),
                text: text.clone().unwrap_or_default(),
                keywords: keywords.clone(),
                source: source.clone(),
                model: model.clone(),
                project: project.clone(),
                file_reference: file_reference.clone(),
                force: *force,
                replaces: *replaces,
            }),
        }
    }

    /// The one lookup an `instructions` invocation names. clap's `lookup`
    /// group admits exactly one.
    fn lookup(&self) -> instructions::Lookup {
        let Command::Instructions {
            query,
            get,
            list: _,
            k,
            project,
        } = self
        else {
            unreachable!("lookup is only called for instructions")
        };
        if let Some(query) = query {
            instructions::Lookup::Search {
                query: query.clone(),
                k: k.unwrap_or(instructions::DEFAULT_K),
                project: project.clone(),
            }
        } else if let Some(key) = get {
            instructions::Lookup::Get(key.clone())
        } else {
            instructions::Lookup::List
        }
    }

    /// The one mode a `coworkers` invocation names. clap's `mode` group
    /// admits exactly one, so the order here decides nothing.
    fn coworkers_mode(&self) -> coworkers::Mode {
        let Command::Coworkers {
            add,
            name,
            expertise,
            personality,
            appraise,
            trust,
            text,
            set_trust,
            retire,
            reactivate,
            list: _,
            load,
        } = self
        else {
            unreachable!("coworkers_mode is only called for coworkers")
        };
        let given = |v: &Option<String>| v.clone().unwrap_or_default();
        if *add {
            coworkers::Mode::Add {
                name: given(name),
                expertise: given(expertise),
                personality: given(personality),
            }
        } else if let Some(name) = appraise {
            coworkers::Mode::Appraise {
                name: name.clone(),
                trust: given(trust),
                text: given(text),
            }
        } else if let Some([name, level]) = set_trust.as_deref() {
            coworkers::Mode::SetTrust {
                name: name.clone(),
                level: level.clone(),
            }
        } else if let Some(name) = retire {
            coworkers::Mode::Retire(name.clone())
        } else if let Some(name) = reactivate {
            coworkers::Mode::Reactivate(name.clone())
        } else if let Some(name) = load {
            coworkers::Mode::Load(name.clone())
        } else {
            coworkers::Mode::List
        }
    }

    /// The one mode an `eval` invocation names. clap's `mode` group admits
    /// exactly one, so the order here decides nothing.
    fn eval_mode(&self) -> eval::Mode {
        let Command::Eval {
            validate: _,
            report,
            sweep,
            variants,
            import,
            repoint,
            from,
            retire_case,
            add_case,
            id,
            class,
            table,
            query,
            expect,
            last_run,
        } = self
        else {
            unreachable!("eval_mode is only called for eval")
        };
        let given = |v: &Option<String>| v.clone().unwrap_or_default();
        if *report {
            eval::Mode::Report
        } else if let Some(alphas) = sweep {
            eval::Mode::Sweep(alphas.0.clone())
        } else if *variants {
            eval::Mode::Variants
        } else if let Some(dir) = import {
            eval::Mode::Import(dir.clone())
        } else if let Some([case, to]) = repoint.as_deref() {
            eval::Mode::Repoint {
                case: case.clone(),
                to: to.clone(),
                from: *from,
            }
        } else if let Some(case) = retire_case {
            eval::Mode::RetireCase(case.clone())
        } else if *add_case {
            eval::Mode::AddCase {
                id: given(id),
                class: given(class),
                table: table.map_or(store::Table::Semantic, Into::into),
                query: given(query),
                expect: given(expect),
            }
        } else if *last_run {
            eval::Mode::LastRun
        } else {
            eval::Mode::Validate
        }
    }

    /// The one mode a `sleep` invocation names. clap's `mode` group admits
    /// exactly one, so the order here decides nothing.
    fn sleep_mode(&self) -> sleep::Mode {
        let Command::Sleep {
            mark_processed,
            retire,
            rebuild_topics: _,
            purge,
            confirm_purge,
            cluster,
            verify_candidates,
            contradiction_candidates,
            staleness,
            table,
            threshold,
            limit,
        } = self
        else {
            unreachable!("sleep_mode is only called for sleep")
        };
        let table = table.map_or(store::Table::Semantic, Into::into);
        let threshold = *threshold;
        if let Some(ids) = mark_processed {
            sleep::Mode::MarkProcessed(ids.clone())
        } else if let Some(id) = retire {
            sleep::Mode::Retire(*id)
        } else if let Some(ids) = purge {
            sleep::Mode::Purge {
                ids: ids.clone(),
                confirm: *confirm_purge,
            }
        } else if *cluster {
            sleep::Mode::Cluster { table, threshold }
        } else if *contradiction_candidates {
            sleep::Mode::ContradictionCandidates { table, threshold }
        } else if *verify_candidates {
            sleep::Mode::VerifyCandidates {
                table,
                limit: limit.unwrap_or(0),
            }
        } else if *staleness {
            sleep::Mode::Staleness { table }
        } else {
            sleep::Mode::RebuildTopics
        }
    }
}

/// Resolve configuration, then run the command with it.
///
/// Every command goes through here, stubs included, so a `.env` that cannot be
/// trusted stops the run before anything happens -- running on the built-in
/// defaults instead is how a skipped database-path line would silently open the
/// wrong database.
fn execute(command: &Command, env: std::result::Result<Env, ConfigError>) -> Result<()> {
    let cfg = Config::from_env(&env?)?;
    for warning in &cfg.warnings {
        eprintln!("warning: {warning}");
    }
    command.run(&cfg)
}

/// Parse and dispatch one invocation, returning the process exit code.
pub fn main<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => {
            // clap already rendered the message and the usage text. --help and
            // --version are successful requests, not failures.
            let _ = err.print();
            return ExitCode::from(if err.use_stderr() {
                EXIT_USAGE
            } else {
                EXIT_OK
            });
        }
    };

    match execute(&cli.command, Env::system()) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(err.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;
    use crate::error::EXIT_REFUSED;

    #[test]
    fn clap_definition_is_wellformed() {
        Cli::command().debug_assert();
    }

    /// Every subcommand the rewrite promises must exist, so a runbook rewritten
    /// against this surface cannot name a command that was never wired up.
    #[test]
    fn every_promised_subcommand_is_registered() {
        let registered: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for want in [
            "status",
            "recall",
            "remember",
            "instruct",
            "instructions",
            "embed",
            "coworkers",
            "sleep",
            "backfill",
            "restore",
            "backup",
            "find-existing-memory",
            "init",
            "update",
            "eval",
            "config",
            "runbook",
        ] {
            assert!(
                registered.contains(&want.to_string()),
                "{want} is not registered"
            );
        }
        // `init` replaced the installer script's stand-in; nothing is left to
        // call by the old name.
        assert!(!registered.contains(&"install".to_string()));
        // `init` creates the database and `remember` / `backfill` load it, so
        // there is no separate seeding step to name.
        assert!(!registered.contains(&"seed".to_string()));
    }

    /// Minimal valid arguments per subcommand, so each one gets past clap.
    fn invocation(name: &str) -> Vec<&str> {
        let mut args = vec!["sm", name];
        match name {
            "recall" => args.push("anything"),
            "remember" => args.extend(["--table", "semantic", "--text", "a memory"]),
            "backfill" => args.extend(["--dir", "/nonexistent/notes"]),
            "instruct" => args.extend(["--topic", "deploy", "--text", "run the checklist"]),
            "instructions" => args.push("--list"),
            "list" => args.extend(["--table", "semantic"]),
            "show" => args.extend(["--table", "semantic", "1"]),
            "embed" => args.push("--missing"),
            "restore" => args.extend(["--out", "/nonexistent/restored.db"]),
            "sleep" => args.push("--staleness"),
            "coworkers" => args.push("--list"),
            "eval" => args.push("--validate"),
            _ => {}
        }
        args
    }

    /// A malformed `.env` must stop EVERY command, stubs included, before it does
    /// anything -- running on defaults is the silent wrong-database failure.
    #[test]
    fn a_malformed_dotenv_makes_every_command_refuse() {
        let dir = crate::config::dotenv::tests::tmpdir("cli-malformed");
        let exe = dir.join("supercharged-memory");
        std::fs::write(&exe, b"").unwrap();
        std::fs::write(
            dir.join(".env"),
            "SUPERCHARGED_MEMORY_TURSO_PATH=/x.db\nexport A=1\n",
        )
        .unwrap();

        let names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .filter(|n| n != "help")
            .collect();
        assert!(names.contains(&"config".to_string()));
        for name in &names {
            let cli =
                Cli::try_parse_from(invocation(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            let env = crate::config::DotEnv::beside(&exe).map(|dotenv| Env {
                vars: Default::default(),
                home: dir.clone(),
                windows: false,
                dotenv,
            });
            let err = execute(&cli.command, env).expect_err(name);
            assert_eq!(err.exit_code(), EXIT_REFUSED, "{name}: {err}");
            let msg = err.to_string();
            assert!(msg.starts_with("refused: "), "{name}: {msg}");
            assert!(msg.contains("line 2"), "{name}: {msg}");
        }
    }

    #[test]
    fn config_reads_only() {
        let cfg = Config::from_env(&Env {
            vars: Default::default(),
            home: std::path::PathBuf::from("/home/tester"),
            windows: false,
            dotenv: crate::config::DotEnv::absent(crate::config::tests::EXE.into()),
        })
        .unwrap();
        assert!(Command::Config.run(&cfg).is_ok());
        assert_eq!(Command::Config.name(), "config");
    }

    #[test]
    fn every_subcommand_reports_a_name() {
        for cmd in [
            Command::Status,
            Command::Recall {
                query: None,
                table: TableArg::Both,
                project: None,
                k: 5,
                coworker: None,
                baseline: false,
                topics: None,
                candidates: false,
                count: false,
            },
            Command::Remember {
                table: OneTable::Semantic,
                text: String::new(),
                topic: None,
                project: None,
                keywords: None,
                source: None,
                model: None,
                file_reference: None,
                created_at: None,
                category: None,
                event_type: None,
                importance: None,
                confirm_baseline: false,
                force: false,
                supersedes: None,
                coworker: None,
            },
            Command::Instruct {
                topic: None,
                text: None,
                keywords: None,
                source: None,
                model: None,
                project: None,
                file_reference: None,
                force: false,
                replaces: None,
                retire: None,
            },
            Command::Instructions {
                query: None,
                get: None,
                list: true,
                k: None,
                project: None,
            },
            Command::List {
                table: ReadTable::Semantic,
                ids: None,
                current: false,
                not_current: false,
                unprocessed: false,
                topic: None,
                category: None,
                source: None,
                project: None,
                since: None,
                preview: None,
                keywords: false,
            },
            Command::Show {
                table: ReadTable::Semantic,
                ids: "1".into(),
            },
            Command::Embed {
                missing: true,
                table: EmbedTable::All,
                limit: None,
                batch: 1,
            },
            Command::Coworkers {
                add: false,
                name: None,
                expertise: None,
                personality: None,
                appraise: None,
                trust: None,
                text: None,
                set_trust: None,
                retire: None,
                reactivate: None,
                list: true,
                load: None,
            },
            Command::Sleep {
                mark_processed: None,
                retire: None,
                rebuild_topics: false,
                purge: None,
                confirm_purge: false,
                cluster: false,
                verify_candidates: false,
                contradiction_candidates: false,
                staleness: true,
                table: None,
                threshold: None,
                limit: None,
            },
            Command::Backfill {
                dir: PathBuf::from("/nonexistent/notes"),
                glob: "*.md".into(),
                table: OneTable::Semantic,
                category: "reference".into(),
                project: None,
                model: "unknown".into(),
            },
            Command::Restore {
                dump: None,
                out: PathBuf::from("/nonexistent/restored.db"),
                force: false,
            },
            Command::Backup,
            Command::FindExistingMemory { quiet: false },
            Command::Init {
                db_path: None,
                backup_dir: None,
                episodic_mode: None,
                yes: false,
            },
            Command::Update {
                yes: false,
                print_block: false,
                episodic_mode: None,
            },
            Command::Eval {
                validate: true,
                report: false,
                sweep: None,
                variants: false,
                import: None,
                repoint: None,
                from: None,
                retire_case: None,
                add_case: false,
                id: None,
                class: None,
                table: None,
                query: None,
                expect: None,
                last_run: false,
            },
            Command::Config,
            Command::Runbook { name: None },
        ] {
            assert!(!cmd.name().is_empty());
        }
    }

    #[test]
    fn backfill_takes_a_directory_and_defaults_the_rest() {
        let cli = Cli::try_parse_from(["sm", "backfill", "--dir", "/notes"]).unwrap();
        match cli.command {
            Command::Backfill {
                dir,
                glob,
                table,
                category,
                project,
                model,
            } => {
                assert_eq!(dir, PathBuf::from("/notes"));
                assert_eq!(glob, "*.md");
                assert_eq!(table, OneTable::Semantic);
                assert_eq!(category, "reference");
                assert_eq!(project, None);
                assert_eq!(model, "unknown");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn backfill_usage_errors_exit_64() {
        for args in [
            vec!["sm", "backfill"],
            vec!["sm", "backfill", "--dir", "/n", "--table", "instructions"],
            vec!["sm", "backfill", "--dir", "/n", "--force"],
        ] {
            assert_eq!(code(&args), EXIT_USAGE, "{args:?}");
        }
    }

    #[test]
    fn backfill_of_a_missing_directory_is_a_refusal() {
        assert_eq!(
            code(&["sm", "backfill", "--dir", "/nonexistent/notes"]),
            crate::error::EXIT_REFUSED
        );
    }

    fn sleep_mode(args: &[&str]) -> sleep::Mode {
        let mut full = vec!["sm", "sleep"];
        full.extend(args);
        Cli::try_parse_from(full)
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
            .command
            .sleep_mode()
    }

    #[test]
    fn each_sleep_flag_names_its_mode() {
        use crate::store::Table::{Episodic, Semantic};
        use sleep::Mode;
        for (args, want) in [
            (
                vec!["--mark-processed", "1,2"],
                Mode::MarkProcessed("1,2".into()),
            ),
            (vec!["--retire", "5"], Mode::Retire(5)),
            (vec!["--rebuild-topics"], Mode::RebuildTopics),
            (
                vec!["--purge", "2,3", "--confirm-purge"],
                Mode::Purge {
                    ids: "2,3".into(),
                    confirm: true,
                },
            ),
            (
                vec!["--purge", "2"],
                Mode::Purge {
                    ids: "2".into(),
                    confirm: false,
                },
            ),
            (
                vec!["--cluster"],
                Mode::Cluster {
                    table: Semantic,
                    threshold: None,
                },
            ),
            (
                vec!["--cluster", "--table", "episodic", "--threshold", "0.3"],
                Mode::Cluster {
                    table: Episodic,
                    threshold: Some(0.3),
                },
            ),
            (
                vec!["--contradiction-candidates", "--threshold", "0.12"],
                Mode::ContradictionCandidates {
                    table: Semantic,
                    threshold: Some(0.12),
                },
            ),
            (
                vec!["--staleness", "--table", "episodic"],
                Mode::Staleness { table: Episodic },
            ),
            (
                vec!["--verify-candidates", "--limit", "40"],
                Mode::VerifyCandidates {
                    table: Semantic,
                    limit: 40,
                },
            ),
            (
                vec!["--verify-candidates", "--table", "episodic"],
                Mode::VerifyCandidates {
                    table: Episodic,
                    limit: 0,
                },
            ),
        ] {
            assert_eq!(sleep_mode(&args), want, "{args:?}");
        }
    }

    /// Every malformed `sleep` is a usage error. Parsed only, never run: a
    /// case that got past clap must not reach the real environment's database.
    #[test]
    fn malformed_sleep_invocations_are_usage_errors() {
        for (args, why) in [
            (vec![], "no mode"),
            (vec!["--retire", "1", "--cluster"], "two modes"),
            (vec!["--staleness", "--verify-candidates"], "two reports"),
            (vec!["--confirm-purge"], "an acknowledgement is no mode"),
            (
                vec!["--retire", "1", "--confirm-purge"],
                "--confirm-purge belongs to --purge",
            ),
            (
                vec!["--purge", "5", "--confirm-purge", "--table", "episodic"],
                "a purge only ever means semantic rows",
            ),
            (
                vec!["--mark-processed", "1", "--table", "semantic"],
                "no table",
            ),
            (vec!["--staleness", "--threshold", "0.2"], "no vectors"),
            (vec!["--cluster", "--limit", "3"], "no limit"),
            (
                vec!["--cluster", "--threshold", "0"],
                "nothing is closer than 0",
            ),
            (vec!["--cluster", "--threshold", "nan"], "not a distance"),
            (
                vec!["--cluster", "--threshold", "3"],
                "beyond cosine's range",
            ),
            (vec!["--retire", "x"], "not an id"),
            (
                vec!["--cluster", "--table", "instructions"],
                "not a memory table",
            ),
            (vec!["--staleness", "--frobnicate"], "unknown flag"),
        ] {
            let mut full = vec!["sm", "sleep"];
            full.extend(args);
            match Cli::try_parse_from(&full) {
                Err(e) => assert!(e.use_stderr(), "{why}: {full:?}"),
                Ok(cli) => panic!("{why}: {full:?} parsed as {:?}", cli.command),
            }
        }
    }

    fn eval_mode(args: &[&str]) -> eval::Mode {
        let mut full = vec!["sm", "eval"];
        full.extend(args);
        parsed(&full).eval_mode()
    }

    #[test]
    fn each_eval_flag_names_its_mode() {
        use eval::Mode;
        assert_eq!(eval_mode(&["--validate"]), Mode::Validate);
        assert_eq!(eval_mode(&["--report"]), Mode::Report);
        assert_eq!(eval_mode(&["--variants"]), Mode::Variants);
        assert_eq!(eval_mode(&["--last-run"]), Mode::LastRun);
        assert_eq!(
            eval_mode(&["--sweep", "0.05,0.1,0.5"]),
            Mode::Sweep(vec![0.05, 0.1, 0.5])
        );
        assert_eq!(
            eval_mode(&["--import", "/old/eval"]),
            Mode::Import(PathBuf::from("/old/eval"))
        );
        assert_eq!(
            eval_mode(&["--retire-case", "s01"]),
            Mode::RetireCase("s01".into())
        );
        assert_eq!(
            eval_mode(&["--repoint", "s01", "42"]),
            Mode::Repoint {
                case: "s01".into(),
                to: "42".into(),
                from: None
            }
        );
        assert_eq!(
            eval_mode(&["--repoint", "s01", "42", "--from", "7"]),
            Mode::Repoint {
                case: "s01".into(),
                to: "42".into(),
                from: Some(7)
            }
        );
        assert_eq!(
            eval_mode(&[
                "--add-case",
                "--id",
                "s09",
                "--class",
                "semantic",
                "--table",
                "episodic",
                "--query",
                "how do I x",
                "--expect",
                "288,289",
            ]),
            Mode::AddCase {
                id: "s09".into(),
                class: "semantic".into(),
                table: store::Table::Episodic,
                query: "how do I x".into(),
                expect: "288,289".into(),
            }
        );
    }

    /// Every malformed `eval` is a usage error. Parsed only, never run: a case
    /// that got past clap must not reach the real environment's database.
    #[test]
    fn malformed_eval_invocations_are_usage_errors() {
        let add = [
            "--add-case",
            "--id",
            "s09",
            "--class",
            "semantic",
            "--table",
            "semantic",
            "--query",
            "q",
            "--expect",
            "1",
        ];
        let without = |flag: &str| -> Vec<&str> {
            let mut out = Vec::new();
            let mut skip = false;
            for a in add {
                if skip {
                    skip = false;
                } else if a == flag {
                    skip = true;
                } else {
                    out.push(a);
                }
            }
            out
        };
        let mut cases: Vec<(Vec<&str>, &str)> = vec![
            (vec![], "no mode"),
            (vec!["--validate", "--report"], "two modes"),
            (vec!["--report", "--last-run"], "two modes"),
            (vec!["--sweep"], "no alphas"),
            (vec!["--sweep", "a,b"], "not numbers"),
            (vec!["--sweep", "0.1,,0.2"], "an empty alpha"),
            (vec!["--sweep", "-1"], "a negative alpha"),
            (vec!["--repoint", "s01"], "no new id"),
            (vec!["--repoint", "s01", "1", "2"], "three values"),
            (vec!["--from", "7"], "--from belongs to --repoint"),
            (
                vec!["--retire-case", "s01", "--from", "7"],
                "--from belongs to --repoint",
            ),
            (vec!["--repoint", "s01", "1", "--from", "x"], "not an id"),
            (vec!["--id", "s01"], "a field is no mode"),
            (
                vec!["--validate", "--id", "s01"],
                "--id belongs to --add-case",
            ),
            (
                vec!["--report", "--query", "q"],
                "--query belongs to --add-case",
            ),
            (vec!["--validate", "--table", "semantic"], "no table"),
            (vec!["--validate", "--expect", "1"], "no targets"),
            (vec!["--validate", "--class", "x"], "no class"),
            (vec!["--validate", "--frobnicate"], "unknown flag"),
        ];
        for flag in ["--id", "--class", "--table", "--query", "--expect"] {
            cases.push((without(flag), "--add-case needs every field"));
        }
        let mut instructions_table = add.to_vec();
        instructions_table[6] = "instructions";
        cases.push((instructions_table, "instructions are not measured"));
        for (args, why) in cases {
            let mut full = vec!["sm", "eval"];
            full.extend(args);
            match Cli::try_parse_from(&full) {
                Err(e) => assert!(e.use_stderr(), "{why}: {full:?}"),
                Ok(cli) => panic!("{why}: {full:?} parsed as {:?}", cli.command),
            }
        }
    }

    fn parsed(args: &[&str]) -> Command {
        Cli::try_parse_from(args)
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
            .command
    }

    #[test]
    fn instruct_and_instructions_parse_into_what_they_ask_for() {
        use instructions::write::{Args, Mode};
        let store = |extra: &[&str]| {
            let mut args = vec!["sm", "instruct", "--topic", "deploy", "--text", "run it"];
            args.extend(extra);
            parsed(&args).instruct_mode()
        };
        let plain = Args {
            topic: Some("deploy".into()),
            text: "run it".into(),
            ..Args::default()
        };
        assert_eq!(store(&[]), Mode::Store(plain.clone()));
        assert_eq!(
            store(&[
                "--keywords",
                "k",
                "--source",
                "s",
                "--model",
                "m",
                "--project",
                "p",
                "--file-reference",
                "f",
                "--force",
                "--replaces",
                "4",
            ]),
            Mode::Store(Args {
                keywords: Some("k".into()),
                source: Some("s".into()),
                model: Some("m".into()),
                project: Some("p".into()),
                file_reference: Some("f".into()),
                force: true,
                replaces: Some(4),
                ..plain
            })
        );
        assert_eq!(
            parsed(&["sm", "instruct", "--retire", "3"]).instruct_mode(),
            Mode::Retire(3)
        );

        use instructions::Lookup;
        let lookup = |args: &[&str]| {
            let mut full = vec!["sm", "instructions"];
            full.extend(args);
            parsed(&full).lookup()
        };
        assert_eq!(
            lookup(&["release notes"]),
            Lookup::Search {
                query: "release notes".into(),
                k: instructions::DEFAULT_K,
                project: None
            }
        );
        assert_eq!(
            lookup(&["release notes", "--k", "3", "--project", "p1"]),
            Lookup::Search {
                query: "release notes".into(),
                k: 3,
                project: Some("p1".into())
            }
        );
        assert_eq!(lookup(&["--get", "deploy"]), Lookup::Get("deploy".into()));
        assert_eq!(lookup(&["--list"]), Lookup::List);
    }

    /// Every malformed `instruct` or `instructions` is a usage error. Parsed
    /// only, never run: a case that got past clap must not reach the real
    /// environment's database.
    #[test]
    fn malformed_instruct_and_instructions_invocations_are_usage_errors() {
        for (args, why) in [
            (vec!["instruct"], "nothing to do"),
            (vec!["instruct", "--topic", "t"], "no text"),
            (vec!["instruct", "--text", "x"], "no topic"),
            (
                vec!["instruct", "--retire", "3", "--topic", "t"],
                "a retirement stores nothing",
            ),
            (
                vec!["instruct", "--retire", "3", "--text", "x"],
                "a retirement stores nothing",
            ),
            (
                vec!["instruct", "--retire", "3", "--replaces", "4"],
                "one or the other",
            ),
            (
                vec!["instruct", "--retire", "3", "--force"],
                "nothing to force",
            ),
            (
                vec!["instruct", "--retire", "3", "--keywords", "k"],
                "nothing to tag",
            ),
            (vec!["instruct", "--retire", "x"], "not an id"),
            (
                vec!["instruct", "--topic", "t", "--text", "x", "--replaces", "x"],
                "not an id",
            ),
            (
                vec![
                    "instruct", "--topic", "t", "--text", "x", "--table", "semantic",
                ],
                "unknown flag",
            ),
            (vec!["instructions"], "no lookup"),
            (vec!["instructions", "q", "--list"], "two lookups"),
            (vec!["instructions", "--get", "x", "--list"], "two lookups"),
            (vec!["instructions", "q", "--get", "x"], "two lookups"),
            (
                vec!["instructions", "--list", "--k", "3"],
                "--k belongs to a search",
            ),
            (
                vec!["instructions", "--get", "x", "--project", "p"],
                "--project belongs to a search",
            ),
            (
                vec!["instructions", "q", "--k", "0"],
                "zero hits asks for nothing",
            ),
            (
                vec!["instructions", "--list", "--frobnicate"],
                "unknown flag",
            ),
        ] {
            let mut full = vec!["sm"];
            full.extend(args);
            match Cli::try_parse_from(&full) {
                Err(e) => assert!(e.use_stderr(), "{why}: {full:?}"),
                Ok(cli) => panic!("{why}: {full:?} parsed as {:?}", cli.command),
            }
        }
    }

    #[test]
    fn each_coworkers_flag_names_its_mode() {
        use coworkers::Mode;
        let mode = |args: &[&str]| {
            let mut full = vec!["sm", "coworkers"];
            full.extend(args);
            parsed(&full).coworkers_mode()
        };
        for (args, want) in [
            (
                vec![
                    "--add",
                    "--name",
                    "jeff",
                    "--expertise",
                    "rust",
                    "--personality",
                    "terse",
                ],
                Mode::Add {
                    name: "jeff".into(),
                    expertise: "rust".into(),
                    personality: "terse".into(),
                },
            ),
            (
                vec!["--appraise", "jeff", "--trust", "trusted", "--text", "good"],
                Mode::Appraise {
                    name: "jeff".into(),
                    trust: "trusted".into(),
                    text: "good".into(),
                },
            ),
            (
                vec!["--set-trust", "jeff", "autonomous"],
                Mode::SetTrust {
                    name: "jeff".into(),
                    level: "autonomous".into(),
                },
            ),
            (vec!["--retire", "jeff"], Mode::Retire("jeff".into())),
            (
                vec!["--reactivate", "jeff"],
                Mode::Reactivate("jeff".into()),
            ),
            (vec!["--list"], Mode::List),
            (vec!["--load", "jeff"], Mode::Load("jeff".into())),
        ] {
            assert_eq!(mode(&args), want, "{args:?}");
        }
        // A bad trust level gets past clap on purpose: it is a refusal, with
        // the `refused:` prefix, not a usage error.
        assert_eq!(
            mode(&["--set-trust", "jeff", "bogus"]),
            Mode::SetTrust {
                name: "jeff".into(),
                level: "bogus".into(),
            }
        );
    }

    /// Every malformed `coworkers` is a usage error. Parsed only, never run: a
    /// case that got past clap must not reach the real environment's database.
    #[test]
    fn malformed_coworkers_invocations_are_usage_errors() {
        for (args, why) in [
            (vec![], "no mode"),
            (vec!["--list", "--load", "jeff"], "two modes"),
            (
                vec!["--retire", "jeff", "--reactivate", "jeff"],
                "two modes",
            ),
            (
                vec!["--add", "--name", "jeff", "--expertise", "rust"],
                "--add needs --personality",
            ),
            (vec!["--add"], "--add needs its three fields"),
            (vec!["--name", "jeff"], "--name belongs to --add"),
            (
                vec!["--list", "--expertise", "rust"],
                "--expertise belongs to --add",
            ),
            (
                vec!["--retire", "jeff", "--name", "ada"],
                "--name belongs to --add",
            ),
            (
                vec!["--appraise", "jeff"],
                "--appraise needs --trust and --text",
            ),
            (
                vec!["--appraise", "jeff", "--trust", "trusted"],
                "--appraise needs --text",
            ),
            (
                vec!["--load", "jeff", "--trust", "trusted"],
                "--trust belongs to --appraise",
            ),
            (
                vec!["--list", "--text", "x"],
                "--text belongs to --appraise",
            ),
            (
                vec![
                    "--add",
                    "--name",
                    "jeff",
                    "--expertise",
                    "rust",
                    "--personality",
                    "terse",
                    "--text",
                    "x",
                ],
                "--text belongs to --appraise",
            ),
            (
                vec!["--set-trust", "jeff"],
                "--set-trust takes a name and a level",
            ),
            (vec!["--load"], "--load names a coworker"),
            (vec!["--list", "--frobnicate"], "unknown flag"),
        ] {
            let mut full = vec!["sm", "coworkers"];
            full.extend(args);
            match Cli::try_parse_from(&full) {
                Err(e) => assert!(e.use_stderr(), "{why}: {full:?}"),
                Ok(cli) => panic!("{why}: {full:?} parsed as {:?}", cli.command),
            }
        }
    }

    /// `embed --table all` covers every table that stores an embedding.
    #[test]
    fn embed_all_covers_every_table_with_an_embedding() {
        assert_eq!(EmbedTable::All.tables(), store::Table::EMBEDDED.to_vec());
        assert_eq!(
            EmbedTable::Instructions.tables(),
            vec![store::Table::Instructions]
        );
        let Command::Embed { table, .. } =
            parsed(&["sm", "embed", "--missing", "--table", "instructions"])
        else {
            panic!("not embed")
        };
        assert_eq!(table, EmbedTable::Instructions);
    }

    /// `recall --topics` is the whole index bare and a filter with a word; `list`
    /// and `show` name their table. Parsed only, never run.
    #[test]
    fn the_read_commands_parse_their_arguments() {
        let parse = |args: &[&str]| {
            let mut full = vec!["sm"];
            full.extend(args);
            Cli::try_parse_from(full)
        };
        let topics = |args: &[&str]| match parse(args).unwrap().command {
            Command::Recall { topics, query, .. } => (topics, query),
            other => panic!("{other:?}"),
        };
        assert_eq!(topics(&["recall", "--topics"]), (Some(String::new()), None));
        assert_eq!(
            topics(&["recall", "--topics", "deploy"]),
            (Some("deploy".into()), None)
        );
        assert_eq!(
            topics(&["recall", "--topics=x y"]),
            (Some("x y".into()), None)
        );
        assert_eq!(
            topics(&["recall", "a query"]),
            (None, Some("a query".into()))
        );

        let Command::List {
            table,
            current,
            preview,
            keywords,
            since,
            ids,
            ..
        } = parse(&[
            "list",
            "--table",
            "semantic",
            "--current",
            "--preview",
            "80",
            "--keywords",
            "--since",
            "2026-01-01",
            "--ids",
            "1,2",
        ])
        .unwrap()
        .command
        else {
            panic!("not a list")
        };
        assert_eq!(table, ReadTable::Semantic);
        assert!(current && keywords);
        assert_eq!(preview, Some(80));
        assert_eq!(since.as_deref(), Some("2026-01-01"));
        assert_eq!(ids.as_deref(), Some("1,2"));

        let Command::Show { table, ids } = parse(&["show", "--table", "instructions", "3,1"])
            .unwrap()
            .command
        else {
            panic!("not a show")
        };
        assert_eq!(table, ReadTable::Instructions);
        assert_eq!(ids, "3,1");
    }

    /// Malformed `list` and `show` invocations are usage errors: parsed only,
    /// because a case that got past clap would read the real environment.
    #[test]
    fn malformed_read_invocations_are_usage_errors() {
        for (args, why) in [
            (vec!["list"], "a table is required"),
            (vec!["list", "--table", "nonsense"], "unknown table"),
            (
                vec!["list", "--table", "semantic", "--current", "--not-current"],
                "contradiction",
            ),
            (
                vec!["list", "--table", "semantic", "--preview", "0"],
                "empty preview",
            ),
            (
                vec!["list", "--table", "semantic", "--preview", "x"],
                "not a number",
            ),
            (
                vec!["list", "--table", "semantic", "--sql", "x"],
                "no passthrough",
            ),
            (vec!["show", "1,2"], "a table is required"),
            (vec!["show", "--table", "semantic"], "ids are required"),
        ] {
            let mut full = vec!["sm"];
            full.extend(&args);
            let err = Cli::try_parse_from(full).expect_err(why);
            assert!(err.use_stderr(), "{args:?}: {why}");
        }
    }

    fn code(args: &[&str]) -> u8 {
        // ExitCode has no accessor, so exercise the same paths it maps from.
        match Cli::try_parse_from(args) {
            Err(err) => {
                if err.use_stderr() {
                    EXIT_USAGE
                } else {
                    EXIT_OK
                }
            }
            Ok(cli) => {
                let root = sandbox_root();
                let code = match execute(&cli.command, Ok(sandbox_env(&root))) {
                    Ok(()) => EXIT_OK,
                    Err(err) => err.exit_code(),
                };
                let _ = std::fs::remove_dir_all(&root);
                code
            }
        }
    }

    /// A directory of its own for one command: a home with nothing in it, so a
    /// command that writes lands there and never in the real environment.
    fn sandbox_root() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sm-cli-{}-{n}", std::process::id()))
    }

    fn sandbox_env(root: &std::path::Path) -> Env {
        Env {
            vars: std::collections::HashMap::new(),
            home: root.join("home"),
            windows: cfg!(windows),
            dotenv: crate::config::dotenv::DotEnv::absent(root.join("bin").join("sm")),
        }
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            code(&["sm", "seed"]),
            EXIT_USAGE,
            "there is no seed command; an unknown subcommand is a usage error"
        );
        assert_eq!(
            code(&["sm", "coworkers"]),
            EXIT_USAGE,
            "coworkers without a mode is a usage error"
        );
        assert_eq!(
            code(&["sm", "remember"]),
            EXIT_USAGE,
            "remember without its required flags is a usage error"
        );
        assert_eq!(
            code(&["sm", "remember", "--table", "semantic", "--text", "  "]),
            EXIT_REFUSED,
            "an empty --text is refused before the database is opened"
        );
        assert_eq!(
            code(&["sm", "recall"]),
            EXIT_REFUSED,
            "recall with no query and no mode is a refusal, and must not open the database"
        );
        assert_eq!(
            code(&["sm", "recall", "--nope"]),
            EXIT_USAGE,
            "unknown flags are rejected"
        );
        assert_eq!(
            code(&["sm", "embed"]),
            EXIT_USAGE,
            "embed without --missing names no mode"
        );
        for bad in [["--batch", "0"], ["--limit", "0"]] {
            let mut args = vec!["sm", "embed", "--missing"];
            args.extend(bad);
            assert_eq!(code(&args), EXIT_USAGE, "{bad:?} asks for no work");
        }
        assert_eq!(
            code(&["sm", "find-existing-memory", "--quiet", "--json"]),
            EXIT_USAGE,
            "the probe rejects unknown flags too"
        );
        assert_eq!(
            code(&["sm", "init", "--force"]),
            EXIT_USAGE,
            "init rejects unknown flags like every command"
        );
        // Rejected by clap, so nothing runs: a scheduled `backup` with a stray
        // argument must fail, not back up something else.
        assert_eq!(
            code(&["sm", "runbook", "sleep", "setup"]),
            EXIT_USAGE,
            "runbook takes one name at most"
        );
        assert_eq!(
            code(&["sm", "runbook", "no-such-runbook"]),
            EXIT_REFUSED,
            "an unknown name is a refusal"
        );
        assert_eq!(
            code(&["sm", "runbook"]),
            EXIT_OK,
            "no name lists the runbooks"
        );
        assert_eq!(
            code(&["sm", "backup", "--out", "/tmp/x"]),
            EXIT_USAGE,
            "backup takes no arguments"
        );
        // Rejected by clap before anything runs. Never add a restore here that
        // gets past clap: these run against the real environment.
        assert_eq!(
            code(&["sm", "restore"]),
            EXIT_USAGE,
            "restore names no target without --out"
        );
        assert_eq!(
            code(&["sm", "restore", "--out", "/nonexistent/x.db", "--into", "y"]),
            EXIT_USAGE,
            "restore rejects unknown flags"
        );
        // Rejected by clap before anything runs. Never add an update here that
        // gets past clap: these run against the real environment.
        assert_eq!(
            code(&["sm", "update", "--force"]),
            EXIT_USAGE,
            "update rejects unknown flags"
        );
        assert_eq!(
            code(&["sm", "update", "--print-block", "-y"]),
            EXIT_USAGE,
            "printing the block applies nothing, so it does not combine with -y"
        );
        // Validated before anything is probed or read, so this touches nothing.
        assert_eq!(
            code(&["sm", "init", "--episodic-mode", "sometimes"]),
            EXIT_REFUSED,
            "an invalid episodic mode is a refusal"
        );
        assert_eq!(
            code(&["sm", "init", "--db-path", "relative.db", "-y"]),
            EXIT_REFUSED,
            "a relative path cannot go into the .env"
        );
        assert_eq!(code(&["sm", "frobnicate"]), EXIT_USAGE, "unknown command");
        assert_eq!(code(&["sm"]), EXIT_USAGE, "no arguments prints usage");
        assert_eq!(code(&["sm", "--help"]), EXIT_OK, "--help is not a failure");
        assert_eq!(code(&["sm", "--version"]), EXIT_OK);
    }
}
