//! TICKET-253 — a range is a value. Grid: form x use x position, each cell at `chezzi check` and
//! `chezzi run` (the spawn and channel uses at `CHEZZI_THREADS` 1, 2 and unset), plus the slice table.
//!
//! The expected output of every cell comes from `Model`, a small reference of the rules in the
//! ticket's Decisions (CPython `range` and slice semantics for steps and negative bounds, Rust for the
//! literal forms). A cell that Chezzi runs differently from the model fails with both texts.

use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy)]
struct Model {
    start: Option<i64>,
    end: Option<i64>,
    step: i64,
    incl: bool,
}

impl Model {
    fn in_end(&self, cur: i64, e: i64) -> bool {
        (self.incl && cur == e) || if self.step > 0 { cur < e } else { cur > e }
    }

    /// The first `limit` values, or the fault text of iterating a range with no start.
    fn head(&self, limit: usize) -> Result<Vec<i64>, String> {
        let Some(s) = self.start else {
            return Err("a range with no start cannot be iterated".into());
        };
        let mut out = Vec::new();
        let mut cur = s;
        while out.len() < limit && self.end.is_none_or(|e| self.in_end(cur, e)) {
            out.push(cur);
            cur += self.step;
        }
        Ok(out)
    }

    fn len(&self) -> Result<usize, String> {
        match (self.start, self.end) {
            (Some(_), Some(_)) => Ok(self.head(usize::MAX / 2).unwrap().len()),
            _ => Err("len() needs a range with a start and an end".into()),
        }
    }

    fn contains(&self, x: i64) -> bool {
        if let Some(s) = self.start {
            if (self.step > 0 && x < s) || (self.step < 0 && x > s) {
                return false;
            }
            if self.step.abs() != 1 && (x - s) % self.step != 0 {
                return false;
            }
        }
        self.end.is_none_or(|e| self.in_end(x, e))
    }

    fn display(&self) -> String {
        let a = self.start.map(|v| v.to_string());
        let b = self.end.map(|v| v.to_string());
        if self.step == 1 {
            let dots = if self.incl { "..=" } else { ".." };
            return format!("{}{}{}", a.unwrap_or_default(), dots, b.unwrap_or_default());
        }
        let a = a.unwrap_or("None".into());
        let b = b.unwrap_or("None".into());
        if self.incl {
            format!("Range({a}, {b}, {}, true)", self.step)
        } else {
            format!("Range({a}, {b}, {})", self.step)
        }
    }

    /// Indices a slice selects out of `n` elements (ticket answer B; CPython `slice.indices` for the
    /// exclusive forms).
    fn indices(&self, n: i64) -> Vec<i64> {
        let up = self.step > 0;
        let norm = |b: i64| if b < 0 { b.saturating_add(n) } else { b };
        let (lo_c, hi_c) = if up { (0, n) } else { (-1, n - 1) };
        let lo = self.start.map(norm).unwrap_or(if up { 0 } else { n - 1 });
        let hi = match self.end {
            Some(e) => {
                let e = norm(e);
                match (self.incl, up) {
                    (false, _) => e,
                    (true, true) => e.saturating_add(1),
                    (true, false) => e.saturating_sub(1),
                }
            }
            None => {
                if up {
                    n
                } else {
                    -1
                }
            }
        };
        let (mut i, hi) = (lo.clamp(lo_c, hi_c), hi.clamp(lo_c, hi_c));
        let mut out = Vec::new();
        while (up && i < hi) || (!up && i > hi) {
            out.push(i);
            i += self.step;
        }
        out
    }
}

fn m(start: Option<i64>, end: Option<i64>, step: i64, incl: bool) -> Model {
    Model {
        start,
        end,
        step,
        incl,
    }
}

#[derive(Clone, Copy)]
enum Form {
    HalfOpen,
    Inclusive,
    NoStart,
    NoEnd,
    Full,
    CtorTwo,
    CtorNoEnd,
    CtorNoStart,
    CtorInclusive,
    CtorStep,
    CtorDown,
    Empty,
    FromCalls,
    InclusiveFromCalls,
}

