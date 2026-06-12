//! Determinism harness CLI (DESIGN.md §3.1, §7).
//!
//! ```text
//! cargo run -p monada-oracle            # check goldens, exit 1 on drift
//! cargo run -p monada-oracle -- --bless # regenerate monada-hashes.txt
//! cargo run -p monada-oracle -- --print # print computed hashes only
//! ```
//!
//! The `check` path is the CI gate: it recomputes the canonical
//! scenario's checkpoints on the current platform and diffs them
//! against the committed `monada-hashes.txt`.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use monada_oracle::{all_checkpoints, diff, parse_goldens, render_goldens, Checkpoint, Verdict};

/// Path to the committed goldens, relative to this crate's manifest.
const GOLDENS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../monada-hashes.txt");

fn main() -> ExitCode {
    let mode = match std::env::args().nth(1).as_deref() {
        None | Some("--check") => Mode::Check,
        Some("--bless") => Mode::Bless,
        Some("--print") => Mode::Print,
        Some(other) => {
            eprintln!("monada-oracle: unknown argument {other:?}");
            eprintln!("usage: monada-oracle [--check | --bless | --print]");
            return ExitCode::from(2);
        }
    };

    let checkpoints = all_checkpoints();
    println!(
        "platform: {}-{} | scripted walk + kernel anchor, {} checkpoints",
        std::env::consts::ARCH,
        std::env::consts::OS,
        checkpoints.len()
    );

    match mode {
        Mode::Print => {
            print_table(&checkpoints, None);
            ExitCode::SUCCESS
        }
        Mode::Bless => match fs::write(GOLDENS_PATH, render_goldens(&checkpoints)) {
            Ok(()) => {
                println!(
                    "blessed {} checkpoints -> {GOLDENS_PATH}",
                    checkpoints.len()
                );
                print_table(&checkpoints, None);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("monada-oracle: failed to write {GOLDENS_PATH}: {e}");
                ExitCode::FAILURE
            }
        },
        Mode::Check => check(&checkpoints),
    }
}

enum Mode {
    Check,
    Bless,
    Print,
}

fn check(checkpoints: &[Checkpoint]) -> ExitCode {
    let text = match fs::read_to_string(Path::new(GOLDENS_PATH)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("monada-oracle: cannot read {GOLDENS_PATH}: {e}");
            eprintln!("  run `cargo run -p monada-oracle -- --bless` to create it.");
            return ExitCode::FAILURE;
        }
    };
    let goldens = match parse_goldens(&text) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("monada-oracle: {GOLDENS_PATH}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let results = diff(checkpoints, &goldens);
    print_table(checkpoints, Some(&results));

    let failures = results.iter().filter(|(_, v)| *v != Verdict::Match).count();
    if failures == 0 {
        println!("OK: all {} checkpoints match goldens.", results.len());
        ExitCode::SUCCESS
    } else {
        eprintln!("DESYNC: {failures} checkpoint(s) diverged from goldens.");
        ExitCode::FAILURE
    }
}

fn print_table(checkpoints: &[Checkpoint], results: Option<&[(Checkpoint, Verdict)]>) {
    for (i, c) in checkpoints.iter().enumerate() {
        let status = match results.and_then(|r| r.get(i)).map(|(_, v)| v) {
            None => String::new(),
            Some(Verdict::Match) => "  ok".to_string(),
            Some(Verdict::MissingGolden) => "  MISSING GOLDEN".to_string(),
            Some(Verdict::Mismatch { golden, got }) => {
                format!("  MISMATCH (golden {golden}, got {got})")
            }
        };
        println!("  {:>12}: {:>20}{}", c.key(), c.hash, status);
    }
}
