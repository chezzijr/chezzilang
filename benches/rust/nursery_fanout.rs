// Ancestor reference for docs/gaps.md W8-7 / W8-8 — the Rust equivalent of
// examples/primes_parallel.chz: 4 CPU-bound tasks over a fixed N-thread fan-out, identical
// trial-division workload, identical ranges. N comes from argv so the sweep matches --threads.
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    let mut i = 2u64;
    while i * i <= n {
        if n % i == 0 {
            return false;
        }
        i += 1;
    }
    true
}

fn count_primes(lo: u64, hi: u64) -> u64 {
    (lo..hi).filter(|&n| is_prime(n)).count() as u64
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let bounds = vec![
        (2u64, 500000u64),
        (500000, 1000000),
        (1000000, 1500000),
        (1500000, 2000000),
    ];
    // N worker threads pulling from a shared work list — the fixed-width fan-out that makes
    // "N threads means N runners" testable, exactly like --threads=N.
    let work = Arc::new(Mutex::new(bounds));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    for _ in 0..n {
        let work = Arc::clone(&work);
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || loop {
            let job = work.lock().unwrap().pop();
            match job {
                Some((lo, hi)) => tx.send(count_primes(lo, hi)).unwrap(),
                None => break,
            }
        }));
    }
    drop(tx);
    for h in handles {
        h.join().unwrap();
    }
    let total: u64 = rx.iter().sum();
    println!("primes below 2,000,000: {total}");
}