impl Form {
    const ALL: [Form; 14] = [
        Form::HalfOpen,
        Form::Inclusive,
        Form::NoStart,
        Form::NoEnd,
        Form::Full,
        Form::CtorTwo,
        Form::CtorNoEnd,
        Form::CtorNoStart,
        Form::CtorInclusive,
        Form::CtorStep,
        Form::CtorDown,
        Form::Empty,
        Form::FromCalls,
        Form::InclusiveFromCalls,
    ];

    fn index(self) -> usize {
        match self {
            Form::HalfOpen => 0,
            Form::Inclusive => 1,
            Form::NoStart => 2,
            Form::NoEnd => 3,
            Form::Full => 4,
            Form::CtorTwo => 5,
            Form::CtorNoEnd => 6,
            Form::CtorNoStart => 7,
            Form::CtorInclusive => 8,
            Form::CtorStep => 9,
            Form::CtorDown => 10,
            Form::Empty => 11,
            Form::FromCalls => 12,
            Form::InclusiveFromCalls => 13,
        }
    }

    fn src(self) -> &'static str {
        match self {
            Form::HalfOpen => "2..5",
            Form::Inclusive => "2..=5",
            Form::NoStart => "..5",
            Form::NoEnd => "2..",
            Form::Full => "..",
            Form::CtorTwo => "Range(2, 5)",
            Form::CtorNoEnd => "Range(2, None)",
            Form::CtorNoStart => "Range(None, 5)",
            Form::CtorInclusive => "Range(2, 5, inclusive=true)",
            Form::CtorStep => "Range(0, 10, 3)",
            Form::CtorDown => "Range(3, 0, -1)",
            Form::Empty => "3..1",
            Form::FromCalls => "lo()..hi()",
            Form::InclusiveFromCalls => "lo()..=hi()",
        }
    }

    fn model(self) -> Model {
        match self {
            Form::HalfOpen | Form::CtorTwo | Form::FromCalls => m(Some(2), Some(5), 1, false),
            Form::Inclusive | Form::CtorInclusive | Form::InclusiveFromCalls => {
                m(Some(2), Some(5), 1, true)
            }
            Form::NoStart | Form::CtorNoStart => m(None, Some(5), 1, false),
            Form::NoEnd | Form::CtorNoEnd => m(Some(2), None, 1, false),
            Form::Full => m(None, None, 1, false),
            Form::CtorStep => m(Some(0), Some(10), 3, false),
            Form::CtorDown => m(Some(3), Some(0), -1, false),
            Form::Empty => m(Some(3), Some(1), 1, false),
        }
    }
}

#[derive(Clone, Copy)]
enum Use {
    For,
    ListComp,
    SetComp,
    MapComp,
    ListCtor,
    SetCtor,
    Bound,
    IterParam,
    In,
    Len,
    Fields,
    Twice,
    PassFn,
    Spawn,
    Channel,
    Eq,
    Display,
    SliceList,
    SliceStr,
    LazyBreak,
}

impl Use {
    const ALL: [Use; 20] = [
        Use::For,
        Use::ListComp,
        Use::SetComp,
        Use::MapComp,
        Use::ListCtor,
        Use::SetCtor,
        Use::Bound,
        Use::IterParam,
        Use::In,
        Use::Len,
        Use::Fields,
        Use::Twice,
        Use::PassFn,
        Use::Spawn,
        Use::Channel,
        Use::Eq,
        Use::Display,
        Use::SliceList,
        Use::SliceStr,
        Use::LazyBreak,
    ];

    fn index(self) -> usize {
        match self {
            Use::For => 0,
            Use::ListComp => 1,
            Use::SetComp => 2,
            Use::MapComp => 3,
            Use::ListCtor => 4,
            Use::SetCtor => 5,
            Use::Bound => 6,
            Use::IterParam => 7,
            Use::In => 8,
            Use::Len => 9,
            Use::Fields => 10,
            Use::Twice => 11,
            Use::PassFn => 12,
            Use::Spawn => 13,
            Use::Channel => 14,
            Use::Eq => 15,
            Use::Display => 16,
            Use::SliceList => 17,
            Use::SliceStr => 18,
            Use::LazyBreak => 19,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Use::For => "for",
            Use::ListComp => "list_comp",
            Use::SetComp => "set_comp",
            Use::MapComp => "map_comp",
            Use::ListCtor => "list_ctor",
            Use::SetCtor => "set_ctor",
            Use::Bound => "bound",
            Use::IterParam => "iter_param",
            Use::In => "in",
            Use::Len => "len",
            Use::Fields => "fields",
            Use::Twice => "twice",
            Use::PassFn => "pass_fn",
            Use::Spawn => "spawn",
            Use::Channel => "channel",
            Use::Eq => "eq",
            Use::Display => "display",
            Use::SliceList => "slice_list",
            Use::SliceStr => "slice_str",
            Use::LazyBreak => "lazy_break",
        }
    }

