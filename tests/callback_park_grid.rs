//! The callback-park grid (plan 2026-11-07, frame-based re-entry): a channel wait inside a hook, a
//! HOF callback, a box closure, a generator body or a `defer` body, reached from every caller, at
//! T=1, 2 and 4. With no sender it must report `deadlock` (exit 1, the same text at every T), as
//! Go reports `fatal error: all goroutines are asleep - deadlock!` for every one of these shapes.
//! With a live sender it must print the program's golden output (no false deadlock).
//!
//! `LANDED` is the last plan task that has landed. A kind whose task is later still hangs in a
//! non-`spawn` caller (the W23-1 residual before the fix); raising `LANDED` flips its cells.
//!
//! A `defer` kind's cells also read `CHEZZI_SCHED_STATS=1` and must report `demotes=0`: the wait
//! parks its task and never hands the worker's slot to a replacement thread (plan
//! 2026-11-07-defer-drain-frames; Go parks a goroutine that waits in a deferred func).

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The last plan task (2-7) whose kinds now park or are judged.
const LANDED: u8 = 6;

/// When a kind's `nothing` cell turns from a hang into `deadlock` in a non-`spawn` caller.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Fix {
    /// at this plan task
    Task(u8),
    /// a `defer` kind: already `deadlock`, but its wait demoted until this task of plan
    /// 2026-11-07-defer-drain-frames drains it on frames; every cell must report `demotes=0`
    Drain(u8),
    /// judged at this plan task, as `Task`, and its guard wait parks: every cell must report
    /// `demotes=0` (plan 2026-11-08-park-guard-and-join; Go parks a goroutine on `mu.Lock()`)
    Parks(u8),
    /// never: a nested container walk keeps the nested loop (narrowed W23-1, out of scope)
    Residual,
}

/// One callback kind: `prelude` declares what `act` needs, `act` (unindented) waits through `w()`
/// and prints; `out` is its stdout with a live sender. `cut` is its stdout with no sender when the
/// wait is a cancelled task's cleanup: the verdict cuts the stuck cleanup and the sibling's fault is
/// the report (`docs/reference/concurrency/05-cancel.md`), so the run exits 0 instead of `deadlock`.
struct Kind {
    name: &'static str,
    fix: Fix,
    prelude: &'static str,
    act: &'static str,
    out: &'static str,
    cut: Option<&'static str>,
}

