//! TICKET-234 -- a value meets a carrier slot whose payload is still open (`z := None`).
//! One program per cell: site x spelling x outcome. The spellings are a plain value (`7`), the
//! explicit `?7`, and an existing carrier (`w: int? = 5`); a conflict cell then writes a value of
//! another payload type (`"hi"`, `?"hi"`, `ws: str? = "s"`). Every cell is RUN through the built
//! binary. An accept cell reads the payload by `match` and prints it plus one (`8`, or `6` for the
//! carrier), because printed text is not the oracle: `7` and `?7` print the same. A reject cell
//! holds a fragment of the message.
//!
//! TICKET-238 -- an untyped binder of an open value (`z := None`, `xs := [None]`) is an error on
//! its own line. Each row keeps its program and states the verdict the checker gives it: a cell
//! whose program has such a binder wants `cannot infer the`, and the cells that still run are the
//! ones with a typed or same-line slot.
//!
//! Row order follows the tables of the ticket: sites, depth, places, aliases, method and call
//! arguments, closure arguments, `recover:` tails.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

/// What one cell must do: print exactly this, or be rejected with this fragment.
#[derive(Clone, Copy)]
enum W {
    P(&'static str),
    R(&'static str),
}
use W::{P, R};

enum Wants {
    /// One cell per spelling, in the order of `SPELLINGS`.
    Each([W; 3]),
    /// The program has one spelling.
    One(W),
}

/// `pre` goes at top level and `body` inside `fn main():`. `{V}` is the value and `{C}` the
/// conflicting value of the cell's spelling.
struct Row {
    name: &'static str,
    pre: &'static str,
    body: &'static str,
    want: Wants,
}

const fn row(name: &'static str, pre: &'static str, body: &'static str, want: [W; 3]) -> Row {
    Row {
        name,
        pre,
        body,
        want: Wants::Each(want),
    }
}

const fn one(name: &'static str, pre: &'static str, body: &'static str, want: W) -> Row {
    Row {
        name,
        pre,
        body,
        want: Wants::One(want),
    }
}

/// (label, `{V}`, `{C}`).
const SPELLINGS: [(&str, &str, &str); 3] = [
    ("plain", "7", r#""hi""#),
    ("?x", "?7", r#"?"hi""#),
    ("carrier", "w", "ws"),
];

const BARE: &str = "";

const BOX: &str = r#"struct Box[T]:
    v: T
"#;

const BOX_OUT: &str = r#"struct Box[T]:
    v: T
struct Out[T]:
    b: Box[T]
"#;

const BOX_SHOW: &str = r#"struct Box[T]:
    v: T
fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
"#;

const CLO: &str = r#"import Shared, RwShared, Atomic from std.concurrency
struct Box[T]:
    v: T
    fn set(self, x: T):
        self.v = x
    fn apply(self, f: fn(T) -> T):
        self.v = f(self.v)
fn upd[T](b: Box[T], f: fn(T) -> T):
    b.v = f(b.v)
fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
"#;

const DEPTH: &str = r#"fn d1(x: int?):
    match x:
        ?v: print("d1", v + 1)
        None: print("d1 none")
fn d2(x: int??):
    match x:
        ?(?v): print("d2", v + 1)
        ?None: print("d2 inner none")
        None: print("d2 none")
fn d3(x: int???):
    match x:
        ?(?(?v)): print("d3", v + 1)
        ?(?None): print("d3 in2 none")
        ?None: print("d3 in1 none")
        None: print("d3 none")
"#;

const METH: &str = r#"struct Box[T]:
    v: T
    fn set(self, x: T):
        self.v = x
    fn get(self) -> T:
        return self.v
    fn fill(b: Box[T], x: T):
        b.v = x
fn put[T](b: Box[T], x: T):
    b.v = x
fn put2[T](x: T, b: Box[T]):
    b.v = x
struct Out[T]:
    b: Box[T]
struct Two[T]:
    b: Box[T]
    x: T
enum E[T]:
    Wrap(T)
    fn same(self, x: T) -> bool:
        return true
fn pair[T](a: T, b: T) -> T:
    return a
fn takes(b: Box[int?]):
    print("took")
fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
"#;

const METH_CONC: &str = r#"import Shared, RwShared, Atomic from std.concurrency
struct Box[T]:
    v: T
    fn set(self, x: T):
        self.v = x
    fn get(self) -> T:
        return self.v
    fn fill(b: Box[T], x: T):
        b.v = x
fn put[T](b: Box[T], x: T):
    b.v = x
fn put2[T](x: T, b: Box[T]):
    b.v = x
struct Out[T]:
    b: Box[T]
struct Two[T]:
    b: Box[T]
    x: T
enum E[T]:
    Wrap(T)
    fn same(self, x: T) -> bool:
        return true
fn pair[T](a: T, b: T) -> T:
    return a
fn takes(b: Box[int?]):
    print("took")
fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
"#;

const SHOW: &str = r#"fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
"#;

const SHOW_ID: &str = r#"fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
fn id[T](x: T) -> T:
    return x
"#;

const SHOW_PICK: &str = r#"fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
fn pick(c: bool): if c: None else: 7
"#;

// The site table: every accept cell reads the payload through `show`, so a missed wrap faults
// with `cannot match on int`. Holds the re-wrap cell and the destructuring target (owner note,
// 2026-10-09 13:41Z).
const SITE_ROWS: &[Row] = &[
    row(
        "site/c_destr",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    y := 0
    z, y = {V}, 1
    z, y = {C}, 2
    print(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/c_destr_then_assign",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    y := 0
    z, y = {V}, 1
    z = {C}
    print(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_assign",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    z = {V}
    show(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_coalesce",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    a := z ?? 5
    z = {V}
    show(z)
    print(a)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_default_typed",
        r#"fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
fn g(z: int? = None):
    show(z)
"#,
        r#"
    w: int? = 5
    ws: str? = "s"
    g({V})
    g()
"#,
        [
            P(r#"8
none"#),
            P(r#"8
none"#),
            P(r#"6
none"#),
        ],
    ),
    row(
        "site/s_destr",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    y := 0
    z, y = {V}, 1
    show(z)
    print(y)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_elif",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    n := 3
    z := if n == 1: {V} elif n == 2: None else: {V}
    show(z)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "site/s_extend",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.extend([{V}])
    show(xs[1])
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_global",
        r#"fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
z := None
fn f():
    w: int? = 5
    ws: str? = "s"
    z = {V}
"#,
        r#"
    f()
    show(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_idx",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs[0] = {V}
    show(xs[0])
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_if",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    c := false
    z := if c: None else: {V}
    show(z)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "site/s_if_rev",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    c := true
    z := if c: {V} else: None
    show(z)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    one(
        "site/s_inline_if",
        SHOW_PICK,
        r#"
    show(pick(false))
    show(pick(true))
"#,
        P(r#"8
none"#),
    ),
    row(
        "site/s_insert",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.insert(0, {V})
    show(xs[0])
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_listlit",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None, {V}]
    show(xs[1])
    show(xs[0])
"#,
        [
            P(r#"8
none"#),
            P(r#"8
none"#),
            P(r#"6
none"#),
        ],
    ),
    row(
        "site/s_listlit3",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [{V}, None, {V}]
    show(xs[0])
    show(xs[1])
    show(xs[2])
"#,
        [
            P(r#"8
none
8"#),
            P(r#"8
none
8"#),
            P(r#"6
none
6"#),
        ],
    ),
    row(
        "site/s_listlit_rev",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [{V}, None]
    show(xs[0])
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "site/s_maplit",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    m := {"a": None, "b": {V}}
    show(m["b"])
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "site/s_mapset",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    m := {"a": None}
    m["b"] = {V}
    show(m["b"])
    show(m["a"])
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_match",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    n := 2
    z := match n:
        1: None
        _: {V}
    show(z)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "site/s_match3",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    n := 3
    z := match n:
        1: {V}
        2: None
        _: {V}
    show(z)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "site/s_push",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.push({V})
    show(xs[1])
    show(xs[0])
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_return",
        r#"fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
fn g(c: bool) -> int?:
    w: int? = 5
    ws: str? = "s"
    z := None
    if c:
        z = {V}
    return z
"#,
        r#"
    show(g(true))
    show(g(false))
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    one(
        "site/s_rewrap.plain",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    z = w
    show(z)
    xs := [None, w]
    show(xs[1])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "site/s_rewrap.q",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    z = w
    show(z)
    xs := [None, w]
    show(xs[1])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "site/s_rewrap.carrier",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    z = w
    show(z)
    xs := [None, w]
    show(xs[1])
"#,
        R(r#"cannot infer the"#),
    ),
    row(
        "site/s_tuple",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    t := (None, 1)
    t = ({V}, 2)
    match t:
        (?v, _): print(v + 1)
        (None, _): print("none")
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "site/s_typed",
        SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z: int? = None
    z = {V}
    show(z)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
];

// The site table again in the printed form, its conflict rows, and the rows that stay as they
// are (typed slot, typed default, open default, set add, set literal).
const CELLS_ROWS: &[Row] = &[
    row(
        "cells/assign",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    z = {V}
    print(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/c_assign",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    z = {V}
    z = {C}
    print(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/c_coalesce",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    a := z ?? 5
    z = {C}
    print(a)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/c_extend",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.extend([{V}])
    xs.extend([{C}])
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/c_global",
        r#"z := None
fn f():
    w: int? = 5
    ws: str? = "s"
    z = {V}
    z = {C}
"#,
        r#"
    f()
    print(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/c_idx",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs[0] = {V}
    xs[0] = {C}
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/c_ifexpr",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    c := true
    z := if c: None else: {V}
    z = {C}
    print(z)
"#,
        [
            R(r#"cannot assign str to int?"#),
            R(r#"'?' value: expected int, found str"#),
            R(r#"cannot assign str? to int?"#),
        ],
    ),
    row(
        "cells/c_insert",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.insert(0, {V})
    xs.insert(0, {C})
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/c_listlit",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None, {V}]
    xs.push({C})
    print(xs)
"#,
        [
            R(
                r#"argument 1 of 'push': expected int?, found str (the collection's element type is int?, fixed where the binding is declared; annotate the binding, e.g. `List[<protocol>] = []`, for a mixed/protocol collection)"#,
            ),
            R(r#"'?' value: expected int, found str"#),
            R(
                r#"argument 1 of 'push': expected int?, found str? (the collection's element type is int?, fixed where the binding is declared; annotate the binding, e.g. `List[<protocol>] = []`, for a mixed/protocol collection)"#,
            ),
        ],
    ),
    row(
        "cells/c_listlit_in",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None, {V}, {C}]
    print(xs)
"#,
        [
            R(r#"list element: expected int?, found str"#),
            R(r#"'?' value: expected int, found str"#),
            R(r#"list element: expected int?, found str?"#),
        ],
    ),
    row(
        "cells/c_maplit",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    m := {"a": None, "b": {V}}
    m["c"] = {C}
    print(m)
"#,
        [
            R(r#"cannot assign str to int?"#),
            R(r#"'?' value: expected int, found str"#),
            R(r#"cannot assign str? to int?"#),
        ],
    ),
    row(
        "cells/c_mapset",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    m := {"a": None}
    m["b"] = {V}
    m["c"] = {C}
    print(m)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/c_match",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    n := 2
    z := match n:
        1: None
        _: {V}
    z = {C}
    print(z)
"#,
        [
            R(r#"cannot assign str to int?"#),
            R(r#"'?' value: expected int, found str"#),
            R(r#"cannot assign str? to int?"#),
        ],
    ),
    row(
        "cells/c_push",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.push({V})
    xs.push({C})
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/c_tuple",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    t := (None, 1)
    t = ({V}, 2)
    t = ({C}, 3)
    print(t)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/coalesce",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    a := z ?? 5
    z = {V}
    print(a)
    print(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/defparam",
        r#"fn g(z = None):
    print(z)
"#,
        r#"
    w: int? = 5
    ws: str? = "s"
    g({V})
"#,
        [
            R(r#"parameter 'z' needs a type annotation"#),
            R(r#"parameter 'z' needs a type annotation"#),
            R(r#"parameter 'z' needs a type annotation"#),
        ],
    ),
    row(
        "cells/defparam_typed",
        r#"fn g(z: int? = None):
    print(z)
"#,
        r#"
    w: int? = 5
    ws: str? = "s"
    g({V})
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
    row(
        "cells/elif",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    n := 3
    z := if n == 1: {V} elif n == 2: None else: {V}
    print(z)
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
    row(
        "cells/extend",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.extend([{V}])
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/global",
        r#"z := None
fn f():
    w: int? = 5
    ws: str? = "s"
    z = {V}
"#,
        r#"
    f()
    print(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/idxassign",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs[0] = {V}
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/ifexpr",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    c := false
    z := if c: None else: {V}
    print(z)
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
    row(
        "cells/ifexpr_rev",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    c := true
    z := if c: {V} else: None
    print(z)
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
    row(
        "cells/ifread",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    c := false
    z := if c: None else: {V}
    match z:
        ?v: print(v + 1)
        None: print("none")
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    one(
        "cells/inlinefn.plain",
        r#"fn pick(c: bool): if c: None else: 7
"#,
        r#"
    print(pick(false))
"#,
        P(r#"7"#),
    ),
    one(
        "cells/inlinefn_match.plain",
        r#"fn pick(n: int): match n:
    1: None
    _: 7
"#,
        r#"
    print(pick(2))
"#,
        R(r#"a nested block must be indented, not written inline after ':'"#),
    ),
    row(
        "cells/insert",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.insert(0, {V})
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/listlit",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None, {V}]
    print(xs)
"#,
        [P(r#"[None, 7]"#), P(r#"[None, 7]"#), P(r#"[None, 5]"#)],
    ),
    row(
        "cells/listlit3",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [{V}, None, {V}]
    print(xs)
"#,
        [
            P(r#"[7, None, 7]"#),
            P(r#"[7, None, 7]"#),
            P(r#"[5, None, 5]"#),
        ],
    ),
    row(
        "cells/listlit_rev",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [{V}, None]
    print(xs)
"#,
        [P(r#"[7, None]"#), P(r#"[7, None]"#), P(r#"[5, None]"#)],
    ),
    row(
        "cells/listread",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None, {V}]
    match xs[1]:
        ?v: print(v + 1)
        None: print("none")
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "cells/maplit",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    m := {"a": None, "b": {V}}
    print(m["b"])
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
    row(
        "cells/mapset",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    m := {"a": None}
    m["b"] = {V}
    print(m["b"])
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/match3",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    n := 3
    z := match n:
        1: {V}
        2: None
        _: {V}
    print(z)
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
    row(
        "cells/matchexpr",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    n := 2
    z := match n:
        1: None
        _: {V}
    print(z)
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
    row(
        "cells/matchread",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    z = {V}
    match z:
        ?v: print(v + 1)
        None: print("none")
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/push",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.push({V})
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/retdirect",
        r#"fn g(c: bool) -> int?:
    w: int? = 5
    ws: str? = "s"
    if c:
        return {V}
    return None
"#,
        r#"
    print(g(true))
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
    row(
        "cells/retopen",
        r#"fn g(c: bool) -> int?:
    w: int? = 5
    ws: str? = "s"
    z := None
    if c:
        z = {V}
    return z
"#,
        r#"
    print(g(true))
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/setadd",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := {None}
    s.add({V})
    print(s.len())
"#,
        [
            R(
                r#"set element type must implement Hashable (int, str, bool, a tuple of Hashable elements, or a struct/enum defining hash(self) -> int), found None"#,
            ),
            R(
                r#"set element type must implement Hashable (int, str, bool, a tuple of Hashable elements, or a struct/enum defining hash(self) -> int), found None"#,
            ),
            R(
                r#"set element type must implement Hashable (int, str, bool, a tuple of Hashable elements, or a struct/enum defining hash(self) -> int), found None"#,
            ),
        ],
    ),
    row(
        "cells/setlit",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := {None, {V}}
    print(s.len())
"#,
        [
            R(
                r#"set element type must implement Hashable (int, str, bool, a tuple of Hashable elements, or a struct/enum defining hash(self) -> int), found None"#,
            ),
            R(
                r#"set element type must implement Hashable (int, str, bool, a tuple of Hashable elements, or a struct/enum defining hash(self) -> int), found None"#,
            ),
            R(
                r#"set element type must implement Hashable (int, str, bool, a tuple of Hashable elements, or a struct/enum defining hash(self) -> int), found None"#,
            ),
        ],
    ),
    row(
        "cells/tuple",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    t := (None, 1)
    t = ({V}, 2)
    print(t)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "cells/typed",
        BARE,
        r#"
    w: int? = 5
    ws: str? = "s"
    z: int? = None
    z = {V}
    print(z)
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
];

// Reads of a carrier binding, and the declines that stay (`[n, 7]`, `[[None], [7]]`, `[w, 7]`).
const READS_ROWS: &[Row] = &[
    one(
        "reads/assign_none_again",
        SHOW,
        r#"
    z := None
    z = 7
    z = None
    show(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/carrier_plain",
        SHOW,
        r#"
    w: int? = 5
    xs := [w, 7]
    print(xs)
"#,
        R(r#"list elements differ: int? vs int"#),
    ),
    one(
        "reads/closure",
        SHOW,
        r#"
    z := None
    f := fn(): z
    z = 7
    show(f())
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/coalesce",
        SHOW,
        r#"
    z := None
    a := z ?? 5
    z = 7
    show(z)
    print(a)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/elif",
        SHOW,
        r#"
    n := 3
    z := if n == 1: 7 elif n == 2: None else: 8
    show(z)
"#,
        P(r#"9"#),
    ),
    one(
        "reads/extend",
        SHOW,
        r#"
    xs := [None]
    xs.extend([7])
    show(xs[1])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/field",
        SHOW,
        r#"
    z := None
    z = 7
    z = 8
    show(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/global",
        r#"fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
z := None
fn f():
    z = 7
"#,
        r#"
    f()
    show(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/idx",
        SHOW,
        r#"
    xs := [None]
    xs[0] = 7
    show(xs[0])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/inline",
        SHOW_PICK,
        r#"
    show(pick(false))
    show(pick(true))
"#,
        P(r#"8
none"#),
    ),
    one(
        "reads/insert",
        SHOW,
        r#"
    xs := [None]
    xs.insert(0, 7)
    show(xs[0])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/listlit",
        SHOW,
        r#"
    xs := [7, None, 8]
    show(xs[0])
    show(xs[1])
    show(xs[2])
"#,
        P(r#"8
none
9"#),
    ),
    one(
        "reads/listvar",
        SHOW,
        r#"
    n := None
    xs := [n, 7]
    print(xs)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/loop",
        SHOW,
        r#"
    z := None
    for i in range(3):
        if i == 1:
            z = i
    show(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/maplit",
        SHOW,
        r#"
    m := {"a": None, "b": 7}
    show(m["b"])
"#,
        P(r#"8"#),
    ),
    one(
        "reads/mapset",
        SHOW,
        r#"
    m := {"a": None}
    m["b"] = 7
    show(m["b"])
    show(m["a"])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/match3",
        SHOW,
        r#"
    n := 3
    z := match n:
        1: 7
        2: None
        _: 8
    show(z)
"#,
        P(r#"9"#),
    ),
    one(
        "reads/nestedlit",
        SHOW,
        r#"
    xs := [[None], [7]]
    print(xs)
"#,
        R(r#"list elements differ: List[None] vs List[int]"#),
    ),
    one(
        "reads/push",
        SHOW,
        r#"
    xs := [None]
    xs.push(7)
    show(xs[1])
    show(xs[0])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/res_beside",
        SHOW,
        r#"
    r: int!str = 5
    xs := [None, r]
    print(xs)
"#,
        R(r#"list elements differ: None vs int!str"#),
    ),
    one(
        "reads/ret",
        r#"fn show(x: int?):
    match x:
        ?v: print(v + 1)
        None: print("none")
fn g(c: bool) -> int?:
    z := None
    if c:
        z = 7
    return z
"#,
        r#"
    show(g(true))
    show(g(false))
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/rewrap",
        SHOW,
        r#"
    w: int? = 5
    z := None
    z = w
    show(z)
    xs := [None, w]
    show(xs[1])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/tuple",
        SHOW,
        r#"
    t := (None, 1)
    t = (7, 2)
    match t:
        (?v, _): print(v + 1)
        (None, _): print("none")
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "reads/tuple_show",
        SHOW,
        r#"
    t := (None, 1)
    t = (7, 2)
    a, _ = t
    show(a)
"#,
        R(r#"cannot infer the"#),
    ),
];

// The owner depth rule: site x partner (`p0` `7`, `p1` `w`, `p2` `ww`, `q0` `?7`, `q1` `?w`,
// `q2` `?ww`), read by `d1`/`d2`/`d3`; the rows that write after the binder (`x_*`).
const DEPTH_ROWS: &[Row] = &[
    one(
        "depth/assign.p0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = 7
    d1(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/assign.p1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = w
    d1(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/assign.p2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = ww
    d2(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/assign.q0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = ?7
    d1(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/assign.q1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = ?w
    d2(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/assign.q2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = ?ww
    d3(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/if.p0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    c := false
    z := if c: None else: 7
    d1(z)
"#,
        P(r#"d1 8"#),
    ),
    one(
        "depth/if.p1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    c := false
    z := if c: None else: w
    d1(z)
"#,
        P(r#"d1 6"#),
    ),
    one(
        "depth/if.p2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    c := false
    z := if c: None else: ww
    d2(z)
"#,
        P(r#"d2 6"#),
    ),
    one(
        "depth/if.q0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    c := false
    z := if c: None else: ?7
    d1(z)
"#,
        P(r#"d1 8"#),
    ),
    one(
        "depth/if.q1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    c := false
    z := if c: None else: ?w
    d2(z)
"#,
        P(r#"d2 6"#),
    ),
    one(
        "depth/if.q2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    c := false
    z := if c: None else: ?ww
    d3(z)
"#,
        P(r#"d3 6"#),
    ),
    one(
        "depth/listlit.p0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None, 7]
    d1(xs[1])
    d1(xs[0])
"#,
        P(r#"d1 8
d1 none"#),
    ),
    one(
        "depth/listlit.p1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None, w]
    d1(xs[1])
    d1(xs[0])
"#,
        P(r#"d1 6
d1 none"#),
    ),
    one(
        "depth/listlit.p2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None, ww]
    d2(xs[1])
    d2(xs[0])
"#,
        P(r#"d2 6
d2 none"#),
    ),
    one(
        "depth/listlit.q0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None, ?7]
    d1(xs[1])
    d1(xs[0])
"#,
        P(r#"d1 8
d1 none"#),
    ),
    one(
        "depth/listlit.q1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None, ?w]
    d2(xs[1])
    d2(xs[0])
"#,
        P(r#"d2 6
d2 none"#),
    ),
    one(
        "depth/listlit.q2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None, ?ww]
    d3(xs[1])
    d3(xs[0])
"#,
        P(r#"d3 6
d3 none"#),
    ),
    one(
        "depth/listlit_rev.p0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [7, None]
    d1(xs[0])
"#,
        P(r#"d1 8"#),
    ),
    one(
        "depth/listlit_rev.p1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [w, None]
    d1(xs[0])
"#,
        P(r#"d1 6"#),
    ),
    one(
        "depth/listlit_rev.p2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [ww, None]
    d2(xs[0])
"#,
        P(r#"d2 6"#),
    ),
    one(
        "depth/listlit_rev.q0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [?7, None]
    d1(xs[0])
"#,
        P(r#"d1 8"#),
    ),
    one(
        "depth/listlit_rev.q1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [?w, None]
    d2(xs[0])
"#,
        P(r#"d2 6"#),
    ),
    one(
        "depth/listlit_rev.q2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [?ww, None]
    d3(xs[0])
"#,
        P(r#"d3 6"#),
    ),
    one(
        "depth/match.p0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    n := 2
    z := match n:
        1: None
        _: 7
    d1(z)
"#,
        P(r#"d1 8"#),
    ),
    one(
        "depth/match.p1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    n := 2
    z := match n:
        1: None
        _: w
    d1(z)
"#,
        P(r#"d1 6"#),
    ),
    one(
        "depth/match.p2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    n := 2
    z := match n:
        1: None
        _: ww
    d2(z)
"#,
        P(r#"d2 6"#),
    ),
    one(
        "depth/match.q0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    n := 2
    z := match n:
        1: None
        _: ?7
    d1(z)
"#,
        P(r#"d1 8"#),
    ),
    one(
        "depth/match.q1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    n := 2
    z := match n:
        1: None
        _: ?w
    d2(z)
"#,
        P(r#"d2 6"#),
    ),
    one(
        "depth/match.q2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    n := 2
    z := match n:
        1: None
        _: ?ww
    d3(z)
"#,
        P(r#"d3 6"#),
    ),
    one(
        "depth/push.p0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None]
    xs.push(7)
    d1(xs[1])
    d1(xs[0])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/push.p1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None]
    xs.push(w)
    d1(xs[1])
    d1(xs[0])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/push.p2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None]
    xs.push(ww)
    d2(xs[1])
    d2(xs[0])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/push.q0",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None]
    xs.push(?7)
    d1(xs[1])
    d1(xs[0])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/push.q1",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None]
    xs.push(?w)
    d2(xs[1])
    d2(xs[0])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/push.q2",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None]
    xs.push(?ww)
    d3(xs[1])
    d3(xs[0])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_deepen_after_plain",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = 7
    z = ?w
    d1(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_deepen_after_w",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = w
    z = ww
    d1(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_none_only_if",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    c := true
    z := if c: None else: None
    print(z)
    _ = w
    _ = ww
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_none_only_list",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None, None]
    print(xs)
    _ = w
    _ = ww
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_printed",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    print(z)
    print(z == None)
    _ = w
    _ = ww
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_push_deepen",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    xs := [None]
    xs.push(7)
    xs.push(?w)
    d1(xs[1])
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_read_ahead",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    d1(z)
    z = ?w
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_shallow_after_deep",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = ?w
    z = 7
    d2(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_unused",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    _ = w
    _ = ww
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "depth/x_w_after_deep",
        DEPTH,
        r#"
    w: int? = 5
    ww: int?? = ?w
    z := None
    z = ?w
    z = w
    d2(z)
"#,
        R(r#"cannot infer the"#),
    ),
];

// Typed and empty twins of the site rows, and the uses of an untyped `None` binder.
const TWINS_ROWS: &[Row] = &[
    one(
        "twins/emptyl",
        BARE,
        r#"
    z := None
    z = []
    z = [1]
    print(z)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twins/errslot",
        BARE,
        r#"
    w := !"boom"
    w = 7
    print(w)
"#,
        R(r#"a `!` value needs its type from an annotation"#),
    ),
    one(
        "twins/extend",
        BARE,
        r#"
    xs: List[int?] = [None]
    xs.extend([7])
    print(xs)
"#,
        P(r#"[None, 7]"#),
    ),
    one(
        "twins/extendvar",
        BARE,
        r#"
    xs: List[int?] = [None]
    ys := [7]
    xs.extend(ys)
    print(xs)
"#,
        R(r#"argument 1 of 'extend': expected List[int?], found List[int]"#),
    ),
    one(
        "twins/fieldopen",
        BARE,
        r#"
    z := None
    z = [1]
    z = ["a"]
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twins/idx",
        BARE,
        r#"
    xs: List[int?] = [None]
    xs[0] = 7
    print(xs)
"#,
        P(r#"[7]"#),
    ),
    one(
        "twins/mapset",
        BARE,
        r#"
    m: Map[str, int?] = {"a": None}
    m["b"] = 7
    print(m)
"#,
        P(r#"{'a': None, 'b': 7}"#),
    ),
    one(
        "twins/nested",
        BARE,
        r#"
    z: int?? = None
    z = 7
    print(z)
"#,
        R(r#"cannot assign int to int??"#),
    ),
    one(
        "twins/nilval",
        BARE,
        r#"
    z := None
    z = print(1)
"#,
        R(r#"expression returns no value (None) and cannot be used as a value"#),
    ),
    one(
        "twins/openmatch",
        BARE,
        r#"
    z := None
    match z:
        ?v: print(v + 1)
        None: print("n")
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twins/openmethod",
        BARE,
        r#"
    z := None
    print(z.foo())
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twins/openprint",
        BARE,
        r#"
    z := None
    print(z)
    xs := [None]
    print(xs)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twins/openread",
        BARE,
        r#"
    z := None
    print(z + 1)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twins/opensink",
        BARE,
        r#"
    z := None
    x: int = z
    print(x)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twins/pushvar",
        BARE,
        r#"
    xs := [None]
    y := 7
    xs.push(y)
    xs.push("s")
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twins/tuple",
        BARE,
        r#"
    t: (int?, int) = (None, 1)
    t = (7, 2)
    print(t)
"#,
        P(r#"(7, 2)"#),
    ),
    one(
        "twins/open_note",
        BARE,
        r#"
    z := None
    print(z + 1)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twins/open_note_warning",
        r#"fn g(c: bool): if c: None else: None
"#,
        r#"
    g(true)
"#,
        R(
            r#"returned by 'g' is discarded — bind it (`r := …`), or discard it explicitly (`_ := …`)"#,
        ),
    ),
];

// Places: a write through a field, `+=`, a nested fn, a `spawn:` body.
const FIELD_ROWS: &[Row] = &[
    row(
        "field/c_cap_nested",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    fn f():
        z = {V}
    f()
    z = {C}
    print(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/c_cap_outer",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    fn f():
        print(z)
    z = {V}
    z = {C}
    f()
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/c_cap_spawn",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    parallel:
        spawn:
            z = {V}
    z = {C}
    print(z)
"#,
        [
            R(
                r#"'z' is this task's copy: a write to it would be lost at the join; share it through Shared/Channel, or make a task-local copy with .copy()"#,
            ),
            R(
                r#"'z' is this task's copy: a write to it would be lost at the join; share it through Shared/Channel, or make a task-local copy with .copy()"#,
            ),
            R(
                r#"'z' is this task's copy: a write to it would be lost at the join; share it through Shared/Channel, or make a task-local copy with .copy()"#,
            ),
        ],
    ),
    row(
        "field/c_field",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    b.v = {V}
    b.v = {C}
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/c_nfield",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box([None])
    b.v.push({V})
    b.v.push({C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/c_pluseq",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs += [{V}]
    xs += [{C}]
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/cap_nested",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    fn f():
        z = {V}
    f()
    show(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/cap_outer",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    fn f():
        print(z)
    z = {V}
    f()
    show(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/cap_spawn",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    parallel:
        spawn:
            z = {V}
    show(z)
"#,
        [
            R(
                r#"'z' is this task's copy: a write to it would be lost at the join; share it through Shared/Channel, or make a task-local copy with .copy()"#,
            ),
            R(
                r#"'z' is this task's copy: a write to it would be lost at the join; share it through Shared/Channel, or make a task-local copy with .copy()"#,
            ),
            R(
                r#"'z' is this task's copy: a write to it would be lost at the join; share it through Shared/Channel, or make a task-local copy with .copy()"#,
            ),
        ],
    ),
    row(
        "field/field",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    b.v = {V}
    show(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/fieldread",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    b.v = {V}
    show(b.v)
    c := b
    match c.v:
        ?v: print(v + 1)
        None: print(0)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/nfield",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box([None])
    b.v.push({V})
    show(b.v[1])
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "field/pluseq",
        BOX_SHOW,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs += [{V}]
    show(xs[1])
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
];

// Places: nested targets (`n_*`) and their empty-collection twins (`e_*`).
const TWIN2_ROWS: &[Row] = &[
    one(
        "twin2/e_field",
        BOX_OUT,
        r#"
    b := Box([])
    b.v = [1]
    b.v = ["a"]
    print(b.v)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin2/e_nfield",
        BOX_OUT,
        r#"
    b := Box([])
    b.v.push(1)
    b.v.push("a")
    print(b.v)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin2/e_nidx",
        BOX_OUT,
        r#"
    xss := [[]]
    xss[0].push(1)
    xss[0].push("a")
    print(xss)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin2/n_boxlist",
        BOX_OUT,
        r#"
    bs := [Box(None)]
    bs[0].v = ?1
    bs[0].v = ?"a"
    print(bs[0].v)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin2/n_deep",
        BOX_OUT,
        r#"
    o := Out(Box(None))
    o.b.v = ?1
    o.b.v = ?"a"
    print(o.b.v)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin2/n_fieldread",
        BOX_OUT,
        r#"
    b := Box(None)
    b.v = ?1
    match b.v:
        ?v: print(v + 1)
        None: print(0)
    b.v = ?"a"
    match b.v:
        ?v: print(v + 1)
        None: print(0)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin2/n_global",
        r#"struct Box[T]:
    v: T
struct Out[T]:
    b: Box[T]
g := Box(None)
"#,
        r#"
    g.v = ?1
    g.v = ?"a"
    print(g.v)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin2/n_idx2",
        BOX_OUT,
        r#"
    xss := [[None]]
    xss[0][0] = ?1
    xss[0][0] = ?"a"
    print(xss)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin2/n_nidx",
        BOX_OUT,
        r#"
    xss := [[None]]
    xss[0].push(?1)
    xss[0].push(?"a")
    print(xss)
"#,
        R(r#"cannot infer the"#),
    ),
];

// Aliases, a copy that is not an alias, and the `+=` twins.
const TWIN3_ROWS: &[Row] = &[
    one(
        "twin3/alias_assign",
        BARE,
        r#"
    xs := [None]
    ys := [None]
    ys = xs
    xs.push(7)
    ys.push("hi")
    print(xs)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/alias_empty",
        BARE,
        r#"
    xs := []
    ys := xs
    xs.push(7)
    ys.push("hi")
    print(xs)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/alias_field",
        BOX,
        r#"
    b := Box(None)
    c := b
    b.v = ?7
    c.v = ?"hi"
    print(b.v)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/alias_none",
        BARE,
        r#"
    xs := [None]
    ys := xs
    xs.push(?7)
    ys.push(?"hi")
    print(xs)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/alias_none_plain",
        BARE,
        r#"
    xs := [None]
    ys := xs
    xs.push(7)
    ys.push("hi")
    print(xs)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/alias_proj",
        BOX,
        r#"
    b := Box([None])
    c := b.v
    b.v.push(7)
    c.push("hi")
    print(b.v)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/alias_proj_empty",
        BARE,
        r#"
    xss := [[]]
    c := xss[0]
    xss[0].push(7)
    c.push("hi")
    print(xss)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/copy_none",
        BARE,
        r#"
    z := None
    y := z
    z = 7
    y = "hi"
    print(z)
    print(y)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/e_pluseq",
        BARE,
        r#"
    xs := []
    xs += [1]
    xs += ["a"]
    print(xs)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/f_pluseq",
        BOX,
        r#"
    b := Box([None])
    b.v += [?7]
    b.v += [?"hi"]
    print(b.v)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "twin3/t_pluseq",
        BARE,
        r#"
    xs: List[int?] = [None]
    xs += [7]
    print(xs)
"#,
        R(r#"cannot apply += to List[int?] and List[int]"#),
    ),
];

// Method and generic call arguments typed by the receiver's type parameter.
const METH_ROWS: &[Row] = &[
    row(
        "meth/atomic",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Atomic(None)
    s.store({V})
    show(s.load())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/box_alias_set",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    c := b
    b.set({V})
    c.set({C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/box_get_first",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    show(b.get())
    b.set({V})
    b.set({C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/box_set",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    b.set({V})
    show(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/box_typed",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b: Box[int?] = Box(None)
    b.set({V})
    show(b.v)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "meth/c_box_set",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    b.set({V})
    b.set({C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/c_put",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    put(b, {V})
    put(b, {C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/c_rw",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := RwShared(None)
    s.set({V})
    s.set({C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/c_shared",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.set({V})
    s.set({C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/c_shared_list",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared([None])
    s.set([{V}])
    s.set([{C}])
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/c_takes",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    takes(b)
    b.set({C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/contains",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    print(xs.contains({V}))
    xs.push({C})
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    one(
        "meth/empty_box_set",
        METH,
        r#"
    b := Box([])
    b.set([1])
    b.set(["a"])
    print(b.v)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "meth/empty_contains",
        METH,
        r#"
    xs := []
    print(xs.contains(1))
    xs.push("a")
    print(xs)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "meth/empty_map_has",
        METH,
        r#"
    m := {}
    print(m.has("a"))
    m[1] = 2
    print(m)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "meth/empty_shared_set",
        METH_CONC,
        r#"
    s := Shared([])
    s.set([1])
    s.set(["a"])
    print(s.get())
"#,
        R(r#"cannot infer the"#),
    ),
    row(
        "meth/enum_same",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    e := E.Wrap(None)
    print(e.same({V}))
    print(e.same({C}))
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/eq",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := None
    print(z == {V})
    z = {C}
    print(z)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/map_get_or",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    m := {"a": None}
    print(m.get_or("z", {V}))
    m["b"] = {C}
    print(m)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/map_has",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    m := {"a": None}
    print(m.has("a"))
    m["b"] = {V}
    m["c"] = {C}
    print(m)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/nested_box_set",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    bs := [Box(None)]
    bs[0].set({V})
    bs[0].set({C})
    print(bs[0].v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/o_b_set",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    o := Out(Box(None))
    o.b.set({V})
    o.b.set({C})
    print(o.b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/pair_none_first",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := pair(None, {V})
    show(z)
"#,
        [
            R(r#"argument to 'pair' has type int, expected int?"#),
            P(r#"none"#),
            P(r#"none"#),
        ],
    ),
    row(
        "meth/pair_none_last",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    z := pair({V}, None)
    show(z)
"#,
        [
            R(r#"argument to 'pair' has type None, expected int"#),
            P(r#"8"#),
            P(r#"6"#),
        ],
    ),
    row(
        "meth/put",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    put(b, {V})
    show(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/put_place",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    o := Out(Box(None))
    put(o.b, {V})
    put(o.b, {C})
    print(o.b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/remove",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    xs.remove({V})
    xs.push({C})
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/rw",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := RwShared(None)
    s.set({V})
    show(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/shared",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.set({V})
    show(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/shared_alias",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    t := s
    s.set({V})
    t.set({C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/shared_list",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared([None])
    s.set([{V}])
    show(s.get()[0])
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/shared_typed",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s: Shared[int?] = Shared(None)
    s.set({V})
    show(s.get())
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "meth/shared_upd",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.update(fn(x) => {V})
    show(s.get())
"#,
        [
            R(r#"expected ':', found '='"#),
            R(r#"expected ':', found '='"#),
            R(r#"expected ':', found '='"#),
        ],
    ),
    row(
        "meth/shared_upd_typed",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.set({V})
    s.update(fn(x: int?) => x)
    show(s.get())
"#,
        [
            R(r#"expected ':', found '='"#),
            R(r#"expected ':', found '='"#),
            R(r#"expected ':', found '='"#),
        ],
    ),
    row(
        "meth/x_concat",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    xs := [None]
    ys := xs.concat([{V}])
    xs.push({C})
    print(xs)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/x_ctor",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    p := Two(b, {V})
    q := Two(b, {C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/x_ctor_none",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    p := Two(Box(None), {V})
    show(p.x)
"#,
        [
            R(r#"argument to 'Two' has type int, expected int?"#),
            P(r#"8"#),
            P(r#"6"#),
        ],
    ),
    row(
        "meth/x_depth_set",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.set(?w)
    s.set({V})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    one(
        "meth/x_empty_put",
        METH,
        r#"
    b := Box([])
    put(b, [1])
    put(b, ["a"])
    print(b.v)
"#,
        R(r#"cannot infer the"#),
    ),
    row(
        "meth/x_map_update",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    m := {"a": None}
    m.update({"b": {V}})
    m.update({"c": {C}})
    print(m)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/x_put2",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    put2({V}, b)
    put2({C}, b)
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/x_spawn_set",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    parallel:
        spawn:
            s.set({V})
    s.set({C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/x_spawn_show",
        METH_CONC,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    parallel:
        spawn:
            s.set({V})
    show(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/x_static",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    Box.fill(b, {V})
    Box.fill(b, {C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "meth/x_static2",
        METH,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    Box.fill(b, {V})
    b.set({C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
];

// Closure arguments whose `fn` type names the receiver's type parameter.
const CLO_ROWS: &[Row] = &[
    row(
        "clo/box_apply",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    b.apply(fn(x): {V})
    show(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/box_apply_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    b.apply(fn(x): {V})
    b.apply(fn(x): {C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    one(
        "clo/empty_upd_c",
        CLO,
        r#"
    s := Shared([])
    s.update(fn(xs): [1])
    s.update(fn(xs): ["a"])
    print(s.get())
"#,
        R(r#"cannot infer the"#),
    ),
    row(
        "clo/ident_upd",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.update(fn(x): x)
    s.set({V})
    s.set({C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/list_upd_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared([None])
    s.update(fn(xs): [{V}])
    s.update(fn(xs): [{C}])
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/named_upd_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    fn k(x: int?) -> int?:
        return {V}
    s.update(k)
    s.set({C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/none_upd",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.update(fn(x): None)
    s.set({V})
    show(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/place_upd_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    bs := [Box(None)]
    bs[0].apply(fn(x): {V})
    bs[0].apply(fn(x): {C})
    print(bs[0].v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/rw_write",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := RwShared(None)
    s.write(fn(x): {V})
    show(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/rw_write_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := RwShared(None)
    s.write(fn(x): {V})
    s.write(fn(x): {C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/set_then_upd_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.set({V})
    s.update(fn(x): {C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/shared_upd",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.update(fn(x): {V})
    show(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/shared_upd_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.update(fn(x): {V})
    s.update(fn(x): {C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/spawn_upd_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    parallel:
        spawn:
            s.update(fn(x): {V})
    s.update(fn(x): {C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/typed_box",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    b: Box[int?] = Box(None)
    b.apply(fn(x): {V})
    show(b.v)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "clo/typed_rw",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s: RwShared[int?] = RwShared(None)
    s.write(fn(x): {V})
    show(s.get())
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "clo/typed_shared",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s: Shared[int?] = Shared(None)
    s.update(fn(x): {V})
    show(s.get())
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "clo/typed_shared_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s: Shared[int?] = Shared(None)
    s.update(fn(x): {C})
"#,
        [
            R(r#"argument 1 of 'update': expected fn(int?) -> int?, found fn(int?) -> str"#),
            R(r#"'?' value: expected int, found str"#),
            R(r#"argument 1 of 'update': expected fn(int?) -> int?, found fn(int?) -> str?"#),
        ],
    ),
    row(
        "clo/typed_upd",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    b: Box[int?] = Box(None)
    upd(b, fn(x): {V})
    show(b.v)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "clo/upd",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    upd(b, fn(x): {V})
    show(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/upd_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    b := Box(None)
    upd(b, fn(x): {V})
    upd(b, fn(x): {C})
    print(b.v)
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
    row(
        "clo/upd_then_set_c",
        CLO,
        r#"
    w: int? = 5
    ws: str? = "s"
    s := Shared(None)
    s.update(fn(x): {V})
    s.set({C})
    print(s.get())
"#,
        [
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
            R(r#"cannot infer the"#),
        ],
    ),
];

// Closure rows with one spelling: annotated closure parameters and the rows that stay.
const CLO2_ROWS: &[Row] = &[
    one(
        "clo2/count_ann_typed",
        SHOW_ID,
        r#"
    xs: List[int?] = [None]
    print(xs.count(fn(x: int?): true))
"#,
        P(r#"1"#),
    ),
    one(
        "clo2/filter_ann_empty",
        SHOW_ID,
        r#"
    xs := []
    ys := xs.filter(fn(x: int): x > 0)
    xs.push("a")
    print(xs, ys)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "clo2/filter_ann_none",
        SHOW_ID,
        r#"
    xs := [None]
    ys := xs.filter(fn(x: int?): true)
    xs.push(?"a")
    print(xs, ys)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "clo2/filter_bare_none",
        SHOW_ID,
        r#"
    xs := [None]
    ys := xs.filter(fn(x): true)
    xs.push(?7)
    show(xs[1])
    print(ys)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "clo2/filter_wrong_ann",
        SHOW_ID,
        r#"
    xs := [None]
    xs.push(?7)
    ys := xs.filter(fn(x: str?): true)
    print(ys)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "clo2/fold_none",
        SHOW_ID,
        r#"
    xs := [None]
    n := xs.fold(0, fn(a, x): a + 1)
    xs.push(?7)
    show(xs[1])
    print(n)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "clo2/map_bare_empty",
        SHOW_ID,
        r#"
    xs := []
    ys := xs.map(fn(x): x)
    xs.push(1)
    print(xs, ys)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "clo2/map_bare_none",
        SHOW_ID,
        r#"
    xs := [None]
    ys := xs.map(fn(x): 7)
    xs.push(?"a")
    print(xs, ys)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "clo2/map_nested_generic",
        SHOW_ID,
        r#"
    xs := []
    ys := xs.map(fn(x): id(x))
    xs.push(1)
    print(xs, ys)
"#,
        R(r#"cannot infer the"#),
    ),
    one(
        "clo2/sortby_ann_empty",
        SHOW_ID,
        r#"
    xs := []
    xs.sort_by(fn(a: int, b: int): a - b)
    xs.push("a")
    print(xs)
"#,
        R(r#"cannot infer the"#),
    ),
];

// `recover:` tails and arms with binders.
const REC_ROWS: &[Row] = &[
    row(
        "rec/rdirect_typed",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r: int?!Error = recover:
        {V}
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [
            R(r#"cannot assign int! to variable of type int?!"#),
            R(r#"cannot assign int?!! to variable of type int?!"#),
            P(r#"6"#),
        ],
    ),
    row(
        "rec/rif",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        if c:
            None
        else:
            {V}
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "rec/rif3_c",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        if c:
            None
        elif d:
            {V}
        else:
            {C}
    print(r)
"#,
        [
            P(r#"7"#),
            R(r#"'?' value: expected int, found str"#),
            P(r#"5"#),
        ],
    ),
    row(
        "rec/rif3_r",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        if c:
            None
        elif c:
            {V}
        else:
            {C}
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [
            R(r#"expression returns no value (None) and cannot be used as a value"#),
            R(r#"'?' value: expected int, found str"#),
            R(r#"expression returns no value (None) and cannot be used as a value"#),
        ],
    ),
    row(
        "rec/rif_hetero",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        if c:
            print("a")
        else:
            {V}
    print(r)
"#,
        [
            P(r#"7"#),
            R(r#"'?' builds an optional or success value, found None"#),
            P(r#"5"#),
        ],
    ),
    row(
        "rec/rif_init",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        if c:
            print("a")
            None
        else:
            print("b")
            {V}
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [
            P(r#"b
8"#),
            P(r#"b
8"#),
            P(r#"b
6"#),
        ],
    ),
    row(
        "rec/rif_local",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        if c:
            None
        else:
            x := {V}
            x
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    one(
        "rec/rif_none",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    r := recover:
        if c:
            None
        else:
            None
    print(r)
"#,
        R(r#"cannot infer the"#),
    ),
    row(
        "rec/rif_print",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        if c:
            None
        else:
            {V}
    print(r)
"#,
        [P(r#"7"#), P(r#"7"#), P(r#"5"#)],
    ),
    row(
        "rec/rif_rev",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        if d:
            {V}
        else:
            None
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    one(
        "rec/rif_strint",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    r := recover:
        if c:
            "s"
        else:
            7
    print(r)
"#,
        P(r#"7"#),
    ),
    row(
        "rec/rif_typed",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r: int?!Error = recover:
        if c:
            None
        else:
            {V}
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "rec/rmatch2",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        match n:
            0: None
            _: {V}
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "rec/rmatch2_rev",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        match n:
            1: {V}
            _: None
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "rec/rmatch3_c",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        match n:
            0: None
            1: {V}
            _: {C}
    print(r)
"#,
        [
            P(r#"7"#),
            R(r#"'?' value: expected int, found str"#),
            P(r#"5"#),
        ],
    ),
    row(
        "rec/rmatch3_r",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r := recover:
        match n:
            0: {V}
            2: None
            _: {C}
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [
            R(r#"expression returns no value (None) and cannot be used as a value"#),
            R(r#"'?' value: expected int, found str"#),
            R(r#"expression returns no value (None) and cannot be used as a value"#),
        ],
    ),
    one(
        "rec/rmatch_bindk",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    r := recover:
        match n:
            0: None
            k: k + 6
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        P(r#"8"#),
    ),
    row(
        "rec/rmatch_typed",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    r: int?!Error = recover:
        match n:
            0: None
            _: {V}
    match r:
        ?v: show(v)
        !e: print("err")
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    row(
        "rec/xmatch_bind",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    w: int? = 5
    ws: str? = "s"
    z := match n:
        0: None
        k: {V}
    show(z)
"#,
        [P(r#"8"#), P(r#"8"#), P(r#"6"#)],
    ),
    one(
        "rec/xmatch_bindk",
        SHOW,
        r#"
    c := false
    d := true
    n := 1
    z := match n:
        0: None
        k: k + 6
    show(z)
"#,
        P(r#"8"#),
    ),
];

fn cells_of(rows: &[Row], out: &mut Vec<Cell>) {
    for r in rows {
        let src = format!("{}fn main():\n{}main()\n", r.pre, &r.body[1..]);
        let mut push = |name: String, src: String, w: W| {
            let expect = match w {
                P(s) => Expect::Prints(s.to_string()),
                R(s) => Expect::Rejects(s),
            };
            out.push(Cell {
                name,
                files: vec![("main.chz".to_string(), src)],
                expect,
            });
        };
        match r.want {
            Wants::One(w) => push(r.name.to_string(), src, w),
            Wants::Each(ws) => {
                for ((label, v, c), w) in SPELLINGS.into_iter().zip(ws) {
                    push(
                        format!("{} [{label}]", r.name),
                        src.replace("{V}", v).replace("{C}", c),
                        w,
                    );
                }
            }
        }
    }
}

#[test]
fn open_none_slot_grid() {
    let mut cells = Vec::new();
    for rows in [
        SITE_ROWS, CELLS_ROWS, READS_ROWS, DEPTH_ROWS, TWINS_ROWS, FIELD_ROWS, TWIN2_ROWS,
        TWIN3_ROWS, METH_ROWS, CLO_ROWS, CLO2_ROWS, REC_ROWS,
    ] {
        cells_of(rows, &mut cells);
    }
    run_grid("open_none_slot", &cells);
}
