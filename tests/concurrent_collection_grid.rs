//! TICKET-243 — the `std.concurrency.collection` grid: collection x method x {single task, eight
//! spawned tasks} x `CHEZZI_THREADS` {1, 2, default}. Every cell RUNS through the built binary (one
//! process per worker count, DEC-230) and its printed value is compared to a literal.
//!
//! A reader cell prints the value once in the single-task program; in the eight-task program each
//! task sends it on a channel and all eight must equal the single-task value. A writer cell writes
//! one value (task index 0) in the single-task program and eight distinct values from eight tasks,
//! then prints an order-independent observation of the collection.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

enum Kind {
    /// `(expression, its printed value)`.
    Read(&'static str, &'static str),
    /// `(statement using the task index i, observation, printed after one write, after eight)`.
    Write(&'static str, &'static str, &'static str, &'static str),
}
use Kind::{Read, Write};

struct Cell {
    collection: &'static str,
    method: &'static str,
    /// Statements that bind `c`, at one indent level inside `main`.
    setup: &'static str,
    kind: Kind,
}

const LIST: &str = "c := ConcurrentList(RwShared([10, 20, 30]))";
const MAP: &str = "c := ConcurrentMap(RwShared({1: 10, 2: 20}))";
const SET: &str = "c := ConcurrentSet(RwShared({1, 2, 3}))";
const COUNTER: &str = "c := ConcurrentCounter[str].new()\n    c.add(\"a\", 5)";