// Go oracle (run on go1.27, 2026-11-07, no sender on `ch`): `<-ch` inside a `slices.IndexFunc`
// callback, under `mu.Lock()`, inside a `slices.SortFunc` comparator, inside a range-over-func
// iterator and inside a deferred func each print `fatal error: all goroutines are asleep -
// deadlock!`. Go is the owner for concurrency.
const KINDS: &[Kind] = &[
    // ---- box closures (Task 5). Go: a goroutine blocked holding `sync.Mutex` -> deadlock.
    Kind {
        name: "update",
        fix: Fix::Task(5),
        prelude: "s := Shared[int](0)",
        act: "s.update(fn(v: int) -> int: v + w())\nprint(s.read(fn(v: int) -> int: v))",
        out: "7\n",
        cut: None,
    },
    Kind {
        name: "rw_write",
        fix: Fix::Task(5),
        prelude: "r := RwShared[int](0)",
        act: "r.write(fn(v: int) -> int: v + w())\nprint(r.get())",
        out: "7\n",
        cut: None,
    },
    Kind {
        name: "rw_read",
        fix: Fix::Task(5),
        prelude: "r := RwShared[int](0)",
        act: "print(r.read(fn(v: int) -> int: v + w()))",
        out: "7\n",
        cut: None,
    },
    // a second task waits on the guard the caller holds while it is parked in its closure. Go: a
    // goroutine blocked on `mu.Lock()` behind a holder blocked on `<-ch` -> deadlock; with a
    // sender both finish.
    Kind {
        name: "update_contend",
        fix: Fix::Parks(5),
        prelude: "s := Shared[int](0)\nheld := Channel[int](1)\n\nfn hold(v: int) -> int:\n    held.send(1)\n    return v + w()",
        act: "parallel:\n    spawn:\n        _ := held.recv()\n        s.update(fn(v: int) -> int: v + 1)\n    s.update(hold)\nprint(s.get())",
        out: "8\n",
        cut: None,
    },
    // ---- HOF callbacks (Task 3). Go: `slices.IndexFunc(xs, func(v int) bool { <-ch ... })`.
    Kind {
        name: "map",
        fix: Fix::Task(3),
        prelude: "",
        act: "print([1].map(fn(v: int) -> int: v + w()))",
        out: "[8]\n",
        cut: None,
    },
    Kind {
        name: "filter",
        fix: Fix::Task(3),
        prelude: "",
        act: "print([1].filter(fn(v: int) -> bool: w() > v))",
        out: "[1]\n",
        cut: None,
    },
    Kind {
        name: "fold",
        fix: Fix::Task(3),
        prelude: "",
        act: "print([1].fold(0, fn(a: int, v: int) -> int: a + v + w()))",
        out: "8\n",
        cut: None,
    },
    Kind {
        name: "take_while",
        fix: Fix::Task(3),
        prelude: "",
        act: "print([1].take_while(fn(v: int) -> bool: w() > v))",
        out: "[1]\n",
        cut: None,
    },
    Kind {
        name: "count",
        fix: Fix::Task(3),
        prelude: "",
        act: "print([1].count(fn(v: int) -> bool: w() > v))",
        out: "1\n",
        cut: None,
    },
    Kind {
        name: "min_by",
        fix: Fix::Task(3),
        prelude: "",
        act: "print([2, 1].min_by(fn(v: int) -> int: v + w() - 7))",
        out: "1\n",
        cut: None,
    },
    // the key pass of `sort_by_key` (the key fn waits)
    Kind {
        name: "sort_by_key",
        fix: Fix::Task(3),
        prelude: "",
        act: "xs := [2, 1]\nxs.sort_by_key(fn(v: int) -> int: v + w() - 7)\nprint(xs)",
        out: "[1, 2]\n",
        cut: None,
    },
    Kind {
        name: "from_iter",
        fix: Fix::Task(3),
        prelude: "struct It:\n    n: int\n    fn next(self) -> int?:\n        if self.n == 0:\n            return None\n        self.n -= 1\n        return self.n + w() - 7",
        act: "print(List.from_iter(It(1)))",
        out: "[0]\n",
        cut: None,
    },
    // an f-string part whose `str` hook waits. Go: `fmt.Sprintf("%v", x)` with a blocking `String()`.
    Kind {
        name: "fstr",
        fix: Fix::Task(2),
        prelude: "struct T:\n    n: int\n    fn str(self) -> str: return \"s{w()}\"",
        act: "print(\"v={T(1)}\")",
        out: "v=s7\n",
        cut: None,
    },
    Kind {
        name: "fstr_fmt",
        fix: Fix::Task(2),
        prelude: "struct T:\n    n: int\n    fn str(self) -> str: return \"s{w()}\"",
        act: "print(\"v={T(1):>4}\")",
        out: "v=  s7\n",
        cut: None,
    },
    // ---- sorts (Task 4). Go: `slices.SortFunc(xs, func(a, b int) int { <-ch ... })`.
    Kind {
        name: "sort_by",
        fix: Fix::Task(4),
        prelude: "",
        act: "xs := [2, 1]\nxs.sort_by(fn(a: int, b: int) -> int: a - b + w() - 7)\nprint(xs)",
        out: "[1, 2]\n",
        cut: None,
    },
    // the compare pass of `sort_by_key` (the struct key's `compare` waits)
    Kind {
        name: "sort_by_key_cmp",
        fix: Fix::Task(4),
        prelude: "struct K:\n    n: int\n    fn eq(self, other: K) -> bool: return self.n == other.n\n    fn compare(self, other: K) -> int: return self.n - other.n + w() - 7",
        act: "xs := [2, 1]\nxs.sort_by_key(fn(v: int) -> K: K(v))\nprint(xs)",
        out: "[1, 2]\n",
        cut: None,
    },
    Kind {
        name: "sort",
        fix: Fix::Task(4),
        prelude: "struct K:\n    n: int\n    fn eq(self, other: K) -> bool: return self.n == other.n\n    fn compare(self, other: K) -> int: return self.n - other.n + w() - 7",
        act: "ks := [K(2), K(1)]\nks.sort()\nprint(ks)",
        out: "[K(n=1), K(n=2)]\n",
        cut: None,
    },
    // a stable `sort_by` over equal keys: the comparator waits on every call, so each merge step
    // is a frame; equal keys keep their input order.
    Kind {
        name: "sort_stable",
        fix: Fix::Task(4),
        prelude: "struct P:\n    k: int\n    tag: str",
        act: "ps := [P(2, \"a\"), P(1, \"b\"), P(2, \"c\"), P(1, \"d\"), P(1, \"e\")]\nps.sort_by(fn(a: P, b: P) -> int: a.k - b.k + w() - 7)\nprint(ps.map(fn(p: P) -> str: p.tag))",
        out: "['b', 'd', 'e', 'a', 'c']\n",
        cut: None,
    },
    // `xs.min()` over structs: the element's `compare` waits.
    Kind {
        name: "min_struct",
        fix: Fix::Task(4),
        prelude: "struct K:\n    n: int\n    fn eq(self, other: K) -> bool: return self.n == other.n\n    fn compare(self, other: K) -> int: return self.n - other.n + w() - 7",
        act: "print([K(2), K(1)].min())",
        out: "K(n=1)\n",
        cut: None,
    },
    // ---- shallow operator hooks (Task 2). Go has no operator overloading; the hook is a method
    // call, so the Go answer is the plain-call answer: deadlock.
    Kind {
        name: "binop",
        fix: Fix::Task(2),
        prelude: "struct A:\n    n: int\n    fn add(self, other: A) -> A: return A(self.n + other.n + w())",
        act: "print((A(1) + A(2)).n)",
        out: "10\n",
        cut: None,
    },
    Kind {
        name: "neg",
        fix: Fix::Task(2),
        prelude: "struct A:\n    n: int\n    fn neg(self) -> A: return A(w() - self.n)",
        act: "print((-A(1)).n)",
        out: "6\n",
        cut: None,
    },
    Kind {
        name: "index",
        fix: Fix::Task(2),
        prelude: "struct I:\n    n: int\n    fn index(self, k: int) -> int: return k + w()",
        act: "print(I(1)[2])",
        out: "9\n",
        cut: None,
    },
    Kind {
        name: "set_index",
        fix: Fix::Task(2),
        prelude: "struct I:\n    n: int\n    fn index(self, k: int) -> int: return k\n    fn set_index(self, k: int, v: int) -> None:\n        self.n = k + v + w()",
        act: "x := I(1)\nx[2] = 3\nprint(x.n)",
        out: "12\n",
        cut: None,
    },
    Kind {
        name: "slice",
        fix: Fix::Task(2),
        prelude: "struct I:\n    n: int\n    fn slice(self, r: Range) -> int: return w()",
        act: "print(I(1)[0..2])",
        out: "7\n",
        cut: None,
    },
    Kind {
        name: "contains",
        fix: Fix::Task(2),
        prelude: "struct I:\n    n: int\n    fn contains(self, item: int) -> bool: return item == w()",
        act: "print(7 in I(1))",
        out: "true\n",
        cut: None,
    },
    Kind {
        name: "lt",
        fix: Fix::Task(2),
        prelude: "struct K:\n    n: int\n    fn eq(self, other: K) -> bool: return self.n == other.n\n    fn compare(self, other: K) -> int: return self.n - other.n + w() - 7",
        act: "print(K(1) < K(2))",
        out: "true\n",
        cut: None,
    },
    Kind {
        name: "eq",
        fix: Fix::Task(2),
        prelude: "struct E:\n    n: int\n    fn eq(self, other: E) -> bool: return self.n == other.n + w() - 7",
        act: "print(E(1) == E(1))",
        out: "true\n",
        cut: None,
    },
    Kind {
        name: "str_hook",
        fix: Fix::Task(3),
        prelude: "struct T:\n    n: int\n    fn str(self) -> str: return \"s{w()}\"",
        act: "print(T(1))",
        out: "s7\n",
        cut: None,
    },
    // every `print` arg with a `str` hook, any argc, a runtime `sep`
    Kind {
        name: "print_sep",
        fix: Fix::Task(3),
        prelude: "struct T:\n    n: int\n    fn str(self) -> str: return \"s{w()}\"",
        act: "print(1, T(1), sep=\"-\")",
        out: "1-s7\n",
        cut: None,
    },
    Kind {
        name: "iter_hook",
        fix: Fix::Task(2),
        prelude: "struct IT:\n    n: int\n    fn iter(self) -> Iterator[int]: return [self.n + w()].iter()",
        act: "for v in IT(1):\n    print(v)",
        out: "8\n",
        cut: None,
    },
    // ---- generator body (Task 6). Go: a range-over-func iterator whose body does `<-ch`.
    Kind {
        name: "generator",
        fix: Fix::Task(6),
        prelude: "fn gen() -> Iterator[int]:\n    yield w()",
        act: "for v in gen():\n    print(v)",
        out: "7\n",
        cut: None,
    },
    // a generator that yields across a park, twice
    Kind {
        name: "gen_park_twice",
        fix: Fix::Task(6),
        prelude: "fn gen() -> Iterator[int]:\n    yield w()\n    yield w() + 1",
        act: "for v in gen():\n    print(v)",
        out: "7\n8\n",
        cut: None,
    },
    // a generator whose body iterates an inner generator that waits
    Kind {
        name: "gen_nested",
        fix: Fix::Task(6),
        prelude: "fn inner() -> Iterator[int]:\n    yield w()\nfn outer() -> Iterator[int]:\n    for v in inner():\n        yield v + 1",
        act: "for v in outer():\n    print(v)",
        out: "8\n",
        cut: None,
    },
    Kind {
        name: "gen_from_iter",
        fix: Fix::Task(6),
        prelude: "fn gen() -> Iterator[int]:\n    yield w()\n    yield 2",
        act: "print(List.from_iter(gen()))",
        out: "[7, 2]\n",
        cut: None,
    },
    Kind {
        name: "gen_next",
        fix: Fix::Task(6),
        prelude: "fn gen() -> Iterator[int]:\n    yield w()",
        act: "g := gen()\nprint(g.next())\nprint(g.next())",
        out: "7\nNone\n",
        cut: None,
    },
    // ---- defer body (plan 2026-11-07-defer-drain-frames: Task 2 drains a return, Task 3 a fault,
    // a catch and a cancel on frames, so the wait parks instead of demoting).
    // Go: `defer func() { <-ch }()` -> deadlock.
    Kind {
        name: "defer",
        fix: Fix::Drain(2),
        prelude: "fn viadefer():\n    defer:\n        print(w())\n    pass",
        act: "viadefer()",
        out: "7\n",
        cut: None,
    },
    // a defer that waits while its frame returns a value
    Kind {
        name: "defer_return",
        fix: Fix::Drain(2),
        prelude: "fn viadefer() -> int:\n    defer:\n        print(w())\n    return 5",
        act: "print(viadefer())",
        out: "7\n5\n",
        cut: None,
    },
    // a defer that waits while a fault unwinds its frame; the fault is still caught after it
    Kind {
        name: "defer_fault",
        fix: Fix::Drain(3),
        prelude: "fn boom():\n    defer:\n        print(w())\n    x := 0\n    print(1 / x)",
        act: "r := recover:\n    boom()\nmatch r:\n    ?_: print(\"none\")\n    !e: print(\"caught: {e}\")",
        out: "7\ncaught: division by zero\n",
        cut: None,
    },
    // a defer inside the `recover:` block that catches the fault; the catch still runs after it
    Kind {
        name: "defer_recover",
        fix: Fix::Drain(3),
        prelude: "",
        act: "r := recover:\n    defer:\n        print(w())\n    x := 0\n    print(1 / x)\nmatch r:\n    ?_: print(\"none\")\n    !e: print(\"caught: {e}\")",
        out: "7\ncaught: division by zero\n",
        cut: None,
    },
    // a deferred struct method call that waits
    Kind {
        name: "defer_method",
        fix: Fix::Drain(2),
        prelude: "struct C:\n    n: int\n    fn close(self):\n        print(self.n + w())\n\nfn viadefer():\n    c := C(1)\n    defer c.close()\n    pass",
        act: "viadefer()",
        out: "8\n",
        cut: None,
    },
    // a deferred native call that waits
    Kind {
        name: "defer_native",
        fix: Fix::Drain(2),
        prelude: "fn viadefer():\n    defer ch.recv()\n    print(1)",
        act: "viadefer()",
        out: "1\n",
        cut: None,
    },
    // a task cancelled by a sibling's fault runs its defer, which waits; the fault is caught after
    Kind {
        name: "defer_cancel",
        fix: Fix::Drain(3),
        prelude: "ready := Channel[int](0)\ngate := Channel[int](0)",
        act: "r := recover:\n    parallel:\n        spawn:\n            defer:\n                print(w())\n            ready.send(1)\n            _ := gate.recv()\n        spawn:\n            _ := ready.recv()\n            x := 0\n            print(1 / x)\nmatch r:\n    ?_: print(\"none\")\n    !e: print(\"caught: {e}\")",
        out: "7\ncaught: division by zero\n",
        cut: Some("caught: division by zero\n"),
    },
    // a task's return-path defer waits, and a sibling's fault then cancels its scope: the parked
    // cleanup is not requeued by the cancel (a `defer` is never cancelled); the fault is caught after
    Kind {
        name: "defer_return_cancel",
        fix: Fix::Drain(2),
        prelude: "ready := Channel[int](0)",
        act: "r := recover:\n    parallel:\n        spawn:\n            defer:\n                print(w())\n            ready.send(1)\n        spawn:\n            _ := ready.recv()\n            x := 0\n            print(1 / x)\nmatch r:\n    ?_: print(\"none\")\n    !e: print(\"caught: {e}\")",
        out: "7\ncaught: division by zero\n",
        cut: Some("caught: division by zero\n"),
    },
    // ---- residuals: a hook reached from a nested container walk keeps the nested loop.
    Kind {
        name: "nested_eq",
        fix: Fix::Residual,
        prelude: "struct E:\n    n: int\n    fn eq(self, other: E) -> bool: return self.n == other.n + w() - 7",
        act: "print([E(1)] == [E(1)])",
        out: "true\n",
        cut: None,
    },
    Kind {
        name: "nested_str",
        fix: Fix::Residual,
        prelude: "struct T:\n    n: int\n    fn str(self) -> str: return \"s{w()}\"",
        act: "print([T(1)])",
        out: "[s7]\n",
        cut: None,
    },
    Kind {
        name: "map_key_hash",
        fix: Fix::Residual,
        prelude: "struct H:\n    n: int\n    fn eq(self, other: H) -> bool: return self.n == other.n\n    fn hash(self) -> int: return self.n + w() - 7",
        act: "m := {H(1): 2}\nprint(m[H(1)])",
        out: "2\n",
        cut: None,
    },
];

