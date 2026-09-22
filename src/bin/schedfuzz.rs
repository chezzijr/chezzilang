//! Unattended seeded-scheduler fuzzer (TICKET-167, `docs/future.md` §2b). Runs `tests/chz` and the
//! concurrency-flavored `examples/*.chz` under a seed range at one or more worker counts and flags
//! a hang, a panic, a changed exit code, or (where schedule-independent) a changed stdout.
//!
//! Usage:
//!   cargo run --release --bin schedfuzz -- [--seeds A..B] [--threads 1,2] [--timeout-ms N]
//!       [--jobs N] [--program PATH ...] [--filter SUBSTR] [--chezzi PATH]
//!       [--baseline-chezzi PATH]
//!
//! Exit codes: `0` = clean (no findings), `1` = findings, `2` = harness error (bad args, or the
//! baseline binary could not be located). Findings win over a later abort, as in `difffuzz`.

#[path = "../difftest/mod.rs"]
mod difftest;
#[path = "../schedfuzz/mod.rs"]
mod schedfuzz;

use schedfuzz::{
    Baseline, BaselineOutcome, Verdict, corpus, judge, measure_baseline, report_line, run_target,
    target_for,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Args {
    seeds: (u64, u64),
    threads: Vec<usize>,
    timeout: Duration,
    jobs: usize,
    programs: Vec<PathBuf>,
    filter: Option<String>,
    chezzi: PathBuf,
    baseline_chezzi: PathBuf,
}

fn parse_range(s: &str) -> (u64, u64) {
    let parts: Vec<&str> = s.split("..").collect();
    assert!(parts.len() == 2, "range must be A..B");
    (
        parts[0].parse().expect("range start"),
        parts[1].parse().expect("range end"),
    )
}

fn locate_chezzi() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().expect("exe dir");
    let cand = dir.join("chezzi");
    if cand.exists() {
        return cand;
    }
    PathBuf::from("chezzi")
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    let default_chezzi = locate_chezzi();
    let mut seeds = (1u64, 33u64);
    let mut threads = vec![1usize, 2usize];
    let mut timeout = Duration::from_millis(20_000);
    let mut jobs = (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        / 2)
    .max(1);
    let mut programs = Vec::new();
    let mut filter = None;
    let mut chezzi = default_chezzi.clone();
    let mut baseline_chezzi: Option<PathBuf> = None;

    let val = |i: usize, flag: &str| -> String {
        raw.get(i)
            .unwrap_or_else(|| {
                eprintln!("{flag} requires a value");
                std::process::exit(2);
            })
            .clone()
    };

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--seeds" => {
                i += 1;
                seeds = parse_range(&val(i, "--seeds"));
            }
            "--threads" => {
                i += 1;
                threads = val(i, "--threads")
                    .split(',')
                    .map(|s| s.parse().expect("--threads N,N"))
                    .collect();
            }
            "--timeout-ms" => {
                i += 1;
                timeout = Duration::from_millis(val(i, "--timeout-ms").parse().expect("N"));
            }
            "--jobs" => {
                i += 1;
                jobs = val(i, "--jobs").parse().expect("--jobs N");
            }
            "--program" => {
                i += 1;
                programs.push(PathBuf::from(val(i, "--program")));
            }
            "--filter" => {
                i += 1;
                filter = Some(val(i, "--filter"));
            }
            "--chezzi" => {
                i += 1;
                chezzi = PathBuf::from(val(i, "--chezzi"));
            }
            "--baseline-chezzi" => {
                i += 1;
                baseline_chezzi = Some(PathBuf::from(val(i, "--baseline-chezzi")));
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: schedfuzz [--seeds A..B] [--threads 1,2] [--timeout-ms N] [--jobs N] \
                     [--program PATH ...] [--filter SUBSTR] [--chezzi PATH] [--baseline-chezzi PATH]"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    if seeds.1 <= seeds.0 {
        eprintln!(
            "empty or inverted seed range: {}..{} (need end > start)",
            seeds.0, seeds.1
        );
        std::process::exit(2);
    }

    let baseline_chezzi = baseline_chezzi.unwrap_or_else(|| chezzi.clone());
    if !baseline_chezzi.exists() {
        eprintln!("--baseline-chezzi {:?} does not exist", baseline_chezzi);
        std::process::exit(2);
    }

    Args {
        seeds,
        threads,
        timeout,
        jobs: jobs.max(1),
        programs,
        filter,
        chezzi,
        baseline_chezzi,
    }
}

