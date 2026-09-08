//! TICKET-085: `benches/chz/map.chz`'s own header states its int-key exclusion is deliberate
//! ("Int keys hash straight to their f64 bits ... rather than string-content hashing"), so no
//! bench in this repo measures a string-keyed `Map[str, int]` -- the shape of word counts,
//! group-bys, and JSON-record pipelines. This gate fails until a sibling string-keyed bench
//! (chz + py pair, wired into `benches/run.chz`'s driver list) exists.

use std::fs;
use std::path::Path;

#[test]
fn a_string_keyed_map_bench_exists_alongside_the_int_keyed_one() {
    let chz_dir = Path::new("benches/chz");
    let py_dir = Path::new("benches/py");

    let chz_candidates: Vec<String> = fs::read_dir(chz_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", chz_dir.display()))
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".chz"))
        .filter(|name| {
            let src = fs::read_to_string(chz_dir.join(name)).unwrap_or_default();
            src.contains("m[") && (src.contains('"') || src.contains("str("))
        })
        .collect();

    assert!(
        !chz_candidates.is_empty(),
        "no file under benches/chz/ builds a Map keyed by string content -- benches/chz/map.chz's \
         own header says its int-key choice deliberately excludes string-content hashing, so add a \
         sibling bench (e.g. benches/chz/map_str.chz) that does, plus a benches/py/ counterpart, and \
         wire it into benches/run.chz's `benches := [...]` driver list"
    );

    for name in &chz_candidates {
        let stem = name.trim_end_matches(".chz");
        let py_path = py_dir.join(format!("{stem}.py"));
        assert!(
            py_path.exists(),
            "benches/chz/{name} has no CPython counterpart at {}",
            py_path.display()
        );
        let run_src = fs::read_to_string("benches/run.chz").expect("read benches/run.chz");
        assert!(
            run_src.contains(&format!("\"{stem}\"")),
            "benches/run.chz's `benches := [...]` list does not include \"{stem}\" -- the bench \
             exists but the driver never runs it"
        );
    }
}