/// Every program's head: `w()` takes one value from `ch`, or 7 once the sender closed it.
const HEAD: &str = "import Shared, RwShared from std.concurrency
import Iterator from std.iter

ch := Channel[int](0)

fn w() -> int:
    match ch.recv():
        ?v: return v
        !_: return 7
";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Caller {
    /// in `main`'s `parallel:` body
    Main,
    /// in a fn that `main`'s `parallel:` body calls
    Helper,
    /// in a top-level `parallel:` body (no `main`)
    Toplevel,
    /// in a `spawn:` task
    Spawn,
    /// in a `test fn`'s `parallel:` body, run by `chezzi test`
    TestFn,
}

impl Caller {
    fn named(name: &str) -> Caller {
        match name {
            "main" => Caller::Main,
            "helper" => Caller::Helper,
            "toplevel" => Caller::Toplevel,
            "spawn" => Caller::Spawn,
            "test_fn" => Caller::TestFn,
            other => panic!("no caller {other}"),
        }
    }
}

fn indent(text: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    text.lines().map(|l| format!("{pad}{l}\n")).collect()
}

/// The sibling that feeds `w()`: one send, then a close so any later `w()` returns at once.
fn sibling(n: usize) -> String {
    indent("spawn:\n    ch.send(7)\n    ch.close()", n)
}