fn main() {
    let args = parse_args();

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut targets = if args.programs.is_empty() {
        corpus(&root)
    } else {
        args.programs.iter().map(|p| target_for(p)).collect()
    };
    if let Some(f) = &args.filter {
        targets.retain(|t| t.path.to_string_lossy().contains(f.as_str()));
    }
    if targets.is_empty() {
        eprintln!("no targets matched");
        std::process::exit(2);
    }

    // One `Vec<(target_idx, seed, threads)>` job list, pulled from a shared `Mutex`-guarded
    // cursor by `args.jobs` worker threads.
    let mut jobs: Vec<(usize, u64, usize)> = Vec::new();
    for (ti, _t) in targets.iter().enumerate() {
        for seed in args.seeds.0..args.seeds.1 {
            for &th in &args.threads {
                jobs.push((ti, seed, th));
            }
        }
    }
    let total_runs = jobs.len();
    let cursor = Mutex::new(0usize);
    let jobs = &jobs;
    let targets = &targets;

    // Per-(target,threads) baseline, measured lazily and cached, so each target's two unseeded
    // reference runs happen only once no matter how many seeds hit it.
    let baseline_cache: Mutex<std::collections::HashMap<(usize, usize), Arc<CachedBaseline>>> =
        Mutex::new(std::collections::HashMap::new());

    let findings = Mutex::new(0usize);
    let unstable = Mutex::new(std::collections::HashSet::new());
    // A harness error (the binary could not even be spawned) is fatal, never "no finding" — see
    // `docs/gaps.md` W7-34 / `src/difftest/run.rs`. Set once, checked by every worker so the run
    // stops pulling new jobs instead of grinding through the rest of the range reporting nothing
    // wrong. `Mutex<Option<_>>`, not an early `exit()` from inside a worker thread: an in-thread
    // exit would skip the `done:` line and any findings already confirmed, the same reason
    // `difffuzz` breaks out of its loop instead of exiting inline.
    let harness_error: Mutex<Option<String>> = Mutex::new(None);

    std::thread::scope(|scope| {
        for _ in 0..args.jobs {
            scope.spawn(|| {
                loop {
                    if harness_error.lock().unwrap().is_some() {
                        break;
                    }
                    let idx = {
                        let mut c = cursor.lock().unwrap();
                        if *c >= jobs.len() {
                            break;
                        }
                        let i = *c;
                        *c += 1;
                        i
                    };
                    let (ti, seed, threads) = jobs[idx];
                    let t = &targets[ti];

                    let cached = {
                        let mut cache = baseline_cache.lock().unwrap();
                        Arc::clone(cache.entry((ti, threads)).or_insert_with(|| {
                            Arc::new(
                                match measure_baseline(&args.baseline_chezzi, t, args.timeout) {
                                    BaselineOutcome::Stable(b) => CachedBaseline::Stable(b),
                                    BaselineOutcome::Unstable(u) => {
                                        CachedBaseline::Unstable(u.reason())
                                    }
                                    BaselineOutcome::HarnessError(msg) => {
                                        CachedBaseline::HarnessError(msg)
                                    }
                                },
                            )
                        }))
                    };
                    let baseline = match cached.as_ref() {
                        CachedBaseline::Stable(b) => b,
                        CachedBaseline::Unstable(reason) => {
                            let mut seen = unstable.lock().unwrap();
                            if seen.insert((ti, threads)) {
                                println!("UNSTABLE program={} reason={reason}", t.path.display());
                            }
                            continue;
                        }
                        CachedBaseline::HarnessError(msg) => {
                            let mut h = harness_error.lock().unwrap();
                            if h.is_none() {
                                eprintln!(
                                    "harness error: baseline {} could not run: {msg}",
                                    t.path.display()
                                );
                                *h = Some(msg.clone());
                            }
                            break;
                        }
                    };

                    let r = run_target(&args.chezzi, t, Some(seed), threads, args.timeout);
                    match judge(baseline, t, &r) {
                        Verdict::Finding(finding) => {
                            *findings.lock().unwrap() += 1;
                            println!("{}", report_line(t, seed, threads, &finding));
                        }
                        Verdict::Clean => {}
                        Verdict::HarnessError(msg) => {
                            let mut h = harness_error.lock().unwrap();
                            if h.is_none() {
                                eprintln!(
                                    "harness error: seed={seed} threads={threads} program={} \
                                     could not run: {msg}",
                                    t.path.display()
                                );
                                *h = Some(msg);
                            }
                            break;
                        }
                    }
                }
            });
        }
    });

    let findings = *findings.lock().unwrap();
    let unstable_n = unstable.lock().unwrap().len();
    let harness_error = harness_error.into_inner().unwrap();
    println!(
        "done: {} targets, {total_runs} runs, {findings} finding(s), {unstable_n} unstable",
        targets.len()
    );
    if harness_error.is_some() {
        eprintln!("ABORTED: the harness broke (see above) — not every job ran");
    }
    // A real finding outranks the abort: exit 1 so it is never masked, same rule as `difffuzz`.
    if findings > 0 {
        if harness_error.is_some() {
            eprintln!("exit 1 (real findings) even though the harness also broke — both above");
        }
        std::process::exit(1);
    }
    if harness_error.is_some() {
        std::process::exit(2);
    }
}

/// A target's per-(threads) baseline, measured once and shared through an `Arc` so every worker
/// thread reads the SAME reference run instead of re-measuring it.
enum CachedBaseline {
    Stable(Baseline),
    Unstable(&'static str),
    HarnessError(String),
}
