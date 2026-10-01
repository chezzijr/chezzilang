//! TICKET-185 (wave 17 Family A, "channel hand-off"): a value is delivered to exactly one
//! receiver, never to its own sender, and a `send` returns only once somebody took the value.
//!
//! The axes come from every consumer of that fact (`## Single source` of the ticket):
//! sender context × sender kind × receiver × cap × order × close. One program per
//! (sender context, sender kind) runs every (receiver, cap, order, close) cell as a section and
//! prints `cell <name>: got=[..] sent=[..] st=<status>`. Each run is a fresh process at
//! `CHEZZI_THREADS` 1, 2 and 0 (default), plus one seeded run, under a hard 20 s limit.
//!
//! The order axis is SHAPED by a 200000-iteration spin on the other side; no expectation depends
//! on the spin winning. The Go rule every line is judged by ([`judge`]):
//! 1. every value whose `send` returned (or whose `try_send` returned `true`) is received exactly
//!    once, in order, and nothing else is received (`got == sent`);
//! 2. close none: both sends complete (`st=ok`);
//! 3. close after take (the receiver closes only after it holds the first value, then drains the
//!    buffer): the second send either completed or faulted `send on a closed channel`; on a cap-0
//!    channel it always faults, because nobody can take it;
//! 4. close while parked (no receiver at all): the send ends only by the close, so it faults, and
//!    on a cap-0 channel nothing is received.
//!
//! Oracle evidence — Go 1.27 twins of one cell per (sender kind, close) pair, run with `go run`:
//!
//! ```text
//! send/none          (chan int, receiver parked first):     got=[1 2] sent=[1 2] st=ok
//! send/after-take    (make(chan int), receiver then close): got=[1] sent=[1] st=send on closed channel
//! send/after-take    (make(chan int, 1)):                   got=[1 2] sent=[1 2] st=ok
//! send/while-parked  (make(chan int), close, no receiver):  got=[] sent=[] st=send on closed channel
//! select-send/none   (select { case ch <- v: }):            got=[1 2] sent=[1 2] st=ok
//! select-send/after-take (make(chan int)):                  got=[1] sent=[1] st=send on closed channel
//! select-send/while-parked (make(chan int)):                got=[] sent=[] st=send on closed channel
//! try-send/none      (select { case ch <- v: default: } loop, receiver parked): got=[1 2] sent=[1 2] st=ok
//! ```
//!
//! Go has no try-send close cells in this grid: a spinning `try_send` never observes a close.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Runs `src` at `threads` workers with an optional `CHEZZI_SCHED_SEED`; `None` = still running
/// after `limit` (killed), with whatever stdout it printed.
fn run_with(
    name: &str,
    src: &str,
    threads: &str,
    seed: Option<u32>,
    limit: Duration,
) -> (Option<i32>, String, String) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "chz-t185-{name}-{threads}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(s) = seed {
        cmd.env("CHEZZI_SCHED_SEED", s.to_string());
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s.code().unwrap_or(-1));
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = out_t.join().unwrap();
    let err = err_t.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (status, out, err)
}

const HARNESS: &str = r#"import std.concurrency

fn spin(n: int):
    i := 0
    while i < n:
        i += 1

fn chan_of(cap: int) -> Channel[int]:
    if cap < 0:
        return Channel[int]()
    return Channel[int](cap)

fn drain(ch: Channel[int]) -> List[int]:
    xs: List[int] = List()
    while true:
        match ch.try_recv():
            Some(v): xs.push(v)
            None: return xs
    return xs

fn rx_wait(ch: Channel[int], never: Channel[int]) -> int:
    wait:
        v := ch.recv():
            return v
        _ := never.recv():
            return -1
    return -2

fn rx_one(rk: int, ch: Channel[int], never: Channel[int]) -> int:
    if rk == 0:
        return ch.recv()
    if rk == 1:
        while true:
            match ch.try_recv():
                Some(v): return v
                None: spin(1)
    if rk == 2:
        for v in ch:
            return v
        return -3
    if rk == 3:
        return rx_wait(ch, never)
    if rk == 4:
        return [0].map(fn(_: int) -> int: ch.recv())[0]
    return [0].map(fn(_: int) -> int: rx_wait(ch, never))[0]