    /// The Chezzi function of the use; it takes `r: Range` and returns a `str`.
    fn body(self) -> &'static str {
        match self {
            Use::For => {
                "    out: List[int] = []\n    for i in r:\n        out.push(i)\n        if out.len() == 4:\n            break\n    return \"{out}\"\n"
            }
            Use::ListComp => "    xs := [i * 2 for i in r]\n    return \"{xs}\"\n",
            Use::SetComp => "    s := {i for i in r}\n    return \"{s.len()}\"\n",
            Use::MapComp => "    m := {i: i * 2 for i in r}\n    return \"{m}\"\n",
            Use::ListCtor => "    return \"{List(r)}\"\n",
            Use::SetCtor => "    return \"{Set(r).len()}\"\n",
            Use::Bound => "    return \"{head_sum(r)}\"\n",
            Use::IterParam => "    return \"{head_sum_dyn(r)}\"\n",
            Use::In => "    return \"{[3 in r, 7 in r, 0 in r]}\"\n",
            Use::Len => "    return \"{r.len()}\"\n",
            Use::Fields => "    return \"[{r.start}, {r.end}, {r.step}, {r.inclusive}]\"\n",
            Use::Twice => {
                "    a := r\n    first: List[int] = []\n    second: List[int] = []\n    for i in a:\n        first.push(i)\n        if first.len() == 3:\n            break\n    for i in a:\n        second.push(i)\n        if second.len() == 3:\n            break\n    return \"{first} {second}\"\n"
            }
            Use::PassFn => "    return \"{head_list(r)}\"\n",
            Use::Spawn => {
                "    out := Channel[Range](1)\n    parallel:\n        spawn echo(r, out)\n    return \"{out.recv()}\"\n"
            }
            Use::Channel => {
                "    ch := Channel[Range](1)\n    ch.send(r)\n    return \"{ch.recv()}\"\n"
            }
            Use::Eq => {
                "    same := Range(r.start, r.end, r.step, r.inclusive) == r\n    other := r == Range(7, 8)\n    return \"{[same, other]}\"\n"
            }
            Use::Display => "    return \"{r}\"\n",
            Use::SliceList => "    xs := [10, 20, 30, 40, 50]\n    return \"{xs[r]}\"\n",
            Use::SliceStr => "    return \"ABCDE\"[r]\n",
            Use::LazyBreak => {
                "    n := 0\n    for i in r:\n        n += i\n        if i >= 6:\n            break\n    return \"{n}\"\n"
            }
        }
    }

    /// `Ok(text)`, `Err(fault substring)`, or `None` when the cell cannot run (it never ends).
    fn expect(self, f: Model) -> Option<Result<String, String>> {
        let list = |v: &[i64]| format!("{v:?}");
        let head = |limit: usize| f.head(limit);
        let endless = f.end.is_none();
        Some(match self {
            Use::For | Use::PassFn => head(4).map(|v| list(&v)),
            Use::ListComp => {
                if endless && f.start.is_some() {
                    return None;
                }
                head(usize::MAX / 2).map(|v| list(&v.iter().map(|x| x * 2).collect::<Vec<_>>()))
            }
            Use::ListCtor => {
                if endless && f.start.is_some() {
                    return None;
                }
                head(usize::MAX / 2).map(|v| list(&v))
            }
            Use::SetComp | Use::SetCtor => {
                if endless && f.start.is_some() {
                    return None;
                }
                head(usize::MAX / 2).map(|v| v.len().to_string())
            }
            Use::MapComp => {
                if endless && f.start.is_some() {
                    return None;
                }
                head(usize::MAX / 2).map(|v| {
                    let items: Vec<String> = v.iter().map(|x| format!("{x}: {}", x * 2)).collect();
                    format!("{{{}}}", items.join(", "))
                })
            }
            Use::Bound | Use::IterParam => head(4).map(|v| v.iter().sum::<i64>().to_string()),
            Use::In => Ok(format!(
                "[{}, {}, {}]",
                f.contains(3),
                f.contains(7),
                f.contains(0)
            )),
            Use::Len => f.len().map(|n| n.to_string()),
            Use::Fields => {
                let opt = |o: Option<i64>| o.map_or("None".to_string(), |v| v.to_string());
                Ok(format!(
                    "[{}, {}, {}, {}]",
                    opt(f.start),
                    opt(f.end),
                    f.step,
                    f.incl
                ))
            }
            Use::Twice => head(3).map(|v| format!("{} {}", list(&v), list(&v))),
            Use::Spawn | Use::Channel | Use::Display => Ok(f.display()),
            Use::Eq => Ok("[true, false]".to_string()),
            Use::SliceList => Ok(list(
                &f.indices(5)
                    .iter()
                    .map(|i| [10, 20, 30, 40, 50][*i as usize])
                    .collect::<Vec<_>>(),
            )),
            Use::SliceStr => Ok(f
                .indices(5)
                .iter()
                .map(|i| ['A', 'B', 'C', 'D', 'E'][*i as usize])
                .collect()),
            Use::LazyBreak => {
                let Some(s) = f.start else {
                    return Some(Err("a range with no start cannot be iterated".into()));
                };
                let (mut n, mut cur) = (0, s);
                while f.end.is_none_or(|e| f.in_end(cur, e)) {
                    n += cur;
                    if cur >= 6 {
                        break;
                    }
                    cur += f.step;
                }
                Ok(n.to_string())
            }
        })
    }
}