const CELLS: &[Cell] = &[
    // ---- ConcurrentList ----
    Cell {
        collection: "ConcurrentList",
        method: "new",
        setup: "c := ConcurrentList[int].new()",
        kind: Read("c.len()", "0"),
    },
    Cell {
        collection: "ConcurrentList",
        method: "push",
        setup: LIST,
        kind: Write(
            "c.push(100 + i)",
            "sorted_list(c.snapshot())",
            "[10, 20, 30, 100]",
            "[10, 20, 30, 100, 101, 102, 103, 104, 105, 106, 107]",
        ),
    },
    Cell {
        collection: "ConcurrentList",
        method: "len",
        setup: LIST,
        kind: Read("c.len()", "3"),
    },
    Cell {
        collection: "ConcurrentList",
        method: "at",
        setup: LIST,
        kind: Read("[c.at(1), c.at(-1), c.at(3)]", "[20, 30, None]"),
    },
    Cell {
        collection: "ConcurrentList",
        method: "slice",
        setup: LIST,
        kind: Read("c[1..3]", "[20, 30]"),
    },
    Cell {
        collection: "ConcurrentList",
        method: "for_each",
        setup: LIST,
        kind: Read("list_each_sum(c)", "60"),
    },
    Cell {
        collection: "ConcurrentList",
        method: "fold",
        setup: LIST,
        kind: Read("c.fold(0, fn(a: int, x: int) -> int: a + x)", "60"),
    },
    Cell {
        collection: "ConcurrentList",
        method: "snapshot",
        setup: LIST,
        kind: Read("c.snapshot()", "[10, 20, 30]"),
    },
    // ---- ConcurrentMap ----
    Cell {
        collection: "ConcurrentMap",
        method: "new",
        setup: "c := ConcurrentMap[int, int].new()",
        kind: Read("c.len()", "0"),
    },
    Cell {
        collection: "ConcurrentMap",
        method: "get",
        setup: MAP,
        kind: Read("[c.get(2), c.get(3)]", "[20, None]"),
    },
    Cell {
        collection: "ConcurrentMap",
        method: "insert",
        setup: MAP,
        kind: Write(
            "_ := c.insert(100 + i, i)",
            "sorted_list(c.snapshot().keys())",
            "[1, 2, 100]",
            "[1, 2, 100, 101, 102, 103, 104, 105, 106, 107]",
        ),
    },
    Cell {
        collection: "ConcurrentMap",
        method: "remove",
        setup: "c := ConcurrentMap(RwShared({100: 0, 101: 0, 102: 0, 103: 0, 104: 0, 105: 0, 106: 0, 107: 0, 1: 0}))",
        kind: Write(
            "_ := c.remove(100 + i)",
            "sorted_list(c.snapshot().keys())",
            "[1, 101, 102, 103, 104, 105, 106, 107]",
            "[1]",
        ),
    },
    Cell {
        collection: "ConcurrentMap",
        method: "contains",
        setup: MAP,
        kind: Read("[c.contains(2), c.contains(3)]", "[true, false]"),
    },
    Cell {
        collection: "ConcurrentMap",
        method: "len",
        setup: MAP,
        kind: Read("c.len()", "2"),
    },
    Cell {
        collection: "ConcurrentMap",
        method: "get_or_insert",
        setup: MAP,
        kind: Write(
            "assert c.get_or_insert(100 + i, i) == i\n                assert c.get_or_insert(1, 99) == 10",
            "sorted_list(c.snapshot().keys())",
            "[1, 2, 100]",
            "[1, 2, 100, 101, 102, 103, 104, 105, 106, 107]",
        ),
    },
    Cell {
        collection: "ConcurrentMap",
        method: "for_each_entry",
        setup: MAP,
        kind: Read("map_each_sum(c)", "33"),
    },
    Cell {
        collection: "ConcurrentMap",
        method: "fold_entries",
        setup: MAP,
        kind: Read(
            "c.fold_entries(0, fn(a: int, k: int, v: int) -> int: a + k + v)",
            "33",
        ),
    },
    Cell {
        collection: "ConcurrentMap",
        method: "snapshot",
        setup: MAP,
        kind: Read("c.snapshot()", "{1: 10, 2: 20}"),
    },
    // ---- ConcurrentSet ----
    Cell {
        collection: "ConcurrentSet",
        method: "new",
        setup: "c := ConcurrentSet[int].new()",
        kind: Read("c.len()", "0"),
    },
    Cell {
        collection: "ConcurrentSet",
        method: "insert",
        setup: SET,
        kind: Write(
            "c.insert(100 + i)\n                c.insert(1)",
            "set_sorted(c.snapshot())",
            "[1, 2, 3, 100]",
            "[1, 2, 3, 100, 101, 102, 103, 104, 105, 106, 107]",
        ),
    },
    Cell {
        collection: "ConcurrentSet",
        method: "remove",
        setup: "c := ConcurrentSet(RwShared({100, 101, 102, 103, 104, 105, 106, 107, 1}))",
        kind: Write(
            "c.remove(100 + i)\n                c.remove(999)",
            "set_sorted(c.snapshot())",
            "[1, 101, 102, 103, 104, 105, 106, 107]",
            "[1]",
        ),
    },
    Cell {
        collection: "ConcurrentSet",
        method: "contains",
        setup: SET,
        kind: Read("[c.contains(2), c.contains(4)]", "[true, false]"),
    },
    Cell {
        collection: "ConcurrentSet",
        method: "len",
        setup: SET,
        kind: Read("c.len()", "3"),
    },
    Cell {
        collection: "ConcurrentSet",
        method: "for_each",
        setup: SET,
        kind: Read("set_each_sum(c)", "6"),
    },
    Cell {
        collection: "ConcurrentSet",
        method: "fold",
        setup: SET,
        kind: Read("c.fold(0, fn(a: int, x: int) -> int: a + x)", "6"),
    },
    Cell {
        collection: "ConcurrentSet",
        method: "snapshot",
        setup: SET,
        kind: Read("set_sorted(c.snapshot())", "[1, 2, 3]"),
    },
    // ---- ConcurrentCounter ----
    Cell {
        collection: "ConcurrentCounter",
        method: "new",
        setup: "c := ConcurrentCounter[str].new()",
        kind: Read("c.total()", "0"),
    },
    Cell {
        collection: "ConcurrentCounter",
        method: "increment",
        setup: COUNTER,
        kind: Write(
            "c.increment(\"k\")",
            "[c.count(\"k\"), c.total()]",
            "[1, 6]",
            "[8, 13]",
        ),
    },
    Cell {
        collection: "ConcurrentCounter",
        method: "add",
        setup: COUNTER,
        kind: Write(
            "c.add(\"k\", i + 1)",
            "[c.count(\"k\"), c.total()]",
            "[1, 6]",
            "[36, 41]",
        ),
    },
    Cell {
        collection: "ConcurrentCounter",
        method: "count",
        setup: COUNTER,
        kind: Read("[c.count(\"a\"), c.count(\"zz\")]", "[5, 0]"),
    },
    Cell {
        collection: "ConcurrentCounter",
        method: "total",
        setup: COUNTER,
        kind: Read("c.total()", "5"),
    },
];

const PRELUDE: &str = "import std.concurrency\nimport AtomicInt, RwShared from std.concurrency
import ConcurrentList, ConcurrentMap, ConcurrentSet, ConcurrentCounter from std.concurrency.collection

fn sorted_list(xs: List[int]) -> List[int]:
    xs.sort()
    return xs