fn receiver(rk: int, ch: Channel[int], never: Channel[int], order: int, close: int, got: Channel[int]):
    if order == 0:
        spin(200000)
    if close == 2:
        ch.close()
    else:
        got.send(rx_one(rk, ch, never))
        if close == 0:
            got.send(rx_one(rk, ch, never))
        else:
            ch.close()
    while true:
        match ch.try_recv():
            Some(v): got.send(v)
            None: return

fn send_one(sk: int, ch: Channel[int], never: Channel[int], v: int) -> bool:
    if sk == 0:
        ch.send(v)
        return true
    if sk == 1:
        while not ch.try_send(v):
            spin(1)
        return true
    wait:
        ch.send(v):
            return true
        _ := never.recv():
            return false
    return false

fn sender(sk: int, ch: Channel[int], never: Channel[int], order: int, v1: int, sent: Channel[int], st: Shared[str]):
    if order == 1:
        spin(200000)
    for v in [v1, v1 + 1]:
        r := recover: send_one(sk, ch, never, v)
        match r:
            Ok(_): sent.send(v)
            Err(e):
                st.set(e.message())
                return
    st.set("ok")

fn sender_i(sk: int, ch: Channel[int], never: Channel[int], order: int, v1: int, sent: Channel[int], st: Shared[str]) -> int:
    sender(sk, ch, never, order, v1, sent, st)
    return 0

fn cell(name: str, ctx: int, sk: int, rk: int, cap: int, order: int, close: int, v1: int):
    ch := chan_of(cap)
    never := Channel[int](0)
    got := Channel[int]()
    sent := Channel[int]()
    st := Shared("")
    if ctx == 0:
        parallel:
            spawn: sender(sk, ch, never, order, v1, sent, st)
            spawn: receiver(rk, ch, never, order, close, got)
    elif ctx == 1:
        ex := Executor()
        ex.submit(fn() -> nil: sender(sk, ch, never, order, v1, sent, st))
        parallel:
            spawn: receiver(rk, ch, never, order, close, got)
        ex.shutdown()
    elif ctx == 2:
        ex := Executor()
        ex.submit(fn() -> nil: receiver(rk, ch, never, order, close, got))
        sender(sk, ch, never, order, v1, sent, st)
        ex.shutdown()
    elif ctx == 3:
        parallel:
            spawn: receiver(rk, ch, never, order, close, got)
            sender(sk, ch, never, order, v1, sent, st)
    else:
        parallel:
            spawn:
                _ := [0].map(fn(_: int) -> int: sender_i(sk, ch, never, order, v1, sent, st))
            spawn: receiver(rk, ch, never, order, close, got)
    print("cell {name}: got={drain(got)} sent={drain(sent)} st={st.get()}")

fn main():
"#;

const CTXS: [&str; 5] = ["fiber", "job", "main", "body", "demoted"];
const SKS: [&str; 3] = ["send", "trysend", "waitsend"];
const RKS: [&str; 6] = ["recv", "tryrecv", "forin", "waitrecv", "demrecv", "demwait"];
const CAPS: [i64; 3] = [0, 1, -1];
const ORDERS: [&str; 2] = ["senderfirst", "receiverfirst"];
const CLOSES: [&str; 3] = ["none", "aftertake", "whileparked"];

#[derive(Clone)]
struct Cell {
    name: String,
    cap: i64,
    close: usize,
    v1: i64,
}

