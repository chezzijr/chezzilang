//! TICKET-265: a program that names a std protocol needs `import <Name> from <module>`. One cell
//! pair per `protocol` declaration in `std/*.chz` (the declarations are the one list) and per site
//! in `SITES`: the program without the import must be rejected with the import hint, the program
//! with it must run. `std/prelude.chz` declares the two global protocols (`Any`, `Error`); they
//! need no import and have no cell.

use std::path::Path;

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect};

/// `(name, type parameter count, module)` of every `protocol` declaration in `std/*.chz` outside
/// the prelude. The module is the file stem under `std.`.
fn std_protocols() -> Vec<(String, usize, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("std");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read std/") {
        let path = entry.expect("read a std entry").path();
        if path.extension().is_none_or(|e| e != "chz") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("a utf-8 stem");
        if stem == "prelude" {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read a std file");
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("protocol ") else {
                continue;
            };
            let head = rest.split(':').next().unwrap_or("").trim();
            let (name, params) = match head.split_once('[') {
                Some((name, params)) => (name, params.trim_end_matches(']').split(',').count()),
                None => (head, 0),
            };
            found.push((name.to_string(), params, format!("std.{stem}")));
        }
    }
    found.sort();
    found
}

/// Every place a program names a protocol, `{b}` standing for the protocol and its type args, with
/// the text its error names the place by. A missing site here is a site the import rule can skip.
const SITES: [(&str, &str, &str); 7] = [
    (
        "fn bound",
        "fn f[T: {b}](x: T) -> int:\n    return 0\n",
        "in bound on 'T'",
    ),
    (
        "where clause",
        "fn f[T](x: T) -> int where T: {b}:\n    return 0\n",
        "in bound on 'T'",
    ),
    (
        "method bound",
        "struct S:\n    v: int\n    fn m[T: {b}](self, x: T) -> int:\n        return 0\n",
        "in bound on 'T'",
    ),
    (
        "struct bound",
        "struct S[T: {b}]:\n    v: T\n",
        "in bound on 'T'",
    ),
    (
        "enum bound",
        "enum E[T: {b}]:\n    A(T)\n",
        "in bound on 'T'",
    ),
    (
        "protocol bound",
        "protocol Q[T: {b}]:\n    fn q(self) -> int\n",
        "in bound on 'T'",
    ),
    (
        "protocol embed",
        "protocol Q:\n    {b}\n    fn q(self) -> int\n",
        "embedded in 'Q'",
    ),
];

#[test]
fn a_std_protocol_needs_its_import_at_every_site() {
    let protocols = std_protocols();
    assert!(
        protocols.len() >= 20,
        "found {} protocol declarations in std/",
        protocols.len()
    );
    let mut cells = Vec::new();
    for (name, params, module) in &protocols {
        let args = if *params == 0 {
            String::new()
        } else {
            format!("[{}]", vec!["int"; *params].join(", "))
        };
        let b = format!("{name}{args}");
        for (site, decl, place) in SITES {
            let body = format!("{}print(1)\n", decl.replace("{b}", &b));
            let unknown: &'static str = Box::leak(
                format!("unknown name '{name}' {place}; import it from {module}").into_boxed_str(),
            );
            cells.push(Cell {
                name: format!("{name} {site} without import"),
                files: vec![("main.chz".to_string(), body.clone())],
                expect: Expect::Rejects(unknown),
            });
            cells.push(Cell {
                name: format!("{name} {site} from {module}"),
                files: vec![(
                    "main.chz".to_string(),
                    format!("import {name} from {module}\n{body}"),
                )],
                expect: Expect::Prints("1".to_string()),
            });
        }
    }
    grid_cell::run_grid("std-protocol-import", &cells, &[]);
}