fn program(kind: &Kind, caller: Caller, live: bool) -> String {
    let sib = |n| if live { sibling(n) } else { String::new() };
    let body = match caller {
        Caller::Main => format!(
            "fn main():\n    parallel:\n{}{}main()\n",
            sib(8),
            indent(kind.act, 8)
        ),
        Caller::Helper => format!(
            "fn helper():\n{}fn main():\n    parallel:\n{}        helper()\nmain()\n",
            indent(kind.act, 4),
            sib(8)
        ),
        Caller::Toplevel => format!("parallel:\n{}{}", sib(4), indent(kind.act, 4)),
        Caller::Spawn => format!(
            "fn main():\n    parallel:\n        spawn:\n{}{}main()\n",
            indent(kind.act, 12),
            sib(8)
        ),
        Caller::TestFn => format!(
            "test fn t():\n    parallel:\n{}{}",
            sib(8),
            indent(kind.act, 8)
        ),
    };
    format!("{HEAD}{}\n{body}", kind.prelude)
}

/// What one cell must do.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Expect {
    /// exit 0; stdout is the golden (`chezzi run`) or holds `1 passed` (`chezzi test`)
    Out,
    /// exit 1 with `deadlock`, the same text at every T
    Deadlock,
    /// exit 0; stdout is the kind's `cut` golden (`chezzi run`) or holds `1 passed` (`chezzi test`)
    Cut,
    /// still running at the probe: the W23-1 hang (pinned until its task lands)
    Hang,
}

