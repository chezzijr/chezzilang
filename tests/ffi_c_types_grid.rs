//! TICKET-217 grid: `float32`, nested structs by value and C variadic externs, judged against a C
//! reference program built with `cc` and RUN. One fixture `.so` (`FIXTURE_C`) serves both sides.
//!
//! Accept cells: each prints `<cell> <value>` from a Chezzi program and from the C reference; the
//! values compare as parsed `f64`, exactly. Reject cells: `chezzi check` exits non-zero and its stderr
//! contains the quoted text. Skips loudly (`SKIP ffi_c_types_grid: ...`) when `cc` is missing (DEC-006).
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE_C: &str = r#"#include <stdarg.h>
#include <stdio.h>
#include <string.h>
typedef struct { float x, y; } Vector2;
typedef struct { Vector2 offset; Vector2 target; float rotation; float zoom; } Camera2D;
typedef struct { int a; int b; } In;
typedef struct { In i; int c; } Out;
typedef struct { Out o; double d; } Deep;
float f32_add(float a, float b) { return a + b; }
float f32_id(float a) { return a; }
double f32_widen(float a) { return a; }
float v2_sum(Vector2 v) { return v.x + v.y; }
Vector2 v2_make(float x, float y) { Vector2 v = {x, y}; return v; }
float cam_sum(Camera2D c) { return c.offset.x + c.offset.y + c.target.x + c.target.y + c.rotation + c.zoom; }
Camera2D cam_shift(Camera2D c, float dx) { c.offset.x += dx; c.target.y += dx; c.zoom *= 2; return c; }
int out_sum(Out o) { return o.i.a + o.i.b + o.c; }
Out out_make(int a, int b, int c) { Out o = {{a, b}, c}; return o; }
double deep_sum(Deep d) { return d.o.i.a + d.o.i.b + d.o.c + d.d; }
Deep deep_make(int a, int b, int c, double d) { Deep x = {{{a, b}, c}, d}; return x; }
double vsum(const char *fmt, ...) { va_list ap; va_start(ap, fmt); double t = 0;
  for (const char *p = fmt; *p; p++) switch (*p) {
    case 'i': t += va_arg(ap, int); break;      case 'l': t += va_arg(ap, long); break;
    case 'd': t += va_arg(ap, double); break;   case 's': t += strlen(va_arg(ap, char *)); break;
    case 'p': t += (va_arg(ap, void *) != NULL); break; }
  va_end(ap); return t; }
"#;

/// The C reference `main`: one `<cell> <value>` line per accept cell, `%.17g` so the f64 round-trips.
const REFERENCE_MAIN: &str = r#"#include <math.h>
#include <stdlib.h>
#define P(name, v) printf("%s %.17g\n", name, (double)(v))
int main(void) {
  P("f32_param_ret", f32_add(0.1, 0.2));
  P("f32_round_in", f32_id(16777217.0));
  P("f32_overflow", f32_id(1e39));
  P("f32_param_exact", f32_widen(1.5));
  P("f32_field_param", v2_sum((Vector2){0.1f, 0.2f}));
  P("f32_field_ret", v2_make(0.1, 2.0).x);
  P("f32_vararg", vsum("d", (double)f32_id(0.1)));
  P("f32_alias_cross_module", sqrtf(2.0f));
  P("nest1_param", out_sum((Out){{1, 2}, 3}));
  P("nest1_ret", out_make(1, 2, 3).i.b);
  Camera2D cam = {{1.0f, 2.0f}, {3.0f, 4.0f}, 0.5f, 1.25f};
  P("nest1_f32_param", cam_sum(cam));
  Camera2D sh = cam_shift(cam, 10.0f);
  P("nest1_f32_ret_offset_x", sh.offset.x);
  P("nest1_f32_ret_target_y", sh.target.y);
  P("nest1_f32_ret_zoom", sh.zoom);
  P("nest1_f32_ret_rotation", sh.rotation);
  P("nest2_param", deep_sum((Deep){{{1, 2}, 3}, 0.5}));
  Deep dm = deep_make(1, 2, 3, 0.5);
  P("nest2_ret_a", dm.o.i.a);
  P("nest2_ret_d", dm.d);
  P("va_int", vsum("l", 2L));
  P("va_float", vsum("d", 2.5));
  P("va_bool", vsum("ii", 1, 0));
  P("va_str", vsum("s", "abc"));
  P("va_ptr", vsum("pp", (void *)0, malloc(8)));
  P("va_width", vsum("l", (long)out_sum((Out){{1, 2}, 3})));
  P("va_mixed", vsum("ilds", 1, 2L, 2.5, "abc"));
  P("va_none", vsum(""));
  return 0;
}
"#;

