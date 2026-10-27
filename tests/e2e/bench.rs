//! The benchmark driver is never run from here -- `bench/bench.py` runs on
//! demand -- but two things about it are checked: its own statistics pass
//! their self-test, and its fake Ollama gives exactly this suite's vectors, so
//! a benchmark measures the corpus this suite verified.

use std::path::Path;
use std::process::Command;

use crate::harness::{Ctx, repo};
use crate::runner::Test;
use crate::vectors;

pub const TESTS: &[Test] = crate::tests![
    the_bench_driver_passes_its_self_test,
    the_bench_fake_ollama_gives_the_suites_vectors,
];

const T: &str = "bench";

fn python(ctx: &Ctx, args: &[&str]) -> Option<std::process::Output> {
    let py = ctx.python.as_ref()?;
    let scratch = ctx.work.join("bench-driver");
    std::fs::create_dir_all(&scratch).unwrap();
    Some(
        Command::new(&py.python3)
            .arg("-B")
            .arg(repo().join("bench/bench.py"))
            .args(args)
            .env_clear()
            .env("HOME", &scratch)
            .env("TMPDIR", &scratch)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .unwrap(),
    )
}

fn the_bench_driver_passes_its_self_test(ctx: &Ctx) {
    // The measuring launcher too, when this build holds it: `cargo test`
    // builds the examples, `cargo test --test e2e` alone does not.
    let launcher = Path::new(env!("CARGO_BIN_EXE_supercharged-memory"))
        .with_file_name("examples")
        .join(if cfg!(windows) {
            "bench-measure.exe"
        } else {
            "bench-measure"
        });
    let launcher = launcher.display().to_string();
    let mut args = vec!["--self-test"];
    if Path::new(&launcher).is_file() {
        args.extend(["--measure", &launcher]);
    }
    let Some(out) = python(ctx, &args) else {
        return ctx.skip(T, "bench/bench.py --self-test");
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.ends_with("self-test ok\n"),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn the_bench_fake_ollama_gives_the_suites_vectors(ctx: &Ctx) {
    let texts = [
        "",
        "A brand new fact about gamma rays and delta waves.",
        "UPPER and lower CASE, digits 1024 and 0.7.2",
        "Grüße — non-ASCII letters split words: naïve café",
        "Never commit or push without an explicit request; leave every change in the working tree.\n\nKeywords: git, commit, push, policy",
    ];
    for text in texts {
        let Some(out) = python(ctx, &["--vector", text]) else {
            return ctx.skip(T, "bench/bench.py --vector");
        };
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let theirs: Vec<i64> = serde_json::from_slice(&out.stdout).unwrap();
        let ours: Vec<i64> = vectors::vector(text).into_iter().map(i64::from).collect();
        assert_eq!(theirs, ours, "the vectors of {text:?} differ");
    }
}