fn expect(kind: &Kind, caller: Caller, live: bool) -> Expect {
    if live {
        return Expect::Out;
    }
    if kind.cut.is_some() {
        return Expect::Cut;
    }
    // A `spawn:` task demotes today and parks after the plan: both are judged.
    if caller == Caller::Spawn {
        return Expect::Deadlock;
    }
    match kind.fix {
        Fix::Drain(_) => Expect::Deadlock,
        Fix::Task(t) | Fix::Parks(t) if t <= LANDED => Expect::Deadlock,
        Fix::Task(_) | Fix::Parks(_) | Fix::Residual => Expect::Hang,
    }
}

/// Runs `src` at `threads` workers in a fresh dir; `None` = still running after `limit` (killed).
/// The stderr it returns has no `[sched-stats]` line; see [`run_counted`].
fn run(
    tag: &str,
    src: &str,
    test_fn: bool,
    threads: &str,
    limit: Duration,
) -> Option<(i32, String, String)> {
    run_counted(tag, src, test_fn, threads, limit).map(|(got, _)| got)
}

/// [`run`] plus the run's demote count, read from the `[sched-stats] ... demotes=<n>` line that
/// `CHEZZI_SCHED_STATS=1` prints last on stderr (`None` if the line is missing).
fn run_counted(
    tag: &str,
    src: &str,
    test_fn: bool,
    threads: &str,
    limit: Duration,
) -> Option<((i32, String, String), Option<usize>)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "chz-cbpark-{tag}-{threads}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = if test_fn { "main_test.chz" } else { "main.chz" };
    std::fs::write(dir.join(file), src).expect("write program");
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg(if test_fn { "test" } else { "run" })
        .arg(file)
        .current_dir(&dir)
        .env("CHEZZI_THREADS", threads)
        .env("CHEZZI_SCHED_STATS", "1")
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
        std::thread::sleep(Duration::from_millis(20));
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
    let mut demotes = None;
    let err: String = err
        .lines()
        .filter(|l| match l.strip_prefix("[sched-stats] ") {
            Some(stats) => {
                demotes = stats
                    .split(' ')
                    .find_map(|f| f.strip_prefix("demotes="))
                    .and_then(|n| n.parse().ok());
                false
            }
            None => true,
        })
        .map(|l| format!("{l}\n"))
        .collect();
    status.map(|s| ((s.code().unwrap_or(-1), out, err), demotes))
}

