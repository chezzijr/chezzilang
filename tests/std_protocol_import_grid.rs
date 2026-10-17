//! TICKET-265: a bound that names a std protocol needs `import <Name> from <module>`. One cell
//! pair per `protocol` declaration in `std/*.chz` (the declarations are the one list): the
//! program without the import must be rejected, the program with it must run. `std/prelude.chz`
//! declares the two global protocols (`Any`, `Error`); they need no import and have no cell.

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

#[test]
fn a_std_protocol_bound_needs_its_import() {
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
        let body = format!("fn f[T: {name}{args}](x: T) -> int:\n    return 0\nprint(1)\n");
        let unknown: &'static str = Box::leak(format!("unknown name '{name}'").into_boxed_str());
        cells.push(Cell {
            name: format!("{name} without import"),
            files: vec![("main.chz".to_string(), body.clone())],
            expect: Expect::Rejects(unknown),
        });
        cells.push(Cell {
            name: format!("{name} from {module}"),
            files: vec![(
                "main.chz".to_string(),
                format!("import {name} from {module}\n{body}"),
            )],
            expect: Expect::Prints("1".to_string()),
        });
    }
    grid_cell::run_grid("std-protocol-import", &cells);
}
