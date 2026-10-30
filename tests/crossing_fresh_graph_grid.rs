#![allow(clippy::wildcard_enum_match_arm)]
//! TICKET-240 (wave 22 Family D): the grid is wrapper path x inner value x write depth.
//! Every cell does its write inside `recover:` in the task and sends `runs` or `faults`; the parent
//! prints its own named values after the join. M3.4 (Go model): a spawn, a `spawn:` capture and an
//! Executor job hold the parent's values by handle, so every cell runs and a write to a value the
//! parent can reach lands in the parent's view (Go, a goroutine writes through a pointer: the
//! parent sees the write). Each cell writes its own `named{n}` / `nn{n}`: one value written by two
//! tasks would be a race (the user's bug). Each program runs in a fresh process at
//! `CHEZZI_THREADS` 1, 2 and default. A `.chz` test cannot run one program at three worker counts
//! or read a compile error, so this runs the built binary.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(60);
const THREADS: [Option<&str>; 3] = [Some("1"), Some("2"), None];
const L: &str = "List[List[List[int]]]";

/// Runs `src` at `threads` workers (`None` = the default count): `(exit code, stdout, stderr)`.
fn run(name: &str, src: &str, threads: Option<&str>) -> (i32, String, String) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("chz-t240-{name}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match threads {
        Some(t) => cmd.env("CHEZZI_THREADS", t),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    let mut child = cmd.spawn().expect("spawn chezzi");
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break s;
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{name} at {threads:?}: still running after {LIMIT:?}\n{src}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut out = String::new();
    let mut err = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).ok();
    child.stderr.take().unwrap().read_to_string(&mut err).ok();
    let _ = std::fs::remove_dir_all(&dir);
    (status.code().unwrap_or(-1), out, err)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Inner {
    Lit,
    Comp,
    Struct,
    Named,
    Call,
    Copy,
    StructCopy,
}
use Inner::*;
const INNERS: [Inner; 7] = [Lit, Comp, Struct, Named, Call, Copy, StructCopy];