/// Runs one (kind, caller) row, `nothing` and `live`, each at T=1, 2 and 4.
fn run_cell(kind: &str, caller: &str) {
    let kind = KINDS
        .iter()
        .find(|k| k.name == kind)
        .unwrap_or_else(|| panic!("no kind {kind} in KINDS"));
    let caller = Caller::named(caller);
    let mut misses = Vec::new();
    for live in [false, true] {
        misses.extend(check_cell(kind, caller, live));
    }
    assert!(misses.is_empty(), "{}", misses.join("\n"));
}

/// Runs one cell at T=1, 2 and 4; returns one line per miss.
fn check_cell(kind: &Kind, caller: Caller, live: bool) -> Vec<String> {
    let want = expect(kind, caller, live);
    let src = program(kind, caller, live);
    let test_fn = caller == Caller::TestFn;
    let name = format!(
        "{}/{caller:?}/{}",
        kind.name,
        if live { "live" } else { "nothing" }
    );
    let limit = match want {
        Expect::Hang => Duration::from_secs(2),
        Expect::Out | Expect::Cut | Expect::Deadlock => Duration::from_secs(10),
    };
    let tag = name.replace('/', "-");
    let mut misses = Vec::new();
    let mut verdicts: Vec<(&str, String)> = Vec::new();
    for t in ["1", "2", "4"] {
        let counted = run_counted(&tag, &src, test_fn, t, limit);
        if let Some((_, demotes)) = &counted
            && matches!(kind.fix, Fix::Drain(_) | Fix::Parks(_))
            && *demotes != Some(0)
        {
            misses.push(format!(
                "{name} T={t}: [sched-stats] demotes={demotes:?}; expected Some(0) (the wait must park, not demote)"
            ));
        }
        let got = counted.map(|(got, _)| got);
        let ok = match (&got, want) {
            (None, Expect::Hang) => true,
            (Some((0, out, _)), Expect::Out) => {
                if test_fn {
                    out.contains("1 passed")
                } else {
                    out == kind.out
                }
            }
            (Some((0, out, _)), Expect::Cut) => {
                if test_fn {
                    out.contains("1 passed")
                } else {
                    Some(out.as_str()) == kind.cut
                }
            }
            (Some((1, out, err)), Expect::Deadlock) => {
                // `chezzi test` reports the verdict on stdout, `chezzi run` on stderr.
                let text = if test_fn { out } else { err };
                verdicts.push((t, text.clone()));
                text.contains("deadlock")
            }
            _ => false,
        };
        if !ok {
            let what = match got {
                None => "hang".to_string(),
                Some((code, out, err)) => format!(
                    "rc={code} stdout={out:?} stderr={:?}",
                    err.chars().take(200).collect::<String>()
                ),
            };
            misses.push(format!("{name} T={t}: {what}; expected {want:?}"));
        }
    }
    if let Some((t0, first)) = verdicts.first() {
        for (t, text) in &verdicts[1..] {
            if text != first {
                misses.push(format!(
                    "{name}: the deadlock text differs between T={t0} and T={t}:\n{first}\n---\n{text}"
                ));
            }
        }
    }
    misses
}

grid_tests!(KIND_ROWS x CALLER_ROWS: [
    update, rw_write, rw_read, update_contend,
    map, filter, fold, take_while, count, min_by, sort_by_key, from_iter, fstr, fstr_fmt,
    sort_by, sort_by_key_cmp, sort, sort_stable, min_struct,
    binop, neg, index, set_index, slice, contains, lt, eq, str_hook, print_sep, iter_hook,
    generator, gen_park_twice, gen_nested, gen_from_iter, gen_next, defer,
    defer_return, defer_fault, defer_recover, defer_method, defer_native, defer_cancel,
    defer_return_cancel,
    nested_eq, nested_str, map_key_hash,
] x [main, helper, toplevel, spawn, test_fn] => crate::run_cell);

