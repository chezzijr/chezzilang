//! TICKET-298 (wave 24 F1) — a family cut re-labelled as a cancel must release a Shared guard
//! `CUT`, so no waiter reads the half-done value, and the run ends in a fatal deadlock.
//!
//! Under the one global check (plan 2026-11-17) no family cut is left: a holder stuck in a `defer`
//! keeps its guard, and the run ends once every task is asleep, with one report that lists each
//! task at its wait. A lock cycle and a re-lock of a held guard just block, and the same report
//! lists them at their guard wait. `CUT` stays for a holder that a per-test verdict drops in
//! `chezzi test`. Go twin of every fatal cell: `all goroutines are asleep - deadlock!`.

use std::process::Command;

/// The one deadlock header: every task of the run is blocked.
const HEADER: &str = "deadlock: every task is blocked";

/// The listing line of a task blocked at a guard of either box kind.
fn lists_a_guard_wait(stderr: &str) -> bool {
    stderr.contains("waiting for the Shared guard at ")
        || stderr.contains("waiting for the RwShared guard at ")
}

const SRC: &str = r#"import Shared from std.concurrency
import sleep_ms from std.time
s := Shared([0])
never := Channel[int](0)
fn stuck(xs: List[int]) -> List[int]:
    xs.push(1)
    _ := never.recv()
    xs.push(2)
    return xs
parallel:
    spawn:
        r := recover:
            parallel:
                spawn:
                    defer s.update(stuck)
                    sleep_ms(1000)
                spawn:
                    sleep_ms(20)
                    panic("boom")
        print("inner", r)
    spawn:
        sleep_ms(100)
        print("C sees", s.get())
print("end")
"#;