/// Every (receiver, cap, order, close) cell for one (context, sender kind) program, and its source.
fn channel_handoff_grid_cells(ctx: usize, sk: usize) -> (Vec<Cell>, String) {
    let mut cells = Vec::new();
    let mut src = HARNESS.to_string();
    let mut v1 = 10i64;
    for (close, close_name) in CLOSES.iter().enumerate() {
        // A spinning `try_send` never observes a close (Go twin: it spins forever).
        if sk == 1 && close != 0 {
            continue;
        }
        for (rk, rk_name) in RKS.iter().enumerate() {
            // Close while parked has no receiver, so the receiver axis collapses to one row.
            if close == 2 && rk != 0 {
                continue;
            }
            for &cap in &CAPS {
                // An unbounded send never parks, so it has no "while parked" cell.
                if close == 2 && cap < 0 {
                    continue;
                }
                // Two polls never meet on a rendezvous channel (Go: same).
                if sk == 1 && rk == 1 && cap == 0 {
                    continue;
                }
                for (order, order_name) in ORDERS.iter().enumerate() {
                    let rk_label = if close == 2 { "none" } else { rk_name };
                    let name = format!(
                        "{}-{}-{}-cap{}-{}-{}",
                        CTXS[ctx], SKS[sk], rk_label, cap, order_name, close_name
                    );
                    src.push_str(&format!(
                        "    cell(\"{name}\", {ctx}, {sk}, {rk}, {cap}, {order}, {close}, {v1})\n"
                    ));
                    cells.push(Cell {
                        name,
                        cap,
                        close,
                        v1,
                    });
                    v1 += 10;
                }
            }
        }
    }
    src.push_str("main()\n");
    (cells, src)
}

fn parse_list(s: &str) -> Option<Vec<i64>> {
    let s = s.strip_prefix('[')?.strip_suffix(']')?;
    if s.is_empty() {
        return Some(vec![]);
    }
    s.split(", ").map(|x| x.parse().ok()).collect()
}

const CLOSED: &str = "send on a closed channel";

/// Judges one printed line against the Go rule in the module doc. `Err` names the violation.
fn judge(cell: &Cell, line: &str) -> Result<(), String> {
    let rest = line
        .strip_prefix(&format!("cell {}: got=", cell.name))
        .ok_or("malformed line")?;
    let (got, rest) = rest.split_once(" sent=").ok_or("malformed line")?;
    let (sent, st) = rest.split_once(" st=").ok_or("malformed line")?;
    let got = parse_list(got).ok_or("malformed got")?;
    let sent = parse_list(sent).ok_or("malformed sent")?;
    let both = vec![cell.v1, cell.v1 + 1];
    if got != sent {
        return Err("a value was lost, duplicated or received out of order (got != sent)".into());
    }
    match cell.close {
        0 if st == "ok" && sent == both => Ok(()),
        0 => Err("both sends must complete".into()),
        1 if got.first() != Some(&cell.v1) => Err("the receiver must hold the first value".into()),
        1 if cell.cap == 0 && st != CLOSED => {
            Err("the second rendezvous send has no taker and must fault closed".into())
        }
        1 if st == "ok" || st == CLOSED => Ok(()),
        1 => Err("unexpected sender status".into()),
        _ if st != CLOSED => Err("a parked send must fault closed".into()),
        _ if cell.cap == 0 && !sent.is_empty() => {
            Err("a rendezvous send with no receiver cannot complete".into())
        }
        _ => Ok(()),
    }
}

/// A whole grid program is one run of every cell: the fiber `send` program takes ~12 s of CPU at
/// T=1 on a debug binary (measured 2026-10-02, base and TICKET-194 alike), so 20 s read as a
/// "hang" under nextest load. A real hang still never finishes.
const LIMIT: Duration = Duration::from_secs(90);

/// The runs every program gets: `(CHEZZI_THREADS, CHEZZI_SCHED_SEED)`.
const RUNS: [(&str, Option<u32>); 4] = [("1", None), ("2", None), ("0", None), ("2", Some(185))];

fn run_label(t: &str, seed: Option<u32>) -> String {
    match seed {
        Some(s) => format!("T={t} seed={s}"),
        None => format!("T={t}"),
    }
}