/// A fault inside a generator body after the body parked: `recover:` catches it (the generator
/// is done), and the uncaught one prints the base binary's trace (`97012a20`), byte-identical,
/// at T=1 and T=4: the generator frame's caller is the `for` that resumed it.
#[test]
fn generator_fault_after_park() {
    const SRC: &str = r#"import Iterator from std.iter

ch := Channel[int](0)

fn w() -> int:
    match ch.recv():
        ?v: return v
        !_: return 7

fn boom(n: int) -> int:
    return 10 / n

fn gen(n: int) -> Iterator[int]:
    yield w()
    yield boom(n)

fn drive():
    r := recover:
        for v in gen(0):
            print(v)
    match r:
        ?_: print("no fault")
        !e: print("caught: {e}")
    for v in gen(0):
        print(v)

fn main():
    parallel:
        spawn:
            ch.send(7)
            ch.send(8)
        drive()

main()
"#;
    const OUT: &str = "7\ncaught: division by zero\n8\n";
    const ERR: &str = "runtime error (main.chz:11:12): division by zero\n  at boom (called at main.chz:15:11)\n  at gen (called at main.chz:24:5)\n  at drive (called at main.chz:32:9)\n  at main (called at main.chz:34:1)\n";
    for t in ["1", "4"] {
        let got = run("gen-fault", SRC, false, t, Duration::from_secs(10));
        assert_eq!(got, Some((1, OUT.to_string(), ERR.to_string())), "T={t}");
    }
}

/// A deadlock cut drops a task parked in a generator body: the generator is done, not left
/// running, so a later `next()` gets `None`, as the base binary (`97012a20`) prints at every T.
#[test]
fn generator_done_after_cut() {
    const SRC: &str = r#"import Iterator from std.iter
ch := Channel[int](0)

fn gen() -> Iterator[int]:
    yield 1
    n := ch.recv() ?? 0
    yield n

g := gen()

test fn a_deadlocks():
    assert g.next() == ?1
    parallel:
        spawn:
            _ := g.next()
    assert true

test fn b_uses_gen():
    r := g.next()
    print(r)
    assert r == None
"#;
    for t in ["1", "2", "4"] {
        let got = run("gen-cut", SRC, true, t, Duration::from_secs(10));
        let Some((1, out, _)) = &got else {
            panic!("T={t}: {got:?}");
        };
        assert!(
            out.contains("PASS b_uses_gen")
                && out.contains("2 test(s): 1 passed, 0 failed, 1 errored"),
            "T={t}: {out}"
        );
    }
}

