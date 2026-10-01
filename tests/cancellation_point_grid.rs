//! TICKET-194 (W18 Family A2): an op is cut by a halt only where it is about to WAIT, or at a loop
//! back-edge (owner decision 1, `docs/root-causes-w18.md`). An op that does not wait -- a send with
//! room, a recv with a value ready, a `try_*` op, a free update guard, an operator or protocol hook,
//! a generator resume -- always completes, so a job that returned never loses its result. Go and
//! CPython keep a finished job's result the same way.
//!
//! Grid: op x state {ready, would wait} x halt {`shutdown_now`, sibling-fault cancel, child fault of
//! an owned nursery, `--timeout`} x `CHEZZI_THREADS` {1, 2, 4, 0}. A ready cell must complete its op
//! in every round; a would-wait cell must be cut (its `after` counter stays 0), except a `try_*` op,
//! which never waits and so always completes. The hook rows are one row per single-call re-entry
//! site (`Vm::reentered`), derived by `grep -n "\.guarded(" src/vm/*.rs` and
//! `grep -n "guarded_walk(" src/vm/*.rs`; the guard rows are every `take_update_guard` caller.
//!
//! Unconstructible cells:
//! - ready x `--timeout`: a party passes a back-edge or a wait, both of which check the deadline,
//!   before it reaches the op.
//! - ready send cap 0: user code cannot see that a receiver parked.
//! - would-wait unbounded send: it never waits.
//! - would-wait hook rows: a hook op never waits.
//! - would-wait `RwShared.read`: it never takes the update guard, and its `RwLock` read lock is
//!   held only for a clone, never across user code or a wait.
//! - guard freed by the halt itself: a holder inside the halt's scope may be cut first, and the act
//!   may then legally find the guard free and complete. Holders here run in their own `hx`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Runs `chezzi <sub> <file>` at `threads` workers with `CHEZZI_SCHED_SEED` removed; `None` = still
/// running after `limit` (killed).
fn run_with(
    name: &str,
    file: &str,
    src: &str,
    sub: &[&str],
    threads: &str,
    limit: Duration,
) -> Option<(i32, String, String)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "chz-t194-{name}-{threads}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(file);
    std::fs::write(&path, src).expect("write program");
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(sub)
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut out = String::new();
    let mut err = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    status.map(|s| (s.code().unwrap_or(-1), out, err))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Halt {
    ShutdownNow,
    Cancel,
    ChildFault,
    Timeout,
}

/// One cell's program text: `out` is the `out` channel's constructor, `prefill` runs before the
/// driver, `act` is the body of `fn act`, `done` the round's pass condition, `holder` the fn that
/// holds an update guard in its own executor (`hold_s`, `hold_w` or `hold_m`).
struct Cell {
    name: String,
    halt: Halt,
    waits: bool,
    out: &'static str,
    prefill: &'static str,
    act: &'static str,
    done: &'static str,
    holder: Option<&'static str>,
    /// A whole `fn round() -> bool` for the std boundary rows; the other fields are then unused.
    round: Option<&'static str>,
}

const DECLS: &str = "struct PA:
    x: int
    fn add(self, other: Self) -> Self:
        return PA(self.x + other.x)
    fn neg(self) -> Self:
        return PA(-self.x)

struct PK:
    x: int
    fn compare(self, other: Self) -> int:
        return self.x - other.x

struct PE:
    x: int
    fn eq(self, o: PE) -> bool:
        return self.x == o.x

struct PH:
    x: int
    fn hash(self) -> int:
        return self.x
    fn eq(self, o: PH) -> bool:
        return self.x == o.x

struct PS:
    x: int
    fn str(self) -> str:
        return \"P\"

struct PI:
    x: int
    fn index(self, k: int) -> int:
        return self.x + k
    fn contains(self, k: int) -> bool:
        return k == self.x

struct PT:
    xs: List[int]
    fn iter(self) -> Iterator[int]:
        return self.xs.iter()