/// Runs one grid program at every [`RUNS`] entry and returns one failure line per bad cell.
fn check_program(ctx: usize, sk: usize) -> Vec<String> {
    let (cells, src) = channel_handoff_grid_cells(ctx, sk);
    let mut failures = Vec::new();
    for (t, seed) in RUNS {
        let label = run_label(t, seed);
        let (code, out, err) =
            run_with(&format!("{}-{}", CTXS[ctx], SKS[sk]), &src, t, seed, LIMIT);
        let lines: Vec<&str> = out.lines().collect();
        for cell in &cells {
            let prefix = format!("cell {}: ", cell.name);
            match lines.iter().find(|l| l.starts_with(&prefix)) {
                Some(l) => {
                    if let Err(why) = judge(cell, l) {
                        failures.push(format!("{label} {l} -- {why}"));
                    }
                }
                None => {
                    let how = match code {
                        None => format!("hang (killed after {}s)", LIMIT.as_secs()),
                        Some(c) => format!("rc={c} stderr={}", err.trim()),
                    };
                    failures.push(format!("{label} cell {}: missing -- {how}", cell.name));
                    break; // later cells of this run never ran
                }
            }
        }
    }
    failures
}

/// A named program whose exact stdout Go fixes; each run appends a failure line on a mismatch.
fn check_named(name: &str, src: &str, want: &str) -> Vec<String> {
    let mut failures = Vec::new();
    for (t, seed) in RUNS {
        let (code, out, err) = run_with(name, src, t, seed, LIMIT);
        if code != Some(0) || out != want {
            failures.push(format!(
                "{} named {name}: rc={code:?} stdout={out:?} stderr={:?}; Go prints {want:?}",
                run_label(t, seed),
                err.trim()
            ));
        }
    }
    failures
}

/// H1 — the ticket's worker-pool idiom. Go (0/200 bad) prints
/// `received by workers: 100 left in channel: 0 None`-shaped output (Go spells the last `<nil>`).
const H1: &str = r#"import std.concurrency
fn worker(done: Channel[bool], work: Channel[int], n: AtomicInt):
    while true:
        wait:
            _ := done.recv():
                return
            v := work.recv():
                n.add(v)
fn main():
    done := Channel[bool]()
    work := Channel[int](0)
    n := AtomicInt(0)
    parallel:
        for _ in 0..4:
            spawn worker(done, work, n)
        spawn:
            for _ in 0..100:
                work.send(1)
            done.trip()
    print("received by workers:", n.load(), "left in channel:", work.len(), work.try_recv())
main()
"#;

/// H3 — a fiber's rendezvous `wait:` send arm, the receiver inside a native callback. Go: `[1]`.
const H3: &str = r#"fn main():
    ch := Channel[int](0)
    never := Channel[int](0)
    parallel:
        spawn:
            wait:
                ch.send(1):
                    pass
                _ := never.recv():
                    pass
        spawn:
            print([0].map(fn(_: int) -> int: ch.recv()))
main()
"#;

/// The repro — a sender that `recv`s after its `send` must not take its own value back.
/// Go prints `got 1` / `back 2`.
const OWN: &str = r#"fn main():
    ch := Channel[int](0)
    parallel:
        spawn:
            print("got", ch.recv())
            ch.send(2)
        spawn:
            spin := 0
            for i in 0..200000:
                spin += i
            ch.send(1)
            print("back", ch.recv())
main()
"#;

#[test]
fn every_sender_hands_every_value_to_exactly_one_receiver() {
    let mut failures: Vec<String> = Vec::new();
    let jobs: Vec<(usize, usize)> = (0..CTXS.len())
        .flat_map(|c| (0..SKS.len()).map(move |s| (c, s)))
        .collect();
    // Four programs at a time: each is its own process with its own worker pool.
    for chunk in jobs.chunks(4) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|&(c, s)| std::thread::spawn(move || check_program(c, s)))
            .collect();
        for h in handles {
            failures.extend(h.join().unwrap());
        }
    }
    failures.extend(check_named(
        "h1",
        H1,
        "received by workers: 100 left in channel: 0 None\n",
    ));
    failures.extend(check_named("h3", H3, "[1]\n"));
    failures.extend(check_named("own", OWN, "got 1\nback 2\n"));
    assert!(
        failures.is_empty(),
        "{} channel hand-off cells differ from Go:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