fn set_sorted(s: Set[int]) -> List[int]:
    out: List[int] = []
    for x in s:
        out.push(x)
    out.sort()
    return out

fn list_each_sum(c: ConcurrentList[int]) -> int:
    acc := AtomicInt(0)
    c.for_each(fn(x: int): acc.add(x))
    return acc.load()

fn set_each_sum(c: ConcurrentSet[int]) -> int:
    acc := AtomicInt(0)
    c.for_each(fn(x: int): acc.add(x))
    return acc.load()

fn map_each_sum(c: ConcurrentMap[int, int]) -> int:
    acc := AtomicInt(0)
    c.for_each_entry(fn(k: int, v: int): acc.add(k + v))
    return acc.load()

";

/// `(source, expected stdout)` for one cell in one mode.
fn program(cell: &Cell, eight: bool) -> (String, String) {
    let mut src = format!("{PRELUDE}fn main():\n    {}\n", cell.setup);
    let want = match (&cell.kind, eight) {
        (Read(expr, value), false) => {
            src.push_str(&format!("    print({expr})\n"));
            value.to_string()
        }
        (Read(expr, value), true) => {
            src.push_str(&format!(
                "    ch: Channel[str] = Channel(8)\n    parallel:\n        for _t in 0..8:\n            spawn:\n                ch.send(\"{{{expr}}}\")\n    for _j in 0..8:\n        print(ch.recv())\n"
            ));
            [*value; 8].join("\n")
        }
        (Write(op, observe, one, _), false) => {
            // The single-task program reuses the eight-task statement at its indent, under `if`.
            src.push_str(&format!(
                "    i := 0\n    if i == 0:\n        if i == 0:\n            if i == 0:\n                {op}\n    print({observe})\n"
            ));
            one.to_string()
        }
        (Write(op, observe, _, all), true) => {
            src.push_str(&format!(
                "    parallel:\n        for i in 0..8:\n            spawn:\n                assert i >= 0\n                {op}\n    print({observe})\n"
            ));
            all.to_string()
        }
    };
    src.push_str("main()\n");
    (src, want)
}

fn run(dir: &Path, src: &str, threads: &str) -> Result<String, String> {
    let file = dir.join("main.chz");
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&file)
        .env("CHEZZI_THREADS", threads)
        .output()
        .expect("spawn chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "exit {:?}; stdout {stdout:?}; stderr {:?}",
            out.status.code(),
            stderr.lines().take(3).collect::<Vec<_>>()
        ))
    }
}

#[test]
fn every_cell_runs_at_every_worker_count() {
    let dir = std::env::temp_dir().join(format!("chezzi-ccgrid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut fails = Vec::new();
    for cell in CELLS {
        for eight in [false, true] {
            let (src, want) = program(cell, eight);
            for threads in ["1", "2", "0"] {
                let got = run(&dir, &src, threads);
                if got.as_deref() != Ok(want.as_str()) {
                    fails.push(format!(
                        "{}.{} [{} at CHEZZI_THREADS={threads}]: want {want:?}, got {got:?}",
                        cell.collection,
                        cell.method,
                        if eight { "eight tasks" } else { "single task" },
                    ));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        fails.is_empty(),
        "{} red cell(s):\n{}",
        fails.len(),
        fails.join("\n")
    );
}

/// Every method `std/concurrency/collection.chz` declares on a `Concurrent*` struct has a cell, and
/// every cell names a declared method.
#[test]
fn the_grid_covers_every_declared_method() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("std/concurrency/collection.chz"),
    )
    .unwrap();
    let mut declared = BTreeSet::new();
    let mut owner: Option<String> = None;
    for line in src.lines() {
        if let Some(rest) = line.strip_prefix("struct ") {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            owner = name.starts_with("Concurrent").then_some(name);
        } else if !line.starts_with(' ') && !line.starts_with('#') && !line.trim().is_empty() {
            owner = None;
        } else if let (Some(o), Some(rest)) = (&owner, line.strip_prefix("    fn ")) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            assert!(
                rest[name.len()..].starts_with(['(', '[']),
                "unparsed method line: {line:?}"
            );
            declared.insert((o.clone(), name));
        }
    }
    let covered: BTreeSet<(String, String)> = CELLS
        .iter()
        .map(|c| (c.collection.to_string(), c.method.to_string()))
        .collect();
    assert!(
        declared.iter().any(|(c, _)| c == "ConcurrentCounter"),
        "the parse found no ConcurrentCounter method: {declared:?}"
    );
    assert_eq!(
        covered, declared,
        "left: the grid's cells; right: collection.chz"
    );
}