enum EH:
    A(int)
    fn hash(self) -> int:
        match self:
            EH.A(x): return x

newtype NH = int:
    fn hash(self) -> int:
        return int(self)

fn gen(v: int) -> Iterator[int]:
    yield v

fn block_in(held: Channel[int], hold: Channel[int]) -> int:
    held.send(1)
    return hold.recv()

fn hold_s(s: Shared[int], held: Channel[int], hold: Channel[int]):
    s.update(fn(x: int) -> int: x + block_in(held, hold))

fn hold_w(w: RwShared[int], held: Channel[int], hold: Channel[int]):
    w.write(fn(x: int) -> int: x + block_in(held, hold))

fn keep(m: Map[int, int], held: Channel[int], hold: Channel[int]) -> Map[int, int]:
    _ := block_in(held, hold)
    return m

fn hold_m(wm: RwShared[Map[int, int]], held: Channel[int], hold: Channel[int]):
    wm.write(fn(m: Map[int, int]) -> Map[int, int]: keep(m, held, hold))

fn fault(gate: Channel[int]):
    gate.send(7)
    panic(\"boom\")

fn child(gate: Channel[int]):
    _ := gate.recv()
    panic(\"boom\")
";

const PARAMS: &str = "out: Channel[int], src: Channel[int], got: AtomicInt, s: Shared[int], w: \
                      RwShared[int], wm: RwShared[Map[int, int]], after: AtomicInt, held: Channel[int]";
const ARGS: &str = "out, src, got, s, w, wm, after, held";

const LOCALS: &str = "src := Channel[int](1)
got := AtomicInt(0)
s := Shared[int](0)
w := RwShared[int](0)
wm := RwShared[Map[int, int]]({})
after := AtomicInt(0)
held := Channel[int](1)";

fn indent(text: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| format!("{pad}{l}\n"))
        .collect()
}

fn holder_args(h: &str) -> &'static str {
    match h {
        "hold_s" => "s, held, hold",
        "hold_w" => "w, held, hold",
        _ => "wm, held, hold",
    }
}

fn act_fn(c: &Cell) -> String {
    format!(
        "fn act(v: int, {PARAMS}):\n{}    after.add(1)\n",
        indent(c.act, 4)
    )
}

/// A `chezzi run` program for every halt but `--timeout`.
fn run_src(c: &Cell) -> String {
    let rounds = if c.waits { 2 } else { 20 };
    let main = format!(
        "bad := 0\nfor _ in 0..{rounds}:\n    if not round():\n        bad += 1\nprint(\"bad={{bad}}\")\n"
    );
    if let Some(r) = c.round {
        return format!("import std.concurrency\n\n{r}\n{main}");
    }
    let (driver, extra) = match c.halt {
        Halt::ShutdownNow => (
            format!(
                "ex := Executor()\nex.submit(fn(): act(gate.recv(), {ARGS}))\ngate.send(7)\nex.shutdown_now()"
            ),
            String::new(),
        ),
        Halt::Cancel => (
            format!("_ := recover: run(gate, {ARGS})"),
            format!(
                "fn run(gate: Channel[int], {PARAMS}):\n    parallel:\n        spawn: act(gate.recv(), {ARGS})\n        spawn: fault(gate)\n"
            ),
        ),
        Halt::ChildFault => (
            format!("_ := recover: run(gate, {ARGS})"),
            format!(
                "fn run(gate: Channel[int], {PARAMS}):\n    parallel:\n        spawn: child(gate)\n        gate.send(7)\n        act(7, {ARGS})\n"
            ),
        ),
        Halt::Timeout => unreachable!("--timeout cells run as a test file"),
    };
    let mut body = format!("gate := Channel[int](0)\nout := {}\n{LOCALS}\n", c.out);
    if let Some(h) = c.holder {
        body += &format!(
            "hold := Channel[int](0)\nhx := Executor()\nhx.submit(fn(): {h}({}))\n",
            holder_args(h)
        );
    }
    body += &format!("{}\n{driver}\n", c.prefill);
    if c.holder.is_some() {
        body += "hold.send(0)\nhx.shutdown()\n";
    }
    body += &format!("return {}\n", c.done);
    format!(
        "import std.concurrency\n\n{DECLS}\n{}\n{extra}\nfn round() -> bool:\n{}\n{main}",
        act_fn(c),
        indent(&body, 4)
    )
}