/// The Chezzi prelude every program shares; `{SO}` is the fixture path.
const PRELUDE: &str = r#"import std.ffi
import float32, int32 from std.ffi

struct Vector2:
    x: float32
    y: float32

struct Camera2D:
    offset: Vector2
    target: Vector2
    rotation: float32
    zoom: float32

struct In:
    a: int32
    b: int32

struct Out:
    i: In
    c: int32

struct Deep:
    o: Out
    d: float

extern "{SO}":
    fn f32_add(a: float32, b: float32) -> float32
    fn f32_id(a: float32) -> float32
    fn f32_widen(a: float32) -> float
    fn v2_sum(v: Vector2) -> float32
    fn v2_make(x: float32, y: float32) -> Vector2
    fn cam_sum(c: Camera2D) -> float32
    fn cam_shift(c: Camera2D, dx: float32) -> Camera2D
    fn out_sum(o: Out) -> int32
    fn out_make(a: int32, b: int32, c: int32) -> Out
    fn deep_sum(d: Deep) -> float
    fn deep_make(a: int32, b: int32, c: int32, d: float) -> Deep
    fn vsum(fmt: str, ...) -> float

"#;

/// Accept-cell programs, grouped by feature so one red feature does not mask the others.
const F32_CELLS: &str = r#"print("f32_param_ret", f32_add(0.1, 0.2))
print("f32_round_in", f32_id(16777217.0))
print("f32_overflow", f32_id(1e39))
print("f32_param_exact", f32_widen(1.5))
print("f32_field_param", v2_sum(Vector2(0.1, 0.2)))
print("f32_field_ret", v2_make(0.1, 2.0).x)
print("f32_vararg", vsum("d", f32_id(0.1)))
"#;

const NEST_CELLS: &str = r#"print("nest1_param", out_sum(Out(In(1, 2), 3)))
print("nest1_ret", out_make(1, 2, 3).i.b)
cam := Camera2D(Vector2(1.0, 2.0), Vector2(3.0, 4.0), 0.5, 1.25)
print("nest1_f32_param", cam_sum(cam))
sh := cam_shift(cam, 10.0)
print("nest1_f32_ret_offset_x", sh.offset.x)
print("nest1_f32_ret_target_y", sh.target.y)
print("nest1_f32_ret_zoom", sh.zoom)
print("nest1_f32_ret_rotation", sh.rotation)
print("nest2_param", deep_sum(Deep(Out(In(1, 2), 3), 0.5)))
dm := deep_make(1, 2, 3, 0.5)
print("nest2_ret_a", dm.o.i.a)
print("nest2_ret_d", dm.d)
"#;

const VA_CELLS: &str = r#"print("va_int", vsum("l", 2))
print("va_float", vsum("d", 2.5))
print("va_bool", vsum("ii", true, false))
print("va_str", vsum("s", "abc"))
print("va_ptr", vsum("pp", ffi.null(), ffi.alloc(8)))
print("va_width", vsum("l", out_sum(Out(In(1, 2), 3))))
print("va_mixed", vsum("ilds", true, 2, 2.5, "abc"))
print("va_none", vsum(""))
"#;

/// `f32_alias_cross_module`: a `float32` alias exported from a sibling module.
const REAL_DEFS: &str = "import float32 from std.ffi\n\ntype Real = float32\n";
const ALIAS_MAIN: &str = r#"import real_defs
import Real from real_defs

extern "libm":
    fn sqrtf(x: Real) -> real_defs.Real

print("f32_alias_cross_module", sqrtf(2.0))
"#;

/// Reject cells: `(name, program tail after PRELUDE, expected stderr text)`. A tail that must not
/// see the prelude starts with `!`.
const REJECTS: &[(&str, &str, &str)] = &[
    (
        "str_field",
        "struct N:\n    name: str\n    k: int32\n\nstruct Out2:\n    n: N\n\nextern \"libm\":\n    fn take2(o: Out2) -> int\n",
        "field 'name' of type 'str' is not C-marshallable",
    ),
    (
        "recursive_struct",
        "struct Node:\n    v: int32\n    next: Node\n\nextern \"libm\":\n    fn take3(n: Node) -> int\n",
        "is recursively defined and cannot be C-marshallable",
    ),
    (
        "mutual_recursive_struct",
        "struct A:\n    b: B\n\nstruct B:\n    a: A\n\nextern \"libm\":\n    fn take4(a: A) -> int\n",
        "is recursively defined and cannot be C-marshallable",
    ),
    (
        "struct_vararg",
        "print(vsum(\"x\", Out(In(1, 2), 3)))\n",
        "cannot be passed to a C variadic parameter",
    ),
    (
        "callback_vararg",
        "print(vsum(\"x\", fn(n: int) -> int: n))\n",
        "cannot be passed to a C variadic parameter",
    ),
    (
        "optstr_vararg",
        "s: str? = None\nprint(vsum(\"x\", s))\n",
        "cannot be passed to a C variadic parameter",
    ),
    (
        "list_vararg",
        "print(vsum(\"x\", [1]))\n",
        "cannot be passed to a C variadic parameter",
    ),
    (
        "too_few_fixed",
        "print(vsum())\n",
        "'vsum' expects at least 1 argument(s), got 0",
    ),
    (
        "ellipsis_outside_extern",
        "!fn f(a: int, ...):\n    pass\n",
        "only allowed as the last parameter of an extern fn",
    ),
    (
        "ellipsis_not_last",
        "!extern \"libm\":\n    fn g(..., a: int)\n",
        "must be the last parameter of an extern fn",
    ),
    (
        "named_variadic_in_extern",
        "!extern \"libm\":\n    fn h(...xs: int)\n",
        "variadic parameters are not supported here",
    ),
];

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("chezzi_t217_grid_{}", std::process::id()));
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

