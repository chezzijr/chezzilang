//! TICKET-218 grid: a C width is range-checked where a Chezzi value crosses into C. Cells: every
//! declared width × {extern param, struct field, `ffi.store_<w>`} × {min, max, min-1, max+1}, plus a
//! callback return, a vararg, the `_at` store form, `float32` {FLT_MAX, 1e39, NaN, inf} and the import
//! cells. Every value reaches C through `fn v(x: int) -> int: return x`, so no cell is a constant.
//! The min/max values are `std.ffi`'s own `<W>_MIN`/`<W>_MAX` constants, and every expected fault
//! message is built from `CType`, so a wrong constant goes red in either direction. Skips loudly
//! (`SKIP ffi_width_range_grid: ...`) when `cc` is missing (DEC-006).
//!
//! References, run 2026-10-06: Python `struct.pack('b', 300)` raises `'b' format requires
//! -128 <= number <= 127`; `struct.pack('f', 1e39)` raises `OverflowError: float too large to pack
//! with f format`; NaN and inf pack to `nan`/`inf`.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

use chezzi::native::cffi::{CType, width_ctype};

/// `(std.ffi name, C type, constant prefix)` of each integer width.
const INT_WIDTHS: &[(&str, &str, &str)] = &[
    ("int8", "int8_t", "INT8"),
    ("int16", "int16_t", "INT16"),
    ("int32", "int32_t", "INT32"),
    ("int64", "int64_t", "INT64"),
    ("uint8", "uint8_t", "UINT8"),
    ("uint16", "uint16_t", "UINT16"),
    ("uint32", "uint32_t", "UINT32"),
    ("uint64", "uint64_t", "UINT64"),
];

/// The C fixture: an identity fn and a one-field struct reader per width, a callback caller and a
/// variadic sum.
fn fixture_c() -> String {
    let mut c = String::from("#include <stdarg.h>\n#include <stdint.h>\n");
    for (w, ct, _) in INT_WIDTHS {
        c.push_str(&format!(
            "{ct} id_{w}({ct} x) {{ return x; }}\n\
             typedef struct {{ {ct} a; }} S_{w};\n\
             int64_t s_{w}_get(S_{w} s) {{ return (int64_t)s.a; }}\n"
        ));
    }
    c.push_str(
        "float id_float32(float x) { return x; }\n\
         typedef struct { float a; } S_float32;\n\
         double s_float32_get(S_float32 s) { return s.a; }\n\
         int8_t cb8(int8_t (*f)(int8_t)) { return f(1); }\n\
         double vsum(const char *fmt, ...) { va_list ap; va_start(ap, fmt); double t = 0;\n\
           for (const char *p = fmt; *p; p++) if (*p == 'l') t += va_arg(ap, long);\n\
           va_end(ap); return t; }\n",
    );
    c
}

/// The Chezzi prelude: the widths, one struct per width, the externs and the value laundering fns.
fn prelude(so: &Path) -> String {
    let mut p = String::from(
        "import std.ffi\nimport std.math\n\
         import int8, int16, int32, int64, uint8, uint16, uint32, uint64, float32 from std.ffi\n\n\
         fn v(x: int) -> int:\n    return x\n\n\
         fn vf(x: float) -> float:\n    return x\n\n",
    );
    for w in INT_WIDTHS.iter().map(|w| w.0).chain(["float32"]) {
        p.push_str(&format!("struct S_{w}:\n    a: {w}\n\n"));
    }
    for (w, _, _) in INT_WIDTHS {
        p.push_str(&format!(
            "fn st_{w}(x: int) -> int:\n    p := ffi.alloc(8)\n    defer ffi.free(p)\n    \
             ffi.store_{w}(p, x)\n    return ffi.load_{w}(p)\n\n"
        ));
    }
    p.push_str(
        "fn st_float32(x: float) -> float:\n    p := ffi.alloc(8)\n    defer ffi.free(p)\n    \
         ffi.store_float32(p, x)\n    return ffi.load_float32(p)\n\n\
         fn st_int8_at(x: int) -> int:\n    p := ffi.alloc(8)\n    defer ffi.free(p)\n    \
         ffi.store_int8_at(p, 0, x)\n    return ffi.load_int8(p)\n\n",
    );
    p.push_str(&format!("extern \"{}\":\n", so.display()));
    for w in INT_WIDTHS.iter().map(|w| w.0) {
        p.push_str(&format!(
            "    fn id_{w}(x: {w}) -> {w}\n    fn s_{w}_get(s: S_{w}) -> int\n"
        ));
    }
    p.push_str(
        "    fn id_float32(x: float32) -> float32\n    fn s_float32_get(s: S_float32) -> float\n    \
         fn cb8(f: fn(int8) -> int8) -> int8\n    fn vsum(fmt: str, ...) -> float\n\n",
    );
    p
}