/// A `chezzi test --timeout=300` file for a would-wait cell.
fn timeout_src(c: &Cell) -> String {
    let mut body = format!("out := {}\n{LOCALS}\n", c.out);
    if c.holder.is_some() {
        body += "hold := Channel[int](0)\n";
    }
    body += &format!("{}\nparallel:\n", c.prefill);
    body += "    spawn: time.sleep_ms(5000)\n";
    if let Some(h) = c.holder {
        body += &format!("    spawn: {h}({})\n", holder_args(h));
    }
    body += &format!("    act(7, {ARGS})\n");
    format!(
        "import std.concurrency\nimport std.time\n\n{DECLS}\n{}\ntest fn cell():\n{}",
        act_fn(c),
        indent(&body, 4)
    )
}

/// One op row: its two states as (prefill, act, done, holder), `None` where unconstructible.
struct Op {
    name: &'static str,
    out: &'static str,
    ready: Option<(&'static str, &'static str, &'static str)>,
    wait: Option<(
        &'static str,
        &'static str,
        &'static str,
        Option<&'static str>,
    )>,
}

fn ops() -> Vec<Op> {
    let send_wait = ("out.send(0)", "out.send(v)", "after.load() == 0", None);
    vec![
        Op {
            name: "send cap 1",
            out: "Channel[int](1)",
            ready: Some(("", "out.send(v)", "out.len() == 1")),
            wait: Some(send_wait),
        },
        Op {
            name: "send cap 0",
            out: "Channel[int](0)",
            ready: None,
            wait: Some(("", "out.send(v)", "after.load() == 0", None)),
        },
        Op {
            name: "unbounded send",
            out: "Channel[int]()",
            ready: Some(("", "out.send(v)", "out.len() == 1")),
            wait: None,
        },
        Op {
            name: "recv",
            out: "Channel[int](1)",
            ready: Some(("src.send(7)", "got.add(src.recv())", "got.load() == 7")),
            wait: Some(("", "got.add(src.recv())", "after.load() == 0", None)),
        },
        Op {
            name: "try_send",
            out: "Channel[int](1)",
            ready: Some(("", "_ := out.try_send(v)", "out.len() == 1")),
            wait: Some((
                "out.send(0)",
                "_ := out.try_send(v)",
                "after.load() == 1",
                None,
            )),
        },
        Op {
            name: "try_recv",
            out: "Channel[int](1)",
            ready: Some((
                "src.send(7)",
                "match src.try_recv():\n    Some(x): got.add(x)\n    None: pass",
                "got.load() == 7",
            )),
            wait: Some((
                "",
                "match src.try_recv():\n    Some(x): got.add(x)\n    None: pass",
                "after.load() == 1",
                None,
            )),
        },
        Op {
            name: "wait: send arm",
            out: "Channel[int](1)",
            ready: Some(("", "wait:\n    out.send(v): pass", "out.len() == 1")),
            wait: Some((
                "out.send(0)",
                "wait:\n    out.send(v): pass",
                "after.load() == 0",
                None,
            )),
        },
        Op {
            name: "wait: recv arm",
            out: "Channel[int](1)",
            ready: Some((
                "src.send(7)",
                "wait:\n    x := src.recv(): got.add(x)",
                "got.load() == 7",
            )),
            wait: Some((
                "",
                "wait:\n    x := src.recv(): got.add(x)",
                "after.load() == 0",
                None,
            )),
        },
        Op {
            name: "for v in ch",
            out: "Channel[int](1)",
            ready: Some((
                "src.send(7)\nsrc.close()",
                "for x in src:\n    got.add(x)",
                "got.load() == 7",
            )),
            wait: Some((
                "",
                "for x in src:\n    got.add(x)",
                "after.load() == 0",
                None,
            )),
        },
        Op {
            name: "Shared.update",
            out: "Channel[int](1)",
            ready: Some(("", "s.update(fn(x: int) -> int: x + v)", "s.get() == 7")),
            wait: Some((
                "",
                "_ := held.recv()\ns.update(fn(x: int) -> int: x + v)",
                "after.load() == 0",
                Some("hold_s"),
            )),
        },
        Op {
            name: "RwShared.write",
            out: "Channel[int](1)",
            ready: Some(("", "w.write(fn(x: int) -> int: x + v)", "w.get() == 7")),
            wait: Some((
                "",
                "_ := held.recv()\nw.write(fn(x: int) -> int: x + v)",
                "after.load() == 0",
                Some("hold_w"),
            )),
        },
        Op {
            name: "Shared.set",
            out: "Channel[int](1)",
            ready: Some(("", "s.set(v)", "s.get() == 7")),
            wait: Some((
                "",
                "_ := held.recv()\ns.set(v)",
                "after.load() == 0",
                Some("hold_s"),
            )),
        },
        Op {
            name: "RwShared.set",
            out: "Channel[int](1)",
            ready: Some(("", "w.set(v)", "w.get() == 7")),
            wait: Some((
                "",
                "_ := held.recv()\nw.set(v)",
                "after.load() == 0",
                Some("hold_w"),
            )),
        },
        Op {
            name: "RwShared.set_key",
            out: "Channel[int](1)",
            ready: Some(("", "wm.set_key(1, v)", "wm.get().len() == 1")),
            wait: Some((
                "",
                "_ := held.recv()\nwm.set_key(1, v)",
                "after.load() == 0",
                Some("hold_m"),
            )),
        },
    ]
}

/// One row per single-call re-entry site (`## Single source` item 3 of TICKET-194 maps each site
/// to its row). Ready only; each must leave `got == 7`.
const HOOKS: [(&str, &str); 13] = [
    ("+ (struct_arith)", "got.add((PA(v) + PA(0)).x)"),
    ("unary - (neg)", "got.add(0 - (-PA(v)).x)"),
    ("> (struct_compare)", "if PK(v) > PK(0):\n    got.add(7)"),
    ("== (eq hook)", "if PE(v) == PE(7):\n    got.add(7)"),
    ("[] (index)", "got.add(PI(v)[0])"),
    ("in (op_contains)", "if 7 in PI(v):\n    got.add(7)"),
    (
        "struct hash",
        "m: Map[PH, int] = {}\nm[PH(v)] = v\ngot.add(m.len() * 7)",
    ),
    ("str hook", "if str(PS(v)) == \"P\":\n    got.add(7)"),
    (
        "generator .next()",
        "g := gen(v)\nmatch g.next():\n    Some(x): got.add(x)\n    None: pass",
    ),
    (
        "for x in struct iter()",
        "for x in PT([v]):\n    got.add(x)",
    ),
    (
        "enum hash",
        "me: Map[EH, int] = {}\nme[EH.A(v)] = v\ngot.add(me.len() * 7)",
    ),
    (
        "newtype hash",
        "mn: Map[NH, int] = {}\nmn[NH(v)] = v\ngot.add(mn.len() * 7)",
    ),
    ("RwShared.read", "got.add(w.read(fn(x: int) -> int: x + v))"),
];

const SUBMIT_RESULT_ROUND: &str = "fn round() -> bool:
    gate := Channel[int](0)
    ex := Executor()
    o := ex.submit_result(fn() -> int: gate.recv())
    gate.send(7)
    ex.shutdown_now()
    ok := false
    match o.try_recv():
        Some(x): ok = x == 7
        None: pass
    return ok
";

const SUBMIT_OUTCOME_ROUND: &str = "fn round() -> bool:
    gate := Channel[int](0)
    ex := Executor()
    out := Channel[int](1)
    errc := Channel[str](1)
    ex.submit_outcome(fn() -> int: gate.recv(), out, errc)
    gate.send(7)
    ex.shutdown_now()
    ok := false
    match out.try_recv():
        Some(x): ok = x == 7
        None: pass
    match errc.try_recv():
        Some(_): ok = false
        None: pass
    return ok
";

const BLANK: Cell = Cell {
    name: String::new(),
    halt: Halt::ShutdownNow,
    waits: false,
    out: "Channel[int](1)",
    prefill: "",
    act: "",
    done: "",
    holder: None,
    round: None,
};

fn cells() -> Vec<Cell> {
    let mut v = Vec::new();
    for op in ops() {
        for halt in [
            Halt::ShutdownNow,
            Halt::Cancel,
            Halt::ChildFault,
            Halt::Timeout,
        ] {
            if let Some((prefill, act, done)) = op.ready
                && halt != Halt::Timeout
            {
                v.push(Cell {
                    name: format!("{} ready {halt:?}", op.name),
                    halt,
                    out: op.out,
                    prefill,
                    act,
                    done,
                    ..BLANK
                });
            }
            if let Some((prefill, act, done, holder)) = op.wait {
                v.push(Cell {
                    name: format!("{} would-wait {halt:?}", op.name),
                    halt,
                    waits: true,
                    out: op.out,
                    prefill,
                    act,
                    done,
                    holder,
                    ..BLANK
                });
            }
        }
    }
    for (name, act) in HOOKS {
        for halt in [Halt::ShutdownNow, Halt::Cancel, Halt::ChildFault] {
            v.push(Cell {
                name: format!("hook {name} ready {halt:?}"),
                halt,
                act,
                done: "got.load() == 7",
                ..BLANK
            });
        }
    }
    v.push(Cell {
        name: "submit_result ready ShutdownNow".into(),
        round: Some(SUBMIT_RESULT_ROUND),
        ..BLANK
    });
    v.push(Cell {
        name: "submit_outcome ready ShutdownNow".into(),
        round: Some(SUBMIT_OUTCOME_ROUND),
        ..BLANK
    });
    v
}

/// Runs one cell at T=1/2/4/0; returns one line per miss.
fn check_cell(c: &Cell) -> Vec<String> {
    let tag: String = c
        .name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let limit = Duration::from_secs(20);
    let mut misses = Vec::new();
    for t in ["1", "2", "4", "0"] {
        let (got, ok) = if c.halt == Halt::Timeout {
            let got = run_with(
                &tag,
                "cell_test.chz",
                &timeout_src(c),
                &["test", "--timeout=300"],
                t,
                limit,
            );
            let ok = matches!(&got, Some((_, out, _)) if out.lines().any(|l| l.starts_with("TIMED-OUT ")));
            (got, ok)
        } else {
            let got = run_with(&tag, "main.chz", &run_src(c), &["run"], t, limit);
            let ok = matches!(&got, Some((0, out, _)) if out == "bad=0\n");
            (got, ok)
        };
        if !ok {
            let what = match got {
                None => "hang (killed after 20s)".to_string(),
                Some((code, out, err)) => format!(
                    "rc={code} stdout={:?} stderr={:?}",
                    out.chars().take(160).collect::<String>(),
                    err.chars().take(200).collect::<String>()
                ),
            };
            misses.push(format!("{} T={t}: {what}", c.name));
        }
    }
    misses
}

/// Every op x state x halt x worker count: an op is cut only where it would wait.
#[test]
fn every_op_is_cut_only_where_it_would_wait() {
    let cells = cells();
    let next = AtomicUsize::new(0);
    let misses = Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for _ in 0..4 {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = cells.get(i) else { break };
                    let m = check_cell(c);
                    misses.lock().unwrap().extend(m);
                }
            });
        }
    });
    let mut misses = misses.into_inner().unwrap();
    misses.sort();
    assert!(
        misses.is_empty(),
        "{} misses over {} cells x 4 worker counts:\n{}",
        misses.len(),
        cells.len(),
        misses.join("\n")
    );
}