/// Parse `<cell> <value>` lines into a map. A line that is not that shape is kept under its own text
/// with a NaN value, so it shows up in the diff.
fn cells(out: &str) -> BTreeMap<String, f64> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| match l.split_once(' ') {
            Some((k, v)) => (k.to_string(), v.trim().parse::<f64>().unwrap_or(f64::NAN)),
            None => (l.to_string(), f64::NAN),
        })
        .collect()
}

fn run_chezzi(sub: &str, entry: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg(sub)
        .arg(entry)
        .output()
        .expect("spawn chezzi")
}

#[test]
fn c_types_grid_matches_the_c_reference() {
    if Command::new("cc").arg("--version").output().is_err() {
        eprintln!("SKIP ffi_c_types_grid: cc not found");
        return;
    }
    let t = TmpDir::new();
    let fixture = t.write("fixture.c", FIXTURE_C);
    let so = t.0.join("libgrid.so");
    let st = Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&so)
        .arg(&fixture)
        .status()
        .expect("spawn cc");
    assert!(st.success(), "cc failed to build the fixture .so");
    let refsrc = t.write("ref.c", &format!("{FIXTURE_C}{REFERENCE_MAIN}"));
    let refbin = t.0.join("ref");
    let st = Command::new("cc")
        .args(["-O0", "-o"])
        .arg(&refbin)
        .arg(&refsrc)
        .arg("-lm")
        .status()
        .expect("spawn cc");
    assert!(st.success(), "cc failed to build the C reference");
    let refout = Command::new(&refbin).output().expect("run the C reference");
    assert!(refout.status.success(), "the C reference failed");
    let want = cells(&String::from_utf8_lossy(&refout.stdout));

    let prelude = PRELUDE.replace("{SO}", so.to_str().unwrap());
    let mut got: BTreeMap<String, f64> = BTreeMap::new();
    let mut red: Vec<String> = Vec::new();
    for (group, body) in [("f32", F32_CELLS), ("nest", NEST_CELLS), ("va", VA_CELLS)] {
        let entry = t.write(&format!("{group}.chz"), &format!("{prelude}{body}"));
        let out = run_chezzi("run", &entry);
        if !out.status.success() {
            red.push(format!(
                "group {group}: chezzi run failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        got.extend(cells(&String::from_utf8_lossy(&out.stdout)));
    }
    t.write("real_defs.chz", REAL_DEFS);
    let alias = t.write("alias_main.chz", ALIAS_MAIN);
    let out = run_chezzi("run", &alias);
    if !out.status.success() {
        red.push(format!(
            "f32_alias_cross_module: chezzi run failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    got.extend(cells(&String::from_utf8_lossy(&out.stdout)));

    for (cell, w) in &want {
        match got.get(cell) {
            Some(g) if g.to_bits() == w.to_bits() || g == w => {}
            Some(g) => red.push(format!("{cell}: chezzi {g:?}, C {w:?}")),
            None => red.push(format!("{cell}: no chezzi output (C {w:?})")),
        }
    }

    for (name, tail, needle) in REJECTS {
        let src = match tail.strip_prefix('!') {
            Some(bare) => bare.to_string(),
            None => format!("{prelude}{tail}"),
        };
        let entry = t.write(&format!("reject_{name}.chz"), &src);
        let out = run_chezzi("check", &entry);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() || !stderr.contains(needle) {
            red.push(format!(
                "reject {name}: exit {:?}, want stderr containing {needle:?}, got: {stderr}",
                out.status.code()
            ));
        }
    }
    assert!(red.is_empty(), "red cells:\n{}", red.join("\n"));
}