/// One grid cell: the Chezzi expression and the line it must print (`ok <value>` or `err <message>`).
struct Cell {
    name: String,
    expr: String,
    want: String,
}

fn fault(ct: &CType, shown: &str) -> String {
    format!(
        "err value {shown} does not fit {} {}",
        ct.width_name().unwrap(),
        ct.range_text()
    )
}

/// A struct-field fault reaches the program through the struct-arg marshaller, which names the
/// argument: `argument 0 to 's_int8_get': value 300 does not fit int8 (-128..127)`.
fn field_want(getter: &str, want: &str) -> String {
    match want.strip_prefix("err ") {
        Some(msg) => format!("err argument 0 to '{getter}': {msg}"),
        None => want.to_string(),
    }
}

fn cells() -> Vec<Cell> {
    let mut out = Vec::new();
    let mut cell = |name: String, expr: String, want: String| out.push(Cell { name, expr, want });
    for (w, _, k) in INT_WIDTHS {
        let ct = width_ctype(w).unwrap();
        let (lo, hi) = ct.int_range().unwrap();
        let signed = lo < 0;
        let min_expr = if signed {
            format!("ffi.{k}_MIN")
        } else {
            "0".to_string()
        };
        let max_expr = format!("ffi.{k}_MAX");
        let mut vals: Vec<(&str, String, String)> = Vec::new();
        match *w {
            // int64 is never checked; its min-1/max+1 do not exist as Chezzi ints.
            "int64" => {
                vals.push(("min", min_expr.clone(), format!("ok {lo}")));
                vals.push(("max", max_expr.clone(), format!("ok {hi}")));
            }
            // uint64 passes any int at run time as its bit pattern; UINT64_MAX is -1.
            "uint64" => {
                vals.push(("min", min_expr.clone(), "ok 0".to_string()));
                vals.push(("max", max_expr.clone(), "ok -1".to_string()));
                vals.push(("min-1", "-1".to_string(), "ok -1".to_string()));
                vals.push((
                    "int64max",
                    "9223372036854775807".to_string(),
                    format!("ok {}", i64::MAX),
                ));
            }
            _ => {
                vals.push(("min", min_expr.clone(), format!("ok {lo}")));
                vals.push(("max", max_expr.clone(), format!("ok {hi}")));
                vals.push((
                    "min-1",
                    format!("{min_expr} - 1"),
                    fault(&ct, &(lo - 1).to_string()),
                ));
                vals.push((
                    "max+1",
                    format!("{max_expr} + 1"),
                    fault(&ct, &(hi + 1).to_string()),
                ));
            }
        }
        for (label, e, want) in vals {
            cell(
                format!("{w}_param_{label}"),
                format!("id_{w}(v({e}))"),
                want.clone(),
            );
            cell(
                format!("{w}_field_{label}"),
                format!("s_{w}_get(S_{w}(v({e})))"),
                field_want(&format!("s_{w}_get"), &want),
            );
            cell(
                format!("{w}_store_{label}"),
                format!("st_{w}(v({e}))"),
                want,
            );
        }
    }
    let f32 = CType::Float32;
    for (label, e, want) in [
        (
            "max",
            "ffi.FLT_MAX".to_string(),
            "ok 3.4028234663852886e+38".to_string(),
        ),
        ("beyond", "1e39".to_string(), fault(&f32, "1e39")),
        ("neg_beyond", "-1e39".to_string(), fault(&f32, "-1e39")),
        ("nan", "math.nan".to_string(), "ok NaN".to_string()),
        ("inf", "math.inf".to_string(), "ok inf".to_string()),
    ] {
        cell(
            format!("float32_param_{label}"),
            format!("id_float32(vf({e}))"),
            want.clone(),
        );
        cell(
            format!("float32_field_{label}"),
            format!("s_float32_get(S_float32(vf({e})))"),
            field_want("s_float32_get", &want),
        );
        cell(
            format!("float32_store_{label}"),
            format!("st_float32(vf({e}))"),
            want,
        );
    }
    let i8t = CType::Int8;
    cell(
        "int8_store_at_beyond".into(),
        "st_int8_at(v(300))".into(),
        fault(&i8t, "300"),
    );
    cell(
        "int8_store_at_in".into(),
        "st_int8_at(v(-5))".into(),
        "ok -5".into(),
    );
    cell(
        "callback_ret_in".into(),
        "cb8(fn(x: int8) -> int8: v(5))".into(),
        "ok 5".into(),
    );
    cell(
        "callback_ret_beyond".into(),
        "cb8(fn(x: int8) -> int8: v(300))".into(),
        fault(&i8t, "300"),
    );
    // DEC-217: no width reaches a vararg — an int vararg is a C long, so 300 passes unchanged.
    cell(
        "vararg_width_local".into(),
        "vararg_width()".into(),
        "ok 300.0".into(),
    );
    out
}