#[derive(Clone, Copy)]
enum Pos {
    Statement,
    Argument,
    Return,
    Field,
    Default,
    Hole,
}

impl Pos {
    const ALL: [Pos; 6] = [
        Pos::Statement,
        Pos::Argument,
        Pos::Return,
        Pos::Field,
        Pos::Default,
        Pos::Hole,
    ];

    fn index(self) -> usize {
        match self {
            Pos::Statement => 0,
            Pos::Argument => 1,
            Pos::Return => 2,
            Pos::Field => 3,
            Pos::Default => 4,
            Pos::Hole => 5,
        }
    }

    /// The statement that binds `r` from the form's source `x`.
    fn bind(self, x: &str) -> String {
        match self {
            Pos::Statement => format!("    r := {x}\n"),
            Pos::Argument => format!("    r := ident({x})\n"),
            Pos::Return => "    r := mk()\n".to_string(),
            Pos::Field => format!("    r := Holder({x}).r\n"),
            Pos::Default => "    r := dflt()\n".to_string(),
            Pos::Hole => format!("    print(\"hole {{{x}}}\")\n"),
        }
    }
}

fn common(x: &str) -> String {
    format!(
        "struct Holder:\n    r: Range\n\nfn lo() -> int:\n    return 2\n\nfn hi() -> int:\n    return 5\n\n\
         fn mk() -> Range:\n    return {x}\n\nfn dflt(r: Range = {x}) -> Range:\n    return r\n\n\
         fn ident(r: Range) -> Range:\n    return r\n\n\
         fn head_sum[S: Iterable[int]](xs: S) -> int:\n    n := 0\n    t := 0\n    for i in xs:\n        t += i\n        n += 1\n        if n == 4:\n            break\n    return t\n\n\
         fn head_sum_dyn(xs: Iterable[int]) -> int:\n    n := 0\n    t := 0\n    for i in xs:\n        t += i\n        n += 1\n        if n == 4:\n            break\n    return t\n\n\
         fn head_list(q: Range) -> List[int]:\n    out: List[int] = []\n    for i in q:\n        out.push(i)\n        if out.len() == 4:\n            break\n    return out\n\n\
         fn echo(q: Range, out: Channel[Range]):\n    out.send(q)\n\n"
    )
}