/// A sibling's fault cancels the scope of a task parked in its return-path `defer` that nothing
/// can finish: the verdict cuts that wait as the cancel, and the sibling's fault is the report at
/// every scheduler seed, as the base binary (`fb8edf37`) prints. Seeds 2 and 4 give the task a
/// reduction budget it spends mid-drain, so the cut must travel with the task across its yield.
#[test]
fn defer_cut_keeps_the_cancel_across_a_yield() {
    const SRC: &str = r#"import std.time
stuck := Channel[int](0)
fn boomer():
    time.sleep_ms(50)
    print("boomer panics")
    panic("boom")
fn kid_stuck():
    defer:
        print("kid defer")
        print(stuck.recv())
    time.sleep_ms(1)
    print("kid returns")
fn sib_nursery():
    parallel:
        spawn kid_stuck()
        spawn boomer()
sib_nursery()
"#;
    let dir = std::env::temp_dir().join(format!("chz-cbpark-defercut-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(dir.join("main.chz"), SRC).expect("write program");
    let mut misses = Vec::new();
    for seed in 1..=5 {
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .args(["run", "--threads=1", "main.chz"])
            .current_dir(&dir)
            .env("CHEZZI_SCHED_SEED", seed.to_string())
            .stdin(Stdio::null())
            .output()
            .expect("run chezzi");
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.starts_with("runtime error (main.chz:6:5): boom\n") {
            misses.push(format!("seed={seed}: {err}"));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(misses.is_empty(), "{}", misses.join("\n"));
}

/// The verdict cuts a cleanup that can never finish as the cause's victim: the cause is the
/// report (05-cancel.md "Cleanup that can NEVER complete"), as the base binary (`fb8edf37`)
/// prints, through every verdict path that meets a fiber parked in a `defer` (the aborting-family
/// cut, the sched-wide verdict, a joined leaf, a join-parked owner, a sleep before the wait).
/// `stderr` is compared on its first line only.
#[test]
fn stuck_cleanup_reports_the_cause_on_every_verdict_path() {
    let cells: &[(&str, &str, i32, &str, &str)] = &[
        // a cancelled task's defer sleeps, then waits forever
        (
            "sleep",
            "import std.time\nstuck := Channel[int](0)\nready := Channel[int](0)\nfn kid():\n    defer:\n        time.sleep_ms(30)\n        print(\"slept\")\n        print(stuck.recv())\n    ready.send(1)\n    _ := stuck.recv()\nfn boomer():\n    _ := ready.recv()\n    panic(\"boom\")\nfn main():\n    r := recover:\n        parallel:\n            spawn kid()\n            spawn boomer()\n    print(\"caught {r}\")\nmain()\n",
            0,
            "slept\ncaught !boom\n",
            "",
        ),
        // a cancelled task's defer joins a nursery whose child waits forever
        (
            "join",
            "stuck := Channel[int](0)\nready := Channel[int](0)\nfn waiter():\n    print(stuck.recv())\nfn kid():\n    defer:\n        parallel:\n            spawn waiter()\n    ready.send(1)\n    _ := stuck.recv()\nfn boomer():\n    _ := ready.recv()\n    panic(\"boom\")\nfn main():\n    r := recover:\n        parallel:\n            spawn kid()\n            spawn boomer()\n    print(\"caught {r}\")\nmain()\n",
            0,
            "caught !boom\n",
            "",
        ),
        // a cancelled leaf nursery, its child's return-path defer waits forever
        (
            "leaf",
            "stuck := Channel[int](0)\nready := Channel[int](0)\nfn kid():\n    defer:\n        print(stuck.recv())\n    ready.send(1)\nfn boomer():\n    _ := ready.recv()\n    panic(\"boom\")\nfn inner():\n    parallel:\n        spawn kid()\n        spawn boomer()\nfn main():\n    r := recover:\n        parallel:\n            spawn inner()\n    print(\"caught {r}\")\nmain()\n",
            0,
            "caught !boom\n",
            "",
        ),
        // an owner cut by its child's fault joins, inside its defer, a child that waits forever
        (
            "joinown",
            "stuck := Channel[int](0)\nfn waiter():\n    print(stuck.recv())\nfn faulty():\n    panic(\"boom\")\nfn owner():\n    defer:\n        parallel:\n            spawn waiter()\n    parallel:\n        spawn faulty()\nfn main():\n    parallel:\n        spawn owner()\nmain()\n",
            1,
            "",
            "runtime error (main.chz:5:5): boom\n",
        ),
        // an owner in a joined leaf, cut by its child's fault, whose defer waits forever
        (
            "leafown",
            "stuck := Channel[int](0)\nfn faulty():\n    panic(\"boom\")\nfn owner():\n    defer:\n        print(stuck.recv())\n    parallel:\n        spawn faulty()\nfn inner():\n    parallel:\n        spawn owner()\nfn main():\n    parallel:\n        spawn inner()\nmain()\n",
            1,
            "",
            "runtime error (main.chz:3:5): boom\n",
        ),
    ];
    let mut misses = Vec::new();
    for &(tag, src, rc, out, err) in cells {
        for t in ["1", "2", "4"] {
            let got = run(tag, src, false, t, Duration::from_secs(10));
            let ok = got.as_ref().is_some_and(|(c, o, e)| {
                let first = e
                    .lines()
                    .next()
                    .map(|l| format!("{l}\n"))
                    .unwrap_or_default();
                *c == rc && o == out && first == err
            });
            if !ok {
                misses.push(format!("{tag} T={t}: {got:?}"));
            }
        }
    }
    assert!(misses.is_empty(), "{}", misses.join("\n"));
}

/// A genuine deadlock's stuck cleanup (a task's return-path `defer` waits on a channel nothing
/// sends to, no fault anywhere) is listed with its OWN wait and site, as the base binary
/// (`fb8edf37`) prints: the verdict's cut of the parked `defer` adds the fiber's park site to the
/// run's listing. `--errors=json` adds only the checker's `[]` line, so this one golden owns both.
#[test]
fn a_stuck_defer_is_listed_with_its_own_wait() {
    let src = "fn worker(ch: Channel[int]):\n    defer ch.recv()\n    print(\"body\")\n\nfn main():\n    ch := Channel[int](0)\n    parallel:\n        spawn worker(ch)\n\nmain()\n";
    let err = "runtime error (main.chz:2:11): deadlock: 1 task is blocked and cannot proceed\n  task 1: recv() on an empty channel at main.chz:2:11\n    at worker (called at main.chz:8:15)\n";
    let mut misses = Vec::new();
    for t in ["1", "2", "4"] {
        let got = run("stuckdefer", src, false, t, Duration::from_secs(10));
        if got.as_ref() != Some(&(1, "body\n".to_string(), err.to_string())) {
            misses.push(format!("T={t}: {got:?}"));
        }
    }
    assert!(misses.is_empty(), "{}", misses.join("\n"));
}

/// Every kind in `KINDS` has a row, so none is left untested.
#[test]
fn every_kind_has_a_row() {
    let names: Vec<&str> = KINDS.iter().map(|k| k.name).collect();
    assert_eq!(names, KIND_ROWS);
}