fn program(prelude: &str, cells: &[Cell]) -> String {
    let mut src = prelude.to_string();
    src.push_str(
        "fn vararg_width() -> float:\n    x: int8 = v(300)\n    return vsum(\"l\", x)\n\n",
    );
    for (i, c) in cells.iter().enumerate() {
        src.push_str(&format!(
            "r{i} := recover: {}\nmatch r{i}:\n    ?x: print(\"{} ok \" + str(x))\n    \
             !e: print(\"{} err \" + e.message())\n",
            c.expr, c.name, c.name
        ));
    }
    src
}

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("chezzi_t218_grid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::write(&p, contents).unwrap();
        p
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_chezzi(sub: &str, entry: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg(sub)
        .arg(entry)
        .output()
        .expect("spawn chezzi")
}

#[test]
fn width_range_grid() {
    if Command::new("cc").arg("--version").output().is_err() {
        eprintln!("SKIP ffi_width_range_grid: cc not found");
        return;
    }
    let t = TmpDir::new();
    let fixture = t.write("fixture.c", &fixture_c());
    let so = t.0.join("libwidth.so");
    let st = Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&so)
        .arg(&fixture)
        .status()
        .expect("spawn cc");
    assert!(st.success(), "cc failed to build the fixture .so");

    let cells = cells();
    let entry = t.write("grid.chz", &program(&prelude(&so), &cells));
    let out = run_chezzi("run", &entry);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut red: Vec<String> = Vec::new();
    if !out.status.success() {
        red.push(format!(
            "chezzi run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    for c in &cells {
        let got = stdout
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{} ", c.name)));
        match got {
            Some(g) if g == c.want => {}
            Some(g) => red.push(format!("{}: got `{g}`, want `{}`", c.name, c.want)),
            None => red.push(format!("{}: no output, want `{}`", c.name, c.want)),
        }
    }

    // Import cells: every name `std/ffi.chz` declares imports and runs; an undeclared width does not
    // check; `ptr` stays opaque.
    let imports = t.write(
        "imports.chz",
        "import ptr, int8, int16, int32, int64, uint8, uint16, uint32, uint64, float32 from std.ffi\nprint(\"ok\")\n",
    );
    let out = run_chezzi("run", &imports);
    if !out.status.success() || String::from_utf8_lossy(&out.stdout).trim() != "ok" {
        red.push(format!(
            "import cell: exit {:?}, stderr {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    for (name, src) in [
        ("undeclared", "import int128 from std.ffi\n"),
        ("ptr_opaque", "import ptr from std.ffi\np: ptr = 5\n"),
    ] {
        let e = t.write(&format!("{name}.chz"), src);
        if run_chezzi("check", &e).status.success() {
            red.push(format!("import cell {name}: checked clean"));
        }
    }
    assert!(red.is_empty(), "red cells:\n{}", red.join("\n"));
}