fn program(form: Form, pos: Pos, uses: &[Use]) -> String {
    let mut p = common(form.src());
    for u in uses {
        p.push_str(&format!(
            "fn u_{}(r: Range) -> str:\n{}\n",
            u.label(),
            u.body()
        ));
    }
    p.push_str("fn main():\n");
    p.push_str(&pos.bind(form.src()));
    for u in uses {
        let l = u.label();
        p.push_str(&format!(
            "    res_{l} := recover: u_{l}(r)\n    match res_{l}:\n        ?v:\n            print(\"{l}: {{v}}\")\n        !e:\n            print(\"{l}: FAULT {{e.message()}}\")\n"
        ));
    }
    p.push_str("\nmain()\n");
    p
}

fn dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("range_value_grid");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn chezzi(args: &[&str], file: &PathBuf, threads: Option<&str>) -> (bool, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    c.args(args).arg(file).env_remove("CHEZZI_THREADS");
    if let Some(t) = threads {
        c.env("CHEZZI_THREADS", t);
    }
    let out = c.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn write(name: &str, src: &str) -> PathBuf {
    let f = dir().join(name);
    std::fs::write(&f, src).unwrap();
    f
}

#[test]
fn the_axes_list_every_variant_once() {
    for (i, f) in Form::ALL.iter().enumerate() {
        assert_eq!(f.index(), i);
    }
    for (i, u) in Use::ALL.iter().enumerate() {
        assert_eq!(u.index(), i);
    }
    for (i, p) in Pos::ALL.iter().enumerate() {
        assert_eq!(p.index(), i);
    }
}

#[test]
fn every_form_use_position_cell_checks_and_runs() {
    let mut failures = Vec::new();
    for form in Form::ALL {
        for pos in Pos::ALL {
            let uses: Vec<Use> = match pos {
                Pos::Hole => vec![],
                Pos::Statement | Pos::Argument | Pos::Return | Pos::Field | Pos::Default => {
                    Use::ALL
                        .iter()
                        .copied()
                        .filter(|u| u.expect(form.model()).is_some())
                        .collect()
                }
            };
            let name = format!("cell_{}_{}.chz", form.index(), pos.index());
            let file = write(&name, &program(form, pos, &uses));
            let (ok, text) = chezzi(&["check"], &file, None);
            if !ok {
                failures.push(format!("{name}: check failed\n{text}"));
                continue;
            }
            let mut expected = String::new();
            if let Pos::Hole = pos {
                expected.push_str(&format!("hole {}\n", form.model().display()));
            }
            for u in &uses {
                let line = match u.expect(form.model()).unwrap() {
                    Ok(t) => format!("{}: {t}\n", u.label()),
                    Err(fault) => format!("{}: FAULT {fault}\n", u.label()),
                };
                expected.push_str(&line);
            }
            // The spawn and channel uses cross the worker pool: run at three worker counts.
            for threads in [Some("1"), Some("2"), None] {
                let (ok, text) = chezzi(&["run"], &file, threads);
                if !ok || !same_output(&text, &expected) {
                    failures.push(format!(
                        "{name} (threads {threads:?}): run mismatch\n--- expected\n{expected}--- got\n{text}"
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// A fault line holds the expected fault text anywhere after `FAULT`; every other line is exact.
fn same_output(got: &str, expected: &str) -> bool {
    let (g, e): (Vec<_>, Vec<_>) = (got.lines().collect(), expected.lines().collect());
    g.len() == e.len()
        && g.iter().zip(&e).all(|(g, e)| match e.split_once("FAULT ") {
            Some((label, fault)) => {
                g.starts_with(label) && g.contains("FAULT ") && g.contains(fault)
            }
            None => g == e,
        })
}

#[test]
fn a_literal_for_prints_what_the_same_range_bound_to_a_name_prints() {
    for form in Form::ALL {
        let Some(_) = form.model().start else {
            continue;
        };
        let lit = form.src();
        let body = "    n += 1\n    print(i)\n    if n == 4:\n        break\n";
        let src = |iter: &str, pre: &str| {
            format!(
                "{}fn main():\n{pre}    n := 0\n    for i in {iter}:\n    {}\nmain()\n",
                common(lit),
                body.replace('\n', "\n    ").trim_end()
            )
        };
        let literal = write("for_lit.chz", &src(lit, ""));
        let bound = write("for_bound.chz", &src("r", &format!("    r := {lit}\n")));
        let (ok_l, out_l) = chezzi(&["run"], &literal, None);
        let (ok_b, out_b) = chezzi(&["run"], &bound, None);
        assert!(ok_l && ok_b, "{lit}: literal {out_l} bound {out_b}");
        assert_eq!(
            out_l, out_b,
            "{lit}: the literal loop and the bound loop differ"
        );
        let want = form.model().head(4).unwrap();
        let want: String = want.iter().map(|v| format!("{v}\n")).collect();
        assert_eq!(out_l, want, "{lit}");
    }
}

#[test]
fn a_literal_with_no_start_is_a_check_error_and_a_value_is_a_run_fault() {
    let lit = write("nostart_lit.chz", "for i in ..5:\n    print(i)\n");
    let (ok, text) = chezzi(&["check"], &lit, None);
    assert!(
        !ok && text.contains("a range with no start cannot be iterated"),
        "{text}"
    );
    let val = write("nostart_val.chz", "r := ..5\nfor i in r:\n    print(i)\n");
    let (ok, text) = chezzi(&["run"], &val, None);
    assert!(
        !ok && text.contains("a range with no start cannot be iterated"),
        "{text}"
    );
}

#[test]
fn a_zero_step_faults_at_construction() {
    let f = write(
        "zero_step.chz",
        "r := recover: Range(0, 3, 0)\nmatch r:\n    ?v:\n        print(\"built {v}\")\n    !e:\n        print(e.message())\n",
    );
    let (ok, text) = chezzi(&["run"], &f, None);
    assert!(ok && text.contains("Range() step cannot be zero"), "{text}");
}

#[test]
fn the_old_spellings_name_the_new_one() {
    let cases = [
        ("for i in range(3):\n    print(i)\n", "write `0..n`"),
        ("xs := [1, 2, 3]\nprint(xs[0:2])\n", "write `xs[a..b]`"),
        (
            "xs := [1, 2, 3]\nprint(xs[::-1])\n",
            "write `xs.reversed()`",
        ),
        ("print(\"abc\".reverse())\n", "write `s.reversed()`"),
    ];
    for (i, (src, want)) in cases.iter().enumerate() {
        let f = write(&format!("old_{i}.chz"), src);
        let (ok, text) = chezzi(&["run"], &f, None);
        assert!(!ok && text.contains(want), "{src}: {text}");
    }
}

/// (range source, model) rows of the slice table: the eight cells of owner answer B and the eleven
/// rows measured on `range_indices`.
fn slice_rows() -> Vec<(&'static str, Model)> {
    vec![
        ("-2..", m(Some(-2), None, 1, false)),
        ("0..=-1", m(Some(0), Some(-1), 1, true)),
        ("2..1", m(Some(2), Some(1), 1, false)),
        ("-1..0", m(Some(-1), Some(0), 1, false)),
        ("1..99", m(Some(1), Some(99), 1, false)),
        ("-99..2", m(Some(-99), Some(2), 1, false)),
        ("..-1", m(None, Some(-1), 1, false)),
        ("..", m(None, None, 1, false)),
        ("..2", m(None, Some(2), 1, false)),
        ("1..", m(Some(1), None, 1, false)),
        ("Range(None, None, -1)", m(None, None, -1, false)),
        (
            "Range(2, 0, -1, inclusive=true)",
            m(Some(2), Some(0), -1, true),
        ),
        ("Range(0, 3, 2)", m(Some(0), Some(3), 2, false)),
        (
            "Range(0, -10, 1, inclusive=true)",
            m(Some(0), Some(-10), 1, true),
        ),
        (
            "0..=9223372036854775807",
            m(Some(0), Some(i64::MAX), 1, true),
        ),
    ]
}

#[test]
fn every_slice_row_selects_the_same_elements_in_all_six_sliceable_types() {
    let src_head = "import std.concurrency\nimport ConcurrentList from std.concurrency.collection\n\n\
        struct Box:\n    items: List[int]\n\n    fn slice(self, r: Range) -> List[int]:\n        return self.items[r]\n\n";
    let mut body = String::from(
        "fn main():\n    xs := [10, 20, 30]\n    cl := ConcurrentList(RwShared([10, 20, 30]))\n    bx := Box([10, 20, 30])\n",
    );
    let mut want = String::new();
    for (src, model) in slice_rows() {
        let idx = model.indices(3);
        let nums: Vec<i64> = idx.iter().map(|i| [10, 20, 30][*i as usize]).collect();
        let chars: String = idx.iter().map(|i| ['A', 'B', 'C'][*i as usize]).collect();
        body.push_str(&format!(
            "    print(xs[{src}])\n    print(\"ABC\"[{src}])\n    print(b\"ABC\"[{src}])\n    print(bytearray(b\"ABC\")[{src}])\n    print(cl[{src}])\n    print(bx[{src}])\n"
        ));
        want.push_str(&format!(
            "{nums:?}\n{chars}\nb'{chars}'\nbytearray(b'{chars}')\n{nums:?}\n{nums:?}\n"
        ));
    }
    let f = write("slice_rows.chz", &format!("{src_head}{body}\nmain()\n"));
    let (ok, text) = chezzi(&["check"], &f, None);
    assert!(ok, "{text}");
    for threads in [Some("1"), Some("2"), None] {
        let (ok, text) = chezzi(&["run"], &f, threads);
        assert!(ok, "{text}");
        assert_eq!(text, want, "threads {threads:?}");
    }
}

#[test]
fn a_plain_index_stays_strict_and_a_range_key_stays_a_key() {
    let f = write(
        "strict.chz",
        "fn main():\n    xs := [10, 20, 30]\n    r := recover: xs[-4]\n    print(r)\n    m := {0..3: \"a\"}\n    print(m[0..3])\n    print(xs[Range(None, None, -1)] == xs.reversed())\n\nmain()\n",
    );
    let (ok, text) = chezzi(&["run"], &f, None);
    assert!(ok, "{text}");
    assert_eq!(text, "!index -4 out of bounds (len 3)\na\ntrue\n");
}

#[test]
fn a_struct_with_slice_or_index_agrees_between_check_and_run() {
    // One decider: the checker picks `slice` or `index` for a `Range` subscript, the compiler emits what it picked.
    // A type with ONE of the two hooks is legal; a type with both is the declaration error below.
    let src = "struct Sl:\n    n: int\n\n    fn slice(self, r: Range) -> List[int]:\n        return [1, 2]\n\n\
        struct Ix:\n    n: int\n\n    fn index(self, k: Range) -> str:\n        return \"idx\"\n\n\
        fn via_slice[T: Slice[List[int]]](x: T) -> List[int]:\n    return x[0..1]\n\n\
        fn via_index[T: Index[Range, str]](x: T) -> str:\n    return x[0..1]\n\n\
        fn via_value(x: Index[Range, str]) -> str:\n    return x[0..1]\n\n\
        fn main():\n    s := Sl(0)\n    i := Ix(0)\n    print(s[0..1])\n    print(via_slice(s))\n    print(i[0..1])\n    print(via_index(i))\n    print(via_value(i))\n\nmain()\n";
    let f = write("slice_or_index.chz", src);
    let (ok, text) = chezzi(&["check"], &f, None);
    assert!(ok, "{text}");
    let (ok, text) = chezzi(&["run"], &f, None);
    assert!(ok, "{text}");
    assert_eq!(text, "[1, 2]\n[1, 2]\nidx\nidx\nidx\n");
}

#[test]
fn a_struct_with_slice_and_range_index_is_a_declaration_error_naming_both() {
    // Owner decision 2026-10-13 (TICKET-254): a type takes a range as a slice or as a key, not both.
    let src = "struct Both:\n    n: int\n\n    fn slice(self, r: Range) -> List[int]:\n        return [1, 2]\n\n    fn index(self, k: Range) -> str:\n        return \"idx\"\n\n\
        fn main():\n    b := Both(0)\n    print(b[0..1])\n\nmain()\n";
    let f = write("both.chz", src);
    for sub in [&["check"][..], &["run"][..]] {
        let (ok, text) = chezzi(sub, &f, None);
        assert!(
            !ok && text.contains("'slice'")
                && text.contains("'index'")
                && text.contains("a type takes a range as a slice or as a key, not both"),
            "{sub:?}: {text}"
        );
    }
}
