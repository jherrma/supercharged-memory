//! The end-to-end suite: the real binary and the Python reference, side by
//! side, on machines built per test and thrown away after it.
//!
//! Every machine is a fresh directory with its own `HOME`, XDG directories,
//! `CLAUDE_CONFIG_DIR`, binary directory (holding the `.env` the binary reads)
//! and backup directory. A child process gets an explicit allow-list of
//! variables and nothing else, so a developer's `SUPERCHARGED_MEMORY_TURSO_PATH`
//! or `EMBED_MODEL` cannot leak into a run. Ollama is a fake one, started on an
//! ephemeral port, answering with vectors derived from the input text; the
//! degraded paths point at a port that refuses. Every database starts as a
//! byte copy of one fixture built from `fixture.sql`.
//!
//! Where a Python reference exists, the same command runs against both and
//! the outputs and the database states they leave are compared. A deliberate
//! difference must be in [`differences::TABLE`] and is asserted as the new
//! behaviour; any other difference fails. Without `python3` or `tursodb` the
//! comparisons are skipped, each by name, and the binary's own assertions
//! still run.

mod backup;
mod bench;
mod coworkers;
mod differences;
mod embed;
mod eval;
mod harness;
mod install;
mod instructions;
mod ollama;
mod recall;
mod runbooks;
mod runner;
mod session;
mod sleep;
mod store;
mod vectors;

fn main() -> std::process::ExitCode {
    let groups: &[&[runner::Test]] = &[
        session::TESTS,
        recall::TESTS,
        store::TESTS,
        embed::TESTS,
        install::TESTS,
        backup::TESTS,
        sleep::TESTS,
        coworkers::TESTS,
        eval::TESTS,
        instructions::TESTS,
        runbooks::TESTS,
        bench::TESTS,
    ];
    runner::main(groups.concat())
}