impl Inner {
    /// The operand expression and whether its type is the struct `N` (else the list `L`).
    fn expr(self) -> (&'static str, bool) {
        match self {
            Lit => ("[[[0]]]", false),
            Comp => ("[[[0]] for _i in 0..1]", false),
            Struct => ("N([[0]])", true),
            Named => ("named", false),
            Call => ("mk_l()", false),
            Copy => ("named.copy()", false),
            StructCopy => ("nn.copy()", true),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wrapper {
    Plain,
    ImplicitOpt,
    ExplicitOpt,
    Result,
    NestedOpt,
    StructField,
    NestedStruct,
    ListElem,
    MapValue,
    TupleElem,
    Variant,
    InlineDefault,
    Pack,
    Receiver,
    GenFrame,
    Submit,
    SubmitResult,
    SubmitTask,
    ChannelSend,
}
use Wrapper::*;
const WRAPPERS: [Wrapper; 19] = [
    Plain,
    ImplicitOpt,
    ExplicitOpt,
    Result,
    NestedOpt,
    StructField,
    NestedStruct,
    ListElem,
    MapValue,
    TupleElem,
    Variant,
    InlineDefault,
    Pack,
    Receiver,
    GenFrame,
    Submit,
    SubmitResult,
    SubmitTask,
    ChannelSend,
];

impl Wrapper {
    /// A hand-off wrapper marks nothing the task builds or receives: an Executor job builds its
    /// argument inside the job, and a channel send hands over the value itself (M3.2).
    fn handoff(self) -> bool {
        matches!(self, Submit | SubmitResult | SubmitTask | ChannelSend)
    }

    /// Whether the cell can be written at all. A default is a literal or a provider call, never a
    /// name of the caller.
    fn expressible(self, i: Inner) -> bool {
        self != InlineDefault || matches!(i, Lit | Call)
    }

    /// The task-side declarations, one set per inner type: `@T` is the type, `@S` its suffix.
    fn decls(self) -> &'static str {
        match self {
            Plain | Submit | SubmitResult | SubmitTask => "",
            ImplicitOpt | ExplicitOpt => {
                "fn w_@S(r: Channel[str], tag: str, d: int, x: @T?):
    match x:
        ?v: poke_@S(r, tag, d, v)
        None: r.send(tag + \" none\")
"
            }
            Result => {
                "fn w_@S(r: Channel[str], tag: str, d: int, x: @T!str):
    match x:
        ?v: poke_@S(r, tag, d, v)
        !_: r.send(tag + \" err\")
"
            }
            NestedOpt => {
                "fn w_@S(r: Channel[str], tag: str, d: int, x: @T??):
    match x:
        ??v: poke_@S(r, tag, d, v)
        _: r.send(tag + \" none\")
"
            }
            StructField => {
                "struct Box_@S:
    v: @T
fn w_@S(r: Channel[str], tag: str, d: int, x: Box_@S):
    poke_@S(r, tag, d, x.v)
"
            }
            NestedStruct => {
                "struct Box_@S:
    v: @T
struct Outer_@S:
    b: Box_@S
fn w_@S(r: Channel[str], tag: str, d: int, x: Outer_@S):
    poke_@S(r, tag, d, x.b.v)
"
            }
            ListElem => {
                "fn w_@S(r: Channel[str], tag: str, d: int, x: List[@T]):
    poke_@S(r, tag, d, x[0])
"
            }
            MapValue => {
                "fn w_@S(r: Channel[str], tag: str, d: int, x: Map[str, @T]):
    poke_@S(r, tag, d, x[\"a\"])
"
            }
            TupleElem => {
                "fn w_@S(r: Channel[str], tag: str, d: int, x: (@T, int)):
    poke_@S(r, tag, d, x.0)
"
            }
            Variant => {
                "enum E_@S:
    V(@T)
fn w_@S(r: Channel[str], tag: str, d: int, x: E_@S):
    match x:
        E_@S.V(v): poke_@S(r, tag, d, v)
"
            }
            InlineDefault => {
                "fn w_lit(r: Channel[str], tag: str, d: int, x: List[List[List[int]]]? = [[[0]]]):
    match x:
        ?v: poke_l(r, tag, d, v)
        None: r.send(tag + \" none\")
fn w_call(r: Channel[str], tag: str, d: int, x: List[List[List[int]]]? = mk_l()):
    match x:
        ?v: poke_l(r, tag, d, v)
        None: r.send(tag + \" none\")
"
            }
            Pack => {
                "fn w_@S(r: Channel[str], tag: str, d: int, ...xs: @T):
    poke_@S(r, tag, d, xs[0])
"
            }
            Receiver => {
                "struct Box_@S:
    v: @T
    fn go(self, r: Channel[str], tag: str, d: int):
        poke_@S(r, tag, d, self.v)
"
            }
            GenFrame => "",
            ChannelSend => {
                "fn w_@S(r: Channel[str], tag: str, d: int, ch: Channel[@T]):
    poke_@S(r, tag, d, ch.recv())
"
            }
        }
    }

    /// The statement that crosses the value: `@S` suffix, `@G` tag, `@D` depth, `@I` operand.
    fn cross(self) -> &'static str {
        match self {
            Plain => "spawn poke_@S(r, \"@G\", @D, @I)",
            ImplicitOpt | Result => "spawn w_@S(r, \"@G\", @D, @I)",
            ExplicitOpt => "spawn w_@S(r, \"@G\", @D, ?@I)",
            NestedOpt => "spawn w_@S(r, \"@G\", @D, ??@I)",
            StructField => "spawn w_@S(r, \"@G\", @D, Box_@S(@I))",
            NestedStruct => "spawn w_@S(r, \"@G\", @D, Outer_@S(Box_@S(@I)))",
            ListElem => "spawn w_@S(r, \"@G\", @D, [@I])",
            MapValue => "spawn w_@S(r, \"@G\", @D, {\"a\": @I})",
            TupleElem => "spawn w_@S(r, \"@G\", @D, (@I, 1))",
            Variant => "spawn w_@S(r, \"@G\", @D, E_@S.V(@I))",
            InlineDefault => unreachable!("built by `cell`"),
            Pack => "spawn w_@S(r, \"@G\", @D, @I, @I)",
            Receiver => "spawn Box_@S(@I).go(r, \"@G\", @D)",
            GenFrame => unreachable!("built by `cell`"),
            Submit => "_ := ex.submit(fn(): poke_@S(r, \"@G\", @D, @I))",
            SubmitResult => "_ := ex.submit(fn(): poke_@S(r, \"@G\", @D, @I))",
            SubmitTask => "_ := ex.submit(fn(): poke_@S(r, \"@G\", @D, @I))",
            ChannelSend => "spawn w_@S(r, \"@G\", @D, c@N)",
        }
    }
}

/// The verdict of one cell: M3.4, every wrapper passes its operand by handle, so every write runs.
fn expect(_w: Wrapper, _i: Inner, _d: usize) -> &'static str {
    "runs"
}

const PRELUDE: &str = "import std.concurrency\nimport Iterator from std.iter\nimport Executor from std.concurrency

struct N:
    kids: List[List[int]]
fn verdict(r: Channel[str], tag: str, res: None!):
    match res:
        ?_: r.send(tag + \" runs\")
        !e:
            if e.message().contains(\"this value is this task's copy: a write to it would be lost at the join\"):
                r.send(tag + \" faults\")
            else:
                r.send(tag + \" other \" + e.message())
fn poke_l(r: Channel[str], tag: str, d: int, v: List[List[List[int]]]):
    res := recover:
        if d == 0:
            v.push([[1]])
        elif d == 1:
            v[0].push([1])
        else:
            v[0][0].push(1)
    verdict(r, tag, res)
fn poke_n(r: Channel[str], tag: str, d: int, v: N):
    res := recover:
        if d == 0:
            v.kids = [[9]]
        elif d == 1:
            v.kids.push([1])
        else:
            v.kids[0].push(1)
    verdict(r, tag, res)
fn mk_l() -> List[List[List[int]]]:
    return [[[0]]]
fn drive(r: Channel[str], tag: str, g: Iterator[int]):
    res := recover:
        for _v in g:
            pass
    verdict(r, tag, res)
";

const WRITE_L: &str = "    yield 0
    if d == 0:
        @V.push([[1]])
    elif d == 1:
        @V[0].push([1])
    else:
        @V[0][0].push(1)
    yield 1
";
const WRITE_N: &str = "    yield 0
    if d == 0:
        @V.kids = [[9]]
    elif d == 1:
        @V.kids.push([1])
    else:
        @V.kids[0].push(1)
    yield 1
";

/// One generator per inner value: a frame-local built by the generator itself, or (for a named
/// value) a param the creating call fills. The generator starts before the spawn.
fn gen_decls() -> String {
    let mut s = String::new();
    for (name, head, v, is_n) in [
        (
            "lit",
            "(d: int) -> Iterator[int]:\n    buf: List[List[List[int]]] = [[[0]]]\n",
            "buf",
            false,
        ),
        (
            "comp",
            "(d: int) -> Iterator[int]:\n    buf: List[List[List[int]]] = [[[0]] for _i in 0..1]\n",
            "buf",
            false,
        ),
        (
            "struct",
            "(d: int) -> Iterator[int]:\n    buf: N = N([[0]])\n",
            "buf",
            true,
        ),
        (
            "named",
            "(d: int, p: List[List[List[int]]]) -> Iterator[int]:\n",
            "p",
            false,
        ),
        (
            "call",
            "(d: int) -> Iterator[int]:\n    buf: List[List[List[int]]] = mk_l()\n",
            "buf",
            false,
        ),
        (
            "copy",
            "(d: int, p: List[List[List[int]]]) -> Iterator[int]:\n    buf: List[List[List[int]]] = p.copy()\n",
            "buf",
            false,
        ),
        (
            "scopy",
            "(d: int, p: N) -> Iterator[int]:\n    buf: N = p.copy()\n",
            "buf",
            true,
        ),
    ] {
        s += &format!("fn gen_{name}{head}");
        s += &(if is_n { WRITE_N } else { WRITE_L }).replace("@V", v);
    }
    s
}

/// The lines of one cell: `(before the parallel block, inside it, after it)`.
fn cell(w: Wrapper, i: Inner, d: usize, n: usize) -> (String, String, String) {
    let tag = format!("{i:?} {d}");
    let (expr, is_n) = i.expr();
    let sfx = if is_n { "n" } else { "l" };
    // The cell's own named values: no two tasks write one value.
    let own = expr
        .replace("named", &format!("named{n}"))
        .replace("nn", &format!("nn{n}"));
    let own_decls = format!("named{n}: {L} = [[[0]]]\n    nn{n} := N([[0]])");
    let fill = |t: &str| {
        t.replace("@S", sfx)
            .replace("@G", &tag)
            .replace("@D", &d.to_string())
            .replace("@I", &own)
            .replace("@N", &n.to_string())
    };
    match w {
        InlineDefault => {
            let f = if i == Lit { "w_lit" } else { "w_call" };
            (
                String::new(),
                format!("spawn {f}(r, \"{tag}\", {d})"),
                String::new(),
            )
        }
        GenFrame => {
            let call = match i {
                Lit => format!("gen_lit({d})"),
                Comp => format!("gen_comp({d})"),
                Struct => format!("gen_struct({d})"),
                Named => format!("gen_named({d}, named{n})"),
                Call => format!("gen_call({d})"),
                Copy => format!("gen_copy({d}, named{n})"),
                StructCopy => format!("gen_scopy({d}, nn{n})"),
            };
            (
                format!("{own_decls}\n    g{n} := {call}\n    g{n}.next()"),
                format!("spawn drive(r, \"{tag}\", g{n})"),
                String::new(),
            )
        }
        // M3.2: a channel hands over the value itself, so a named operand is the parent's own.
        ChannelSend => {
            let ty = if is_n { "N" } else { L };
            (
                format!("{own_decls}\n    c{n} := Channel[{ty}](1)\n    c{n}.send({own})"),
                fill(w.cross()),
                String::new(),
            )
        }
        Submit | SubmitResult | SubmitTask => (own_decls, String::new(), fill(w.cross())),
        _ => (own_decls, fill(w.cross()), String::new()),
    }
}

/// One program for `w` holding `cells`, and the stdout it must print.
fn program(w: Wrapper, cells: &[(Inner, usize)]) -> (String, String) {
    let mut src = String::from(PRELUDE);
    if w == GenFrame {
        src += &gen_decls();
    } else if w == InlineDefault {
        src += w.decls();
    } else {
        src += &w.decls().replace("@S", "l").replace("@T", L);
        src += &w.decls().replace("@S", "n").replace("@T", "N");
    }
    let (mut pre, mut par, mut post) = (String::new(), String::new(), String::new());
    let mut want: Vec<String> = Vec::new();
    for (n, &(i, d)) in cells.iter().enumerate() {
        let (a, b, c) = cell(w, i, d, n);
        for (buf, line, indent) in [
            (&mut pre, a, "    "),
            (&mut par, b, "        "),
            (&mut post, c, "    "),
        ] {
            if !line.is_empty() {
                *buf += &format!("{indent}{line}\n");
            }
        }
        want.push(format!("{i:?} {d} {}", expect(w, i, d)));
    }
    src += "fn main():\n    named: List[List[List[int]]] = [[[0]]]\n    nn := N([[0]])\n";
    src += "    r := Channel[str](64)\n    ex := Executor(2)\n";
    src += &pre;
    if !par.is_empty() {
        src += "    parallel:\n";
        src += &par;
    }
    src += &post;
    src += "    ex.shutdown()\n    out: List[str] = []\n";
    src += &format!(
        "    for _i in 0..{}:\n        out.push(r.recv())\n",
        cells.len()
    );
    src += "    out.sort()\n    for s in out:\n        print(s)\n    print(\"parent\", named, nn)\nmain()\n";
    want.sort();
    want.push("parent [[[0]]] N(kids=[[0]])".to_string());
    (src, want.join("\n") + "\n")
}

/// Runs every wrapper's program over the cells `keep` selects, at each worker count, and reports
/// every cell that differs.
fn check_grid(keep: impl Fn(Wrapper, Inner, usize) -> bool) {
    let mut bad = Vec::new();
    let mut ran = 0;
    for w in WRAPPERS {
        let cells: Vec<(Inner, usize)> = INNERS
            .iter()
            .flat_map(|&i| (0..3).map(move |d| (i, d)))
            .filter(|&(i, d)| w.expressible(i) && keep(w, i, d))
            .collect();
        if cells.is_empty() {
            continue;
        }
        ran += cells.len();
        let (src, want) = program(w, &cells);
        for t in THREADS {
            let (code, out, err) = run(&format!("{w:?}"), &src, t);
            if code != 0 || out != want {
                let got: Vec<&str> = out
                    .lines()
                    .filter(|l| !want.contains(&format!("{l}\n")))
                    .collect();
                bad.push(format!(
                    "{w:?} at CHEZZI_THREADS={t:?}: rc {code}, unexpected lines {got:?}\nstderr: {}",
                    err.lines().filter(|l| l.contains("error")).collect::<Vec<_>>().join(" | ")
                ));
            }
        }
    }
    assert!(ran > 0, "the selection is empty");
    assert!(
        bad.is_empty(),
        "{} program run(s) differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// Every cell of every spawn wrapper runs (M3.4: the task holds the operand by handle). A
/// container `.copy()` is a fresh root (its children stay the parent's, DEC-160).
#[test]
fn fresh_graph_cells_run() {
    check_grid(|w, _, _| !w.handoff());
    // The fresh sibling of a named value is writable, at either position (the named side is the
    // `mixed-siblings` cell of `reachable_cells_land`).
    let src = format!(
        "{PRELUDE}fn w2(r: Channel[str], tag: str, x: List[List[int]], k: int):
    res := recover:
        x[k].push(7)
    verdict(r, tag, res)
fn wtup(r: Channel[str], tag: str, t: (List[int], List[int])):
    res := recover:
        t.1.push(7)
    verdict(r, tag, res)
fn main():
    xs := [0]
    r := Channel[str](4)
    parallel:
        spawn w2(r, \"list last\", [xs, []], 1)
        spawn w2(r, \"list first\", [[], xs], 0)
        spawn wtup(r, \"tuple\", (xs, []))
    out := [r.recv(), r.recv(), r.recv()]
    out.sort()
    print(out, xs)
main()
"
    );
    for t in THREADS {
        let (code, out, err) = run("fresh-sibling", &src, t);
        assert_eq!(
            (code, out.as_str()),
            (
                0,
                "['list first runs', 'list last runs', 'tuple runs'] [0]\n"
            ),
            "at {t:?}: {err}"
        );
    }
}

/// Every cell of every hand-off wrapper (an Executor job, a channel send) runs too.
#[test]
fn controls_hold() {
    check_grid(|w, _, _| w.handoff());
    // A struct `.copy()` receiver is shallow: its `v` is the parent's list, so each task's write
    // lands in the parent's Box (one Box per task: no two tasks write one value). Go, a copied
    // struct holding a slice pointer: `Box(v=[[[0]], [[1]]]) Box(v=[[[0], [1]]]) Box(v=[[[0, 1]]])`.
    let src = format!(
        "{PRELUDE}struct Box:
    v: List[List[List[int]]]
    fn go(self, r: Channel[str], tag: str, d: int):
        poke_l(r, tag, d, self.v)
fn main():
    b0 := Box([[[0]]])
    b1 := Box([[[0]]])
    b2 := Box([[[0]]])
    r := Channel[str](4)
    parallel:
        spawn b0.copy().go(r, \"d0\", 0)
        spawn b1.copy().go(r, \"d1\", 1)
        spawn b2.copy().go(r, \"d2\", 2)
    out := [r.recv(), r.recv(), r.recv()]
    out.sort()
    print(out, b0, b1, b2)
main()
"
    );
    for t in THREADS {
        let (code, out, err) = run("struct-copy-recv", &src, t);
        assert_eq!(
            (code, out.as_str()),
            (
                0,
                "['d0 runs', 'd1 runs', 'd2 runs'] Box(v=[[[0]], [[1]]]) Box(v=[[[0], [1]]]) Box(v=[[[0, 1]]])\n"
            ),
            "at {t:?}: {err}"
        );
    }
}

const KEY: &str = "struct Key:
    id: int
    log: List[int]

    fn hash(self) -> int:
        return self.id

    fn eq(self, other: Key) -> bool:
        return self.id == other.id
fn w(r: Channel[str], m: Map[Key, List[int]]):
    res := recover:
        for key in m.keys():
            key.log.push(1)
    verdict(r, \"key\", res)
";

/// Every cell where the parent CAN still reach the object: `(name, program body, stdout)`. Each
/// prints the task's verdict and then the parent's own view after the join. M3.4: the task holds
/// the object by handle, so the write runs and the parent sees it (CPython `m34py/reachable.py`, a
/// thread: the lists below). A key read back through `keys()` is the map's own key, and a map stores
/// its key by value with no task at all (BASE prints `[]` for `k.log` after `{k: [0]}` and a
/// `keys()` write), so those three key rows print `[]`; a write through the param itself lands.
fn reachable_cells() -> Vec<(&'static str, String, &'static str)> {
    let lists = "fn w(r: Channel[str], x: List[List[int]]):
    res := recover:
        x[0].push(7)
    verdict(r, \"w\", res)
";
    vec![
        (
            "gen-yields-child",
            "fn gen() -> Iterator[List[int]]:
    buf := [[0]]
    yield buf[0]
    buf[0].push(1)
    yield buf[0]
fn drain(r: Channel[str], g: Iterator[List[int]]):
    res := recover:
        for _v in g:
            pass
    verdict(r, \"gen\", res)
fn main():
    g := gen()
    held := g.next()
    r := Channel[str](1)
    parallel:
        spawn drain(r, g)
    print(r.recv(), held)
main()
"
            .to_string(),
            "gen runs [0, 1]\n",
        ),
        (
            "gen-pushes-param",
            "fn gen(p: List[int]) -> Iterator[int]:
    buf := [[0]]
    buf.push(p)
    yield 0
    buf[1].push(1)
    yield 1
fn main():
    named := [0]
    g := gen(named)
    g.next()
    r := Channel[str](1)
    parallel:
        spawn drive(r, \"gen\", g)
    print(r.recv(), named)
main()
"
            .to_string(),
            "gen runs [0, 1]\n",
        ),
        (
            "gen-yields-match-binder",
            "fn gen() -> Iterator[List[int]]:
    buf: List[int]? = []
    match buf:
        ?a:
            yield a
            a.push(1)
            yield a
        None: pass
fn drain(r: Channel[str], g: Iterator[List[int]]):
    res := recover:
        for _v in g:
            pass
    verdict(r, \"gen\", res)
fn main():
    g := gen()
    held := g.next()
    r := Channel[str](1)
    parallel:
        spawn drain(r, g)
    print(r.recv(), held)
main()
"
            .to_string(),
            "gen runs [1]\n",
        ),
        (
            "gen-reverse-moves-param",
            "fn gen(p: List[int]) -> Iterator[int]:
    buf := [p, []]
    buf.reverse()
    yield 0
    buf[1].push(1)
    yield 1
fn main():
    named := [0]
    g := gen(named)
    g.next()
    r := Channel[str](1)
    parallel:
        spawn drive(r, \"gen\", g)
    print(r.recv(), named)
main()
"
            .to_string(),
            "gen runs [0, 1]\n",
        ),
        (
            "struct-named-arg-reordered",
            "struct S:
    a: List[int]
    b: List[int]
fn w(r: Channel[str], s: S):
    res := recover:
        s.a.push(7)
    verdict(r, \"w\", res)
fn main():
    named := [0]
    r := Channel[str](1)
    parallel:
        spawn w(r, S(b=[], a=named))
    print(r.recv(), named)
main()
"
            .to_string(),
            "w runs [0, 7]\n",
        ),
        (
            "map-duplicate-key",
            "fn w(r: Channel[str], m: Map[str, List[int]]):
    res := recover:
        m[\"a\"].push(7)
    verdict(r, \"w\", res)
fn main():
    named := [0]
    r := Channel[str](1)
    parallel:
        spawn w(r, {\"a\": [], \"a\": named})
    print(r.recv(), named)
main()
"
            .to_string(),
            "w runs [0, 7]\n",
        ),
        (
            "named-in-fresh-list",
            format!(
                "{lists}fn main():
    named := [[0]]
    r := Channel[str](2)
    parallel:
        spawn w(r, [named[0]])
    print(r.recv(), named)
    xs := [0]
    parallel:
        spawn w(r, [xs])
    print(r.recv(), xs)
main()
"
            ),
            "w runs [[0, 7]]\nw runs [0, 7]\n",
        ),
        // A named value beside a fresh sibling, at its position: the write lands on the named
        // value. Each task writes its own list (no race). The fresh side of the same line is in
        // `fresh_graph_cells_run`.
        (
            "mixed-siblings",
            r#"fn w2(r: Channel[str], tag: str, x: List[List[int]], k: int):
    res := recover:
        x[k].push(7)
    verdict(r, tag, res)
fn wopt(r: Channel[str], tag: str, x: List[int]?):
    match x:
        ?v:
            res := recover:
                v.push(7)
            verdict(r, tag, res)
        None: r.send(tag + " none")
fn wmap(r: Channel[str], tag: str, m: Map[str, List[int]], k: str):
    res := recover:
        m[k].push(7)
    verdict(r, tag, res)
fn wtup(r: Channel[str], tag: str, t: (List[int], List[int]), k: int):
    res := recover:
        if k == 0:
            t.0.push(7)
        else:
            t.1.push(7)
    verdict(r, tag, res)
fn main():
    xa := [0]
    xc := [0]
    xd := [0]
    opt: List[int]? = [0]
    xf := [0]
    xg := [0]
    xh := [0]
    xi := [0]
    xk := [0]
    r := Channel[str](32)
    parallel:
        spawn w2(r, "a named-first 0", [xa, []], 0)
        spawn w2(r, "c comp-named 0", [xc for _i in 0..2], 0)
        spawn w2(r, "d alias-twice 1", [xd, xd], 1)
        spawn wopt(r, "e named-opt", opt)
        spawn wopt(r, "f wrap-named", xf)
        spawn wmap(r, "g map-mixed a", {"a": xg, "b": []}, "a")
        spawn wmap(r, "h map-mixed b", {"a": xh, "b": []}, "b")
        spawn wtup(r, "i tuple 0", (xi, []), 0)
        spawn w2(r, "k copy-child 0", [xk].copy(), 0)
    out: List[str] = []
    for _i in 0..9:
        out.push(r.recv())
    out.sort()
    for s in out:
        print(s)
    print(xa, xc, xd, opt, xf, xg, xh, xi, xk)
main()
"#
            .to_string(),
            "a named-first 0 runs
c comp-named 0 runs
d alias-twice 1 runs
e named-opt runs
f wrap-named runs
g map-mixed a runs
h map-mixed b runs
i tuple 0 runs
k copy-child 0 runs
[0, 7] [0, 7] [0, 7] [0, 7] [0, 7] [0, 7] [0] [0, 7] [0, 7]
",
        ),
        (
            "key-literal",
            format!(
                "{KEY}fn main():
    k := Key(1, [])
    r := Channel[str](1)
    parallel:
        spawn w(r, {{k: []}})
    print(r.recv(), k.log)
main()
"
            ),
            "key runs []\n",
        ),
        (
            "key-comprehension",
            format!(
                "{KEY}fn main():
    k := Key(1, [])
    ks := [k]
    r := Channel[str](1)
    parallel:
        spawn w(r, {{x: [] for x in ks}})
    print(r.recv(), k.log)
main()
"
            ),
            "key runs []\n",
        ),
        (
            "key-gen-store-then-param-write",
            format!(
                "{KEY}fn gen(p: Key) -> Iterator[int]:
    buf: Map[Key, List[int]] = {{}}
    buf[p] = []
    yield 1
    p.log.push(1)
    yield 2
fn main():
    k := Key(1, [])
    g := gen(k)
    g.next()
    r := Channel[str](1)
    parallel:
        spawn drive(r, \"key\", g)
    print(r.recv(), k.log)
main()
"
            ),
            "key runs [1]\n",
        ),
        (
            "key-gen-store-then-keys-write",
            format!(
                "{KEY}fn gen(p: Key) -> Iterator[int]:
    buf: Map[Key, List[int]] = {{}}
    buf[p] = []
    yield 1
    for key in buf.keys():
        key.log.push(1)
    yield 2
fn main():
    k := Key(1, [])
    g := gen(k)
    g.next()
    r := Channel[str](1)
    parallel:
        spawn drive(r, \"key\", g)
    print(r.recv(), k.log)
main()
"
            ),
            "key runs []\n",
        ),
    ]
}

/// Where the parent can reach the object, the task's write lands and the parent sees it (M3.4).
/// A spawned fn's unconditional write to a named operand compiles and lands too (layer A is gone;
/// Go `go w(named)` prints `[0 7]`).
#[test]
fn reachable_cells_land() {
    let mut bad = Vec::new();
    for (name, body, want) in reachable_cells() {
        let src = format!("{PRELUDE}{body}");
        for t in THREADS {
            let (code, out, err) = run(name, &src, t);
            if code != 0 || out != want {
                bad.push(format!(
                    "{name} at CHEZZI_THREADS={t:?}: rc {code}, stdout {out:?}\n{err}"
                ));
            }
        }
    }
    let src = "fn w(xs: List[int]):
    xs.push(7)
fn main():
    named := [0]
    parallel:
        spawn w(named)
    print(named)
main()
";
    for t in THREADS {
        let (code, out, err) = run("layer-a", src, t);
        if code != 0 || out != "[0, 7]\n" {
            bad.push(format!(
                "layer-a at CHEZZI_THREADS={t:?}: rc {code}, stdout {out:?}\n{err}"
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} cell run(s) differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