#[test]
fn family_cut_with_stuck_update_in_defer_does_not_release_the_guard_normally() {
    let dir = std::env::temp_dir().join(format!("chz-ticket298-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("cut_shared.chz");
    std::fs::write(&path, SRC).expect("write fixture");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", "1")
        .output()
        .expect("run chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("C sees [0, 1]"),
        "a waiter read the half-done value (stdout: {stdout:?})"
    );
    assert!(
        out.status.code() == Some(1) && stderr.contains(HEADER),
        "expected a fatal `{HEADER}` at rc 1, got {} (stderr: {stderr})",
        out.status
    );
}

/// The guard a cell's holder takes: `Shared.update`, `RwShared.write` or `RwShared.read`.
#[derive(Clone, Copy, Debug)]
enum Res {
    Shared,
    RwWrite,
    RwRead,
}

impl Res {
    const ALL: [Res; 3] = [Res::Shared, Res::RwWrite, Res::RwRead];

    /// This variant's index in `ALL`; the exhaustive match makes a new variant fail the build.
    fn index(self) -> usize {
        match self {
            Res::Shared => 0,
            Res::RwWrite => 1,
            Res::RwRead => 2,
        }
    }

    /// (constructor, import, hold method, waiter's acquire, closure return type) of the
    /// `List[int]` box most cells use.
    fn list_box(
        self,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ) {
        match self {
            Res::Shared => (
                "Shared([0])",
                "Shared",
                "s.update",
                "s.update(peekw)",
                "List[int]",
            ),
            Res::RwWrite => (
                "RwShared([0])",
                "RwShared",
                "s.write",
                "s.write(peekw)",
                "List[int]",
            ),
            Res::RwRead => (
                "RwShared([0])",
                "RwShared",
                "s.read",
                "s.write(peekw)",
                "int",
            ),
        }
    }
}

/// Why the guard holder was cut (or not), one row per cause of the TICKET-298 grid.
#[derive(Clone, Copy, Debug)]
enum Cause {
    SchedBody,
    SchedDefer,
    FamilyUpdateInDefer,
    FamilyDeferInUpdate,
    FamilyNoWaiter,
    FreshAfterFamily,
    CancelBody,
    ExecCancelBody,
    ExecCancelDefer,
    ExecCancelDeferMain,
    OsExit,
    LockCycle,
    SelfReentry,
    ReentryInDefer,
    ChildReentryJoin,
    CancelDeferReentry,
    MainBody,
    MainDeferInUpdate,
    NurseryJoinInUpdate,
}

impl Cause {
    const ALL: [Cause; 19] = [
        Cause::SchedBody,
        Cause::SchedDefer,
        Cause::FamilyUpdateInDefer,
        Cause::FamilyDeferInUpdate,
        Cause::FamilyNoWaiter,
        Cause::FreshAfterFamily,
        Cause::CancelBody,
        Cause::ExecCancelBody,
        Cause::ExecCancelDefer,
        Cause::ExecCancelDeferMain,
        Cause::OsExit,
        Cause::LockCycle,
        Cause::SelfReentry,
        Cause::ReentryInDefer,
        Cause::ChildReentryJoin,
        Cause::CancelDeferReentry,
        Cause::MainBody,
        Cause::MainDeferInUpdate,
        Cause::NurseryJoinInUpdate,
    ];

    /// This variant's index in `ALL`; the exhaustive match makes a new variant fail the build.
    fn index(self) -> usize {
        match self {
            Cause::SchedBody => 0,
            Cause::SchedDefer => 1,
            Cause::FamilyUpdateInDefer => 2,
            Cause::FamilyDeferInUpdate => 3,
            Cause::FamilyNoWaiter => 4,
            Cause::FreshAfterFamily => 5,
            Cause::CancelBody => 6,
            Cause::ExecCancelBody => 7,
            Cause::ExecCancelDefer => 8,
            Cause::ExecCancelDeferMain => 9,
            Cause::OsExit => 10,
            Cause::LockCycle => 11,
            Cause::SelfReentry => 12,
            Cause::ReentryInDefer => 13,
            Cause::ChildReentryJoin => 14,
            Cause::CancelDeferReentry => 15,
            Cause::MainBody => 16,
            Cause::MainDeferInUpdate => 17,
            Cause::NurseryJoinInUpdate => 18,
        }
    }

    /// What the run must end with (owner decisions F1, answers 1(a) and 2(b); Q4 kept).
    fn expect(self) -> Expect {
        match self {
            Cause::FamilyUpdateInDefer
            | Cause::FamilyDeferInUpdate
            | Cause::FamilyNoWaiter
            | Cause::FreshAfterFamily
            | Cause::ExecCancelDefer
            | Cause::ExecCancelDeferMain => Expect::FatalHolder,
            // A cycle or a re-lock blocks; the one report lists it at its guard wait (Go twin of a
            // re-`Lock` or an AB-BA `Lock`: fatal, all asleep).
            Cause::LockCycle
            | Cause::SelfReentry
            | Cause::ReentryInDefer
            | Cause::ChildReentryJoin
            | Cause::CancelDeferReentry => Expect::FatalGuard,
            Cause::SchedBody
            | Cause::SchedDefer
            | Cause::MainBody
            | Cause::MainDeferInUpdate
            | Cause::NurseryJoinInUpdate => Expect::Fatal,
            Cause::CancelBody | Cause::ExecCancelBody => Expect::Kept,
            Cause::OsExit => Expect::Exit,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Expect {
    /// exit status 1, a `deadlock:` report, no torn read; the report lists the holder at its wait
    FatalHolder,
    /// exit status 1, a `deadlock:` report, no torn read
    Fatal,
    /// exit status 1, a `deadlock:` report that lists a task at its guard wait, no torn read
    FatalGuard,
    /// Q4 (out of scope): a cancel of a holder outside a `defer` publishes its partial change
    Kept,
    /// `os.exit(3)` ends the run with its own status
    Exit,
}

/// The cell program. `@NAME@` placeholders keep Chezzi's `{}` out of `format!`.
fn prog(r: Res, c: Cause) -> String {
    match c {
        Cause::LockCycle => return lock_cycle(r),
        Cause::SelfReentry => return self_reentry(r),
        Cause::CancelDeferReentry => return cancel_defer_reentry(r),
        Cause::SchedBody
        | Cause::SchedDefer
        | Cause::FamilyUpdateInDefer
        | Cause::FamilyDeferInUpdate
        | Cause::FamilyNoWaiter
        | Cause::FreshAfterFamily
        | Cause::CancelBody
        | Cause::ExecCancelBody
        | Cause::ExecCancelDefer
        | Cause::ExecCancelDeferMain
        | Cause::OsExit
        | Cause::ReentryInDefer
        | Cause::ChildReentryJoin
        | Cause::MainBody
        | Cause::MainDeferInUpdate
        | Cause::NurseryJoinInUpdate => {}
    }
    let (ctor, imp, hold, wait, ht) = r.list_box();
    let ret = if ht == "int" { "return 0" } else { "return xs" };
    let usev = if ht == "int" { "_ := " } else { "" };
    let mut head = String::from(
        "import @IMP@, Executor from std.concurrency
import sleep_ms from std.time
import exit from std.os
s := @CTOR@
never := Channel[int](0)
got := Channel[List[int]](1)
fn stuck(xs: List[int]) -> @HT@:
    xs.push(1)
    _ := never.recv()
    xs.push(2)
    @RET@
fn slowc(xs: List[int]) -> @HT@:
    xs.push(1)
    sleep_ms(1000)
    xs.push(2)
    @RET@
fn stuckd(xs: List[int]) -> @HT@:
    xs.push(1)
    defer never.recv()
    sleep_ms(1000)
    xs.push(2)
    @RET@
fn nest(xs: List[int]) -> @HT@:
    xs.push(1)
    parallel:
        spawn:
            _ := never.recv()
    xs.push(2)
    @RET@
fn peekw(xs: List[int]) -> List[int]:
    got.send(xs.copy())
    return xs
fn waiter():
    sleep_ms(100)
    @WAIT@
    print(\"C sees\", got.recv())
",
    );
    let family = |h: &str, tail: &str| {
        format!(
            "parallel:\n    spawn:\n        r := recover:\n            parallel:\n                spawn:\n{h}                spawn:\n                    sleep_ms(20)\n                    panic(\"boom\")\n        print(\"inner\", r)\n{tail}"
        )
    };
    let i24 = " ".repeat(24);
    let waiter_tail = "    spawn waiter()\nprint(\"end\")\n";
    let body = match c {
        Cause::SchedBody => format!(
            "parallel:\n    spawn:\n        {usev}{hold}(stuck)\n    spawn waiter()\nprint(\"end\")\n"
        ),
        Cause::SchedDefer => format!(
            "parallel:\n    spawn:\n        defer {hold}(stuck)\n        sleep_ms(10)\n    spawn waiter()\nprint(\"end\")\n"
        ),
        Cause::FamilyUpdateInDefer => family(
            &format!("{i24}defer {hold}(stuck)\n{i24}sleep_ms(1000)\n"),
            waiter_tail,
        ),
        Cause::FamilyDeferInUpdate => family(&format!("{i24}{usev}{hold}(stuckd)\n"), waiter_tail),
        Cause::CancelBody => family(&format!("{i24}{usev}{hold}(slowc)\n"), waiter_tail),
        Cause::FamilyNoWaiter => family(
            &format!("{i24}defer {hold}(stuck)\n{i24}sleep_ms(1000)\n"),
            "print(\"end\")\n",
        ),
        Cause::FreshAfterFamily => family(
            &format!("{i24}defer {hold}(stuck)\n{i24}sleep_ms(1000)\n"),
            "print(\"after\", s.get())\n",
        ),
        Cause::ExecCancelDefer => executor_cell(&format!(
            "fn job():\n    defer {hold}(stuck)\n    sleep_ms(1000)\n"
        )),
        Cause::ExecCancelBody => executor_cell(&format!("fn job():\n    {usev}{hold}(slowc)\n")),
        Cause::ExecCancelDeferMain => format!(
            "fn job():\n    defer {hold}(stuck)\n    sleep_ms(1000)\nex := Executor()\nt := ex.submit(job)\nsleep_ms(20)\nex.cancel()\nex.shutdown()\nprint(\"job\", t.get())\nwaiter()\nprint(\"end\")\n"
        ),
        Cause::OsExit => format!(
            "parallel:\n    spawn:\n        {usev}{hold}(stuck)\n    spawn:\n        sleep_ms(50)\n        exit(3)\n    spawn waiter()\nprint(\"end\")\n"
        ),
        Cause::ReentryInDefer | Cause::ChildReentryJoin => {
            head = head.replace(
                "import sleep_ms",
                "import Shared as Sh from std.concurrency\nimport sleep_ms",
            );
            head.push_str("t := Sh(0)\n");
            let reentry = match r {
                Res::Shared | Res::RwWrite => "s.get()",
                Res::RwRead => "s.set([5])",
            };
            let f = if c.index() == Cause::ReentryInDefer.index() {
                format!(
                    "fn f(xs: List[int]) -> {ht}:\n    xs.push(1)\n    sleep_ms(300)\n    defer {reentry}\n    {ret}\n"
                )
            } else {
                format!(
                    "fn f(xs: List[int]) -> {ht}:\n    xs.push(1)\n    parallel:\n        spawn:\n            sleep_ms(300)\n            t.update(fn(x: int) -> int: t.get() + x)\n    {ret}\n"
                )
            };
            format!(
                "{f}parallel:\n    spawn:\n        {usev}{hold}(f)\n    spawn waiter()\nprint(\"end\")\n"
            )
        }
        Cause::MainBody => {
            format!("parallel:\n    spawn waiter()\n    {usev}{hold}(stuck)\nprint(\"end\")\n")
        }
        Cause::MainDeferInUpdate => {
            format!("parallel:\n    spawn waiter()\n    {usev}{hold}(stuckd)\nprint(\"end\")\n")
        }
        Cause::NurseryJoinInUpdate => format!(
            "parallel:\n    spawn:\n        {usev}{hold}(nest)\n    spawn waiter()\nprint(\"end\")\n"
        ),
        Cause::LockCycle | Cause::SelfReentry | Cause::CancelDeferReentry => {
            unreachable!("returned above")
        }
    };
    head.replace("@IMP@", imp)
        .replace("@CTOR@", ctor)
        .replace("@HT@", ht)
        .replace("@RET@", ret)
        .replace("@WAIT@", wait)
        + &body
}

/// The `Executor` cells: `job` is cancelled 20 ms in, while the waiter waits for the box.
fn executor_cell(job: &str) -> String {
    format!(
        "{job}ex := Executor()\nt := ex.submit(job)\nparallel:\n    spawn:\n        sleep_ms(20)\n        ex.cancel()\n        ex.shutdown()\n        print(\"job\", t.get())\n    spawn waiter()\nprint(\"end\")\n"
    )
}

/// (import, outer method, inner method) of an `int` box: `Shared` update/update, `RwShared`
/// write/write, `RwShared` read/write.
fn int_box(r: Res) -> (&'static str, &'static str, &'static str) {
    match r {
        Res::Shared => ("Shared", "update", "update"),
        Res::RwWrite => ("RwShared", "write", "write"),
        Res::RwRead => ("RwShared", "read", "write"),
    }
}

/// An AB-BA cycle across two boxes, inside `recover:`.
fn lock_cycle(r: Res) -> String {
    let (imp, outer, inner) = int_box(r);
    let (tail, use_outer) = match r {
        Res::RwRead => ("    return 0\n", "_ := "),
        Res::Shared | Res::RwWrite => ("    return x + 1\n", ""),
    };
    String::from(
        "import @IMP@ from std.concurrency
fn inner(x: int, b: @IMP@[int], go: Channel[int], ready: Channel[int]) -> int:
    ready.send(1)
    _ := go.recv()
    b.@INNER@(fn(y: int) -> int: y + 1)
@TAIL@fn take(a: @IMP@[int], b: @IMP@[int], go: Channel[int], ready: Channel[int]) -> int:
    @USE@a.@OUTER@(fn(x: int) -> int: inner(x, b, go, ready))
    return 0
fn main_():
    s1 := @IMP@(0)
    s2 := @IMP@(0)
    r1 := Channel[int](1)
    r2 := Channel[int](1)
    g1 := Channel[int](1)
    g2 := Channel[int](1)
    r := recover:
        parallel:
            spawn take(s1, s2, g1, r1)
            spawn take(s2, s1, g2, r2)
            spawn:
                _ := r1.recv()
                _ := r2.recv()
                g1.send(1)
                g2.send(1)
    print(\"r={r}\")
main_()
",
    )
    .replace("@IMP@", imp)
    .replace("@INNER@", inner)
    .replace("@OUTER@", outer)
    .replace("@TAIL@", tail)
    .replace("@USE@", use_outer)
}

/// The same task re-enters its own guard, inside `recover:`.
fn self_reentry(r: Res) -> String {
    let (imp, _, _) = int_box(r);
    let call = match r {
        Res::Shared => "s.update(fn(x: int) -> int: s.get() + x)",
        Res::RwWrite => "s.write(fn(x: int) -> int: s.get() + x)",
        Res::RwRead => "_ := s.read(fn(x: int) -> int: set_in(x))",
    };
    String::from(
        "import @IMP@ from std.concurrency
s := @IMP@(0)
fn set_in(x: int) -> int:
    s.set(x)
    return x
r := recover:
    @CALL@
print(\"r={r}\")
",
    )
    .replace("@IMP@", imp)
    .replace("@CALL@", call)
}

/// A cancelled task's `defer` re-enters its own guard: the re-lock blocks, and once every task is
/// asleep the one report lists it at its guard wait.
fn cancel_defer_reentry(r: Res) -> String {
    let (imp, _, _) = int_box(r);
    let (inner, call) = match r {
        Res::Shared => ("s.get() + x", "s.update(bump)"),
        Res::RwWrite => ("s.get() + x", "s.write(bump)"),
        Res::RwRead => ("set_in(x)", "s.read(bump)"),
    };
    String::from(
        "import @IMP@ from std.concurrency
import sleep_ms from std.time
s := @IMP@(0)
fn set_in(x: int) -> int:
    s.set(x)
    return x
fn bump(x: int) -> int:
    return @INNER@
r := recover:
    parallel:
        spawn:
            defer @CALL@
            sleep_ms(1000)
        spawn:
            sleep_ms(20)
            panic(\"boom\")
print(\"inner\", r)
print(\"end\")
",
    )
    .replace("@IMP@", imp)
    .replace("@INNER@", inner)
    .replace("@CALL@", call)
}

/// Does the run's exit status and output match `e`?
fn check(e: Expect, rc: Option<i32>, stdout: &str, stderr: &str) -> bool {
    let torn = stdout.contains("C sees [0, 1]") || stdout.contains("after [0, 1]");
    let fatal = rc == Some(1) && stderr.contains(HEADER) && !torn;
    match e {
        Expect::FatalHolder => fatal && stderr.contains("recv() on an empty channel"),
        Expect::Fatal => fatal,
        Expect::FatalGuard => fatal && lists_a_guard_wait(stderr),
        Expect::Kept => rc == Some(0) && stdout.contains("C sees [0, 1]"),
        Expect::Exit => rc == Some(3),
    }
}

#[test]
fn every_cut_cause_releases_the_guard_by_its_cause() {
    for (i, r) in Res::ALL.iter().enumerate() {
        assert_eq!(r.index(), i, "Res::ALL is out of order");
    }
    for (i, c) in Cause::ALL.iter().enumerate() {
        assert_eq!(c.index(), i, "Cause::ALL is out of order");
    }
    let dir = std::env::temp_dir().join(format!("chz-ticket298-grid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let mut bad = Vec::new();
    // `None` = CHEZZI_THREADS unset, the default worker count.
    for t in [Some("1"), Some("2"), None] {
        for r in Res::ALL {
            for c in Cause::ALL {
                let path = dir.join(format!("{r:?}_{c:?}.chz"));
                std::fs::write(&path, prog(r, c)).expect("write fixture");
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
                cmd.arg("run").arg(&path);
                match t {
                    Some(n) => cmd.env("CHEZZI_THREADS", n),
                    None => cmd.env_remove("CHEZZI_THREADS"),
                };
                let out = cmd.output().expect("run chezzi");
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !check(c.expect(), out.status.code(), &stdout, &stderr) {
                    bad.push(format!(
                        "{r:?} x {c:?} T={t:?}: rc={:?} stdout={stdout:?} stderr={:?}",
                        out.status.code(),
                        stderr.lines().next().unwrap_or("")
                    ));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// Where a holder that a fatal deadlock cuts waits: in the closure body, in a `defer` inside the
/// closure, or at the join of a `parallel:` inside the closure.
#[derive(Clone, Copy, Debug)]
enum Hold {
    Body,
    DeferInUpdate,
    NurseryJoin,
}

impl Hold {
    const ALL: [Hold; 3] = [Hold::Body, Hold::DeferInUpdate, Hold::NurseryJoin];

    /// This variant's index in `ALL`; the exhaustive match makes a new variant fail the build.
    fn index(self) -> usize {
        match self {
            Hold::Body => 0,
            Hold::DeferInUpdate => 1,
            Hold::NurseryJoin => 2,
        }
    }

    /// The closure the holder passes to its hold method.
    fn closure(self) -> &'static str {
        match self {
            Hold::Body => "stuck",
            Hold::DeferInUpdate => "stuckd",
            Hold::NurseryJoin => "nest",
        }
    }
}

/// A `chezzi test` file: `holds` deadlocks while it holds the guard, then `after` reads the box.
fn later_test(r: Res, h: Hold) -> String {
    let (ctor, _, hold, _, ht) = r.list_box();
    let ret = if ht == "int" { "return 0" } else { "return xs" };
    let usev = if ht == "int" { "_ := " } else { "" };
    String::from(
        "import Shared, RwShared from std.concurrency
s := @CTOR@
never := Channel[int](0)
fn stuck(xs: List[int]) -> @HT@:
    xs.push(1)
    _ := never.recv()
    xs.push(2)
    @RET@
fn stuckd(xs: List[int]) -> @HT@:
    xs.push(1)
    defer never.recv()
    xs.push(2)
    @RET@
fn nest(xs: List[int]) -> @HT@:
    xs.push(1)
    parallel:
        spawn:
            _ := never.recv()
    xs.push(2)
    @RET@
test fn holds():
    @USE@@HOLD@(@FN@)
test fn after():
    print(\"after\", s.get())
",
    )
    .replace("@CTOR@", ctor)
    .replace("@HT@", ht)
    .replace("@RET@", ret)
    .replace("@USE@", usev)
    .replace("@HOLD@", hold)
    .replace("@FN@", h.closure())
}

/// A holder that `holds`'s per-test verdict drops releases its guard `CUT`, so a later test fn
/// cannot take the box (owner answer 1(a)): `after` blocks at the box and gets its own per-test
/// verdict (Go twin: a `Lock` of a `Mutex` nobody unlocks is fatal, all asleep).
#[test]
fn a_later_test_cannot_take_a_box_a_fatal_holder_left() {
    for (i, h) in Hold::ALL.iter().enumerate() {
        assert_eq!(h.index(), i, "Hold::ALL is out of order");
    }
    let root = std::env::temp_dir().join(format!("chz-ticket298-later-{}", std::process::id()));
    let mut bad = Vec::new();
    // `None` = CHEZZI_THREADS unset, the default worker count.
    for t in [Some("1"), Some("2"), None] {
        for r in Res::ALL {
            for h in Hold::ALL {
                let dir = root.join(format!("{r:?}_{h:?}"));
                std::fs::create_dir_all(&dir).expect("create fixture dir");
                std::fs::write(dir.join("main_test.chz"), later_test(r, h)).expect("write fixture");
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
                cmd.arg("test").arg(&dir);
                match t {
                    Some(n) => cmd.env("CHEZZI_THREADS", n),
                    None => cmd.env_remove("CHEZZI_THREADS"),
                };
                let out = cmd.output().expect("run chezzi");
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                let ok = out.status.code() == Some(1)
                    && text.contains("2 test(s): 0 passed, 0 failed, 2 errored")
                    && text.matches(HEADER).count() == 2
                    && !text.contains("after [");
                if !ok {
                    bad.push(format!(
                        "{r:?} x {h:?} T={t:?}: rc={:?} {:?}",
                        out.status.code(),
                        text.lines().last().unwrap_or("")
                    ));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// One row per `recover:` shape of the `.chz` test fns that used to pin a re-entry or a lock
/// cycle as a recoverable fault: (row name, program, stderr fragment). Each program is that test
/// fn's body with its helper fns, the `recover:` kept and every `assert` removed. A re-lock or a
/// cycle blocks (owner, 2026-11-16), so the fragment is the guard wait the report lists; the old
/// `already holds` and `lock cycle` texts are gone.
fn reentry_rows() -> Vec<(&'static str, String, &'static str)> {
    const SHARED: &str = "import Shared from std.concurrency
fn inc(n: int) -> int:
    return n + 1
fn reenter(s: Shared[int], x: int) -> int:
    s.update(inc)
    return x
fn set_then(s: Shared[int], x: int) -> int:
    s.set(0)
    return x
fn update_then(s: Shared[int], x: int) -> int:
    s.update(fn(y): y + 1)
    return x
fn into_a(a: Shared[int], y: int) -> int:
    a.update(fn(z): z + 1)
    return y
fn a_then_b_then_a(a: Shared[int], b: Shared[int], x: int) -> int:
    b.update(fn(y): into_a(a, y))
    return x
";
    const RW: &str = "import RwShared from std.concurrency
import std.time as time
fn write_inside(r: RwShared[int], x: int) -> int:
    r.write(fn(n): n + 1)
    return x
fn rset_then(s: RwShared[int], x: int) -> int:
    s.set(0)
    return x
fn rwrite_then(s: RwShared[int], x: int) -> int:
    s.write(fn(y): y + 1)
    return x
fn rwrite_from_read(s: RwShared[int], x: int) -> int:
    s.write(fn(y): y + x)
    return x
fn rinto_a(a: RwShared[int], y: int) -> int:
    a.write(fn(z): z + 1)
    return y
fn ra_then_b_then_a(a: RwShared[int], b: RwShared[int], x: int) -> int:
    b.write(fn(y): rinto_a(a, y))
    return x
fn read_again(r: RwShared[int], go: Channel[bool], wsig: Channel[bool], n: int) -> int:
    go.send(true)
    _ := wsig.recv() else: panic(\"wsig closed\")
    time.sleep_ms(30)
    return n + r.read(fn(m): m)
fn writer_signal(r: RwShared[int], go: Channel[bool], wsig: Channel[bool]):
    _ := go.recv() else: panic(\"go closed\")
    wsig.send(true)
    r.write(fn(n): n * 10)
";
    // The box kind the re-lock waits on: every `SHARED` row re-locks a `Shared`, every `RW` row an
    // `RwShared` (a write inside a read and a nested read past a waiting writer included).
    const USE: &str = "waiting for the Shared guard at ";
    const RW_USE: &str = "waiting for the RwShared guard at ";
    let shared = |body: &str| format!("{SHARED}s := Shared(1)\n{body}print(\"r\", r)\n");
    let rw = |body: &str| format!("{RW}s := RwShared(1)\n{body}print(\"r\", r)\n");
    let nested_read = "go := Channel[bool](1)
wsig := Channel[bool](1)
r := recover:
    parallel:
        spawn:
            _ := s.read(fn(n): read_again(s, go, wsig, n))
        spawn writer_signal(s, go, wsig)
";
    vec![
        // tests/chz/spec/shared_handle_test.chz
        (
            "a_same_box_reentry",
            shared("r := recover: s.update(fn(x): reenter(s, x))\n"),
            USE,
        ),
        (
            "a_write_inside_a_read_of_the_same_box",
            rw("r := recover: s.read(fn(x): write_inside(s, x))\n"),
            RW_USE,
        ),
        (
            "a_nested_read_past_a_waiting_writer",
            rw(nested_read),
            RW_USE,
        ),
        // tests/chz/suites/box_grid_test.chz
        (
            "shared_get_inside_its_own_update",
            shared("r := recover: s.update(fn(x): x + s.get())\n"),
            USE,
        ),
        (
            "shared_read_inside_its_own_update",
            shared("r := recover: s.update(fn(x): x + s.read(fn(y): y))\n"),
            USE,
        ),
        (
            "shared_set_inside_its_own_update",
            shared("r := recover: s.update(fn(x): set_then(s, x))\n"),
            USE,
        ),
        (
            "shared_update_inside_its_own_update",
            shared("r := recover: s.update(fn(x): update_then(s, x))\n"),
            USE,
        ),
        (
            "shared_get_inside_its_own_read",
            shared("r: int! = recover: s.read(fn(x): x + s.get())\n"),
            USE,
        ),
        (
            "rwshared_get_inside_its_own_write",
            rw("r := recover: s.write(fn(x): x + s.get())\n"),
            RW_USE,
        ),
        (
            "rwshared_read_inside_its_own_write",
            rw("r := recover: s.write(fn(x): x + s.read(fn(y): y))\n"),
            RW_USE,
        ),
        (
            "rwshared_set_inside_its_own_write",
            rw("r := recover: s.write(fn(x): rset_then(s, x))\n"),
            RW_USE,
        ),
        (
            "rwshared_write_inside_its_own_write",
            rw("r := recover: s.write(fn(x): rwrite_then(s, x))\n"),
            RW_USE,
        ),
        (
            "rwshared_write_nested_in_read",
            rw("r := recover: s.read(fn(x): rwrite_from_read(s, x))\n"),
            RW_USE,
        ),
        // tests/chz/suites/box_rules_test.chz
        (
            "ab_ba_cycle_in_one_task",
            shared("b := Shared(2)\nr := recover: s.update(fn(x): a_then_b_then_a(s, b, x))\n"),
            USE,
        ),
        (
            "rwshared_ab_ba_cycle_in_one_task",
            rw("b := RwShared(2)\nr := recover: s.write(fn(x): ra_then_b_then_a(s, b, x))\n"),
            RW_USE,
        ),
    ]
}

/// A re-entry, a write inside a read and a lock cycle block, and the run ends with the one fatal
/// report: no `recover:` catches it (owner, 2026-11-16; Go twin: a re-`Lock` is fatal, all asleep).
#[test]
fn a_recovered_reentry_or_lock_cycle_is_fatal() {
    let dir = std::env::temp_dir().join(format!("chz-ticket298-reentry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let mut bad = Vec::new();
    // `None` = CHEZZI_THREADS unset, the default worker count.
    for t in [Some("1"), None] {
        for (name, program, fragment) in reentry_rows() {
            let path = dir.join(format!("{name}.chz"));
            std::fs::write(&path, program).expect("write fixture");
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
            cmd.arg("run").arg(&path);
            match t {
                Some(n) => cmd.env("CHEZZI_THREADS", n),
                None => cmd.env_remove("CHEZZI_THREADS"),
            };
            let out = cmd.output().expect("run chezzi");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if out.status.code() != Some(1)
                || !stderr.contains(HEADER)
                || !stderr.contains(fragment)
                || stdout.contains("r ")
            {
                bad.push(format!(
                    "{name} T={t:?}: rc={:?} stdout={stdout:?} stderr={:?}",
                    out.status.code(),
                    stderr.lines().next().unwrap_or("")
                ));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        bad.is_empty(),
        "{} row(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// How main joins an `Executor` job whose body re-enters its own guard.
#[derive(Clone, Copy, Debug)]
enum Join {
    Get,
    RecoveredGet,
    Shutdown,
}

impl Join {
    const ALL: [Join; 3] = [Join::Get, Join::RecoveredGet, Join::Shutdown];

    /// This variant's index in `ALL`; the exhaustive match makes a new variant fail the build.
    fn index(self) -> usize {
        match self {
            Join::Get => 0,
            Join::RecoveredGet => 1,
            Join::Shutdown => 2,
        }
    }

    fn tail(self) -> &'static str {
        match self {
            Join::Get => "_ := t.get()\n",
            Join::RecoveredGet => "r := recover:\n    _ := t.get()\nprint(\"r\", r)\n",
            Join::Shutdown => "ex.shutdown()\n",
        }
    }
}

/// An `Executor` job that re-enters its own guard blocks; main blocks at its join (`t.get()` or
/// `shutdown()`), so every task is asleep and the one fatal report lists main at its Executor join
/// and the job at its guard wait, however main joins it (Go twin: fatal, all asleep).
#[test]
fn an_executor_job_reentry_is_fatal_whichever_way_it_is_joined() {
    for (i, j) in Join::ALL.iter().enumerate() {
        assert_eq!(j.index(), i, "Join::ALL is out of order");
    }
    let dir = std::env::temp_dir().join(format!("chz-ticket298-job-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let mut bad = Vec::new();
    // `None` = CHEZZI_THREADS unset, the default worker count.
    for t in [Some("1"), Some("2"), None] {
        for r in Res::ALL {
            for j in Join::ALL {
                let (imp, _, _) = int_box(r);
                let call = match r {
                    Res::Shared => "s.update(fn(x: int) -> int: s.get() + x)",
                    Res::RwWrite => "s.write(fn(x: int) -> int: s.get() + x)",
                    Res::RwRead => "_ := s.read(fn(x: int) -> int: set_in(x))",
                };
                let program = format!(
                    "import {imp}, Executor from std.concurrency\ns := {imp}(0)\nfn set_in(x: int) -> int:\n    s.set(x)\n    return x\nfn job() -> int:\n    {call}\n    return 1\nex := Executor()\nt := ex.submit(job)\n{}",
                    j.tail()
                );
                let path = dir.join(format!("{r:?}_{j:?}.chz"));
                std::fs::write(&path, program).expect("write fixture");
                let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
                cmd.arg("run").arg(&path);
                match t {
                    Some(n) => cmd.env("CHEZZI_THREADS", n),
                    None => cmd.env_remove("CHEZZI_THREADS"),
                };
                let out = cmd.output().expect("run chezzi");
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if out.status.code() != Some(1)
                    || !stderr.contains(HEADER)
                    || !stderr.contains("task 1: waiting for an Executor join at ")
                    || !lists_a_guard_wait(&stderr)
                    || stderr.contains("recv() on an empty channel")
                    || stdout.contains("r ")
                {
                    bad.push(format!(
                        "{r:?} x {j:?} T={t:?}: rc={:?} stdout={stdout:?} stderr={:?}",
                        out.status.code(),
                        stderr.lines().next().unwrap_or("")
                    ));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
