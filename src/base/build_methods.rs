//! Build script of chezzi-base: reads every `native struct` / `native enum` with methods in `std/**/*.chz`
//! with the real parser and writes `$OUT_DIR/builtin_methods.rs` — one `<Type>Method` enum per type
//! (its instance methods), the `NativeStatic` enum (every static method, one variant per type and
//! method) and the `DECLARED_METHODS` table. `std/*.chz` is the one owner of a builtin type's method set.

use chezzi_syntax::ast::{ExprKind, FnDecl, NativeDecl, Param, StmtKind, Type, UnaryOp};
use chezzi_syntax::{lexer, parser};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn chz_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|e| e.expect("directory entry").path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            chz_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "chz") {
            out.push(p);
        }
    }
}

/// `starts_with` -> `StartsWith`; a leading `_` (a std internal) becomes the prefix `Std`.
fn variant_name(method: &str) -> String {
    let mut out = String::new();
    if method.starts_with('_') {
        out.push_str("Std");
    }
    for part in method.split('_').filter(|p| !p.is_empty()) {
        let mut cs = part.chars();
        if let Some(c) = cs.next() {
            out.extend(c.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    out
}

fn enum_name(ty: &str) -> String {
    let mut cs = ty.chars();
    let first = cs.next().expect("type name");
    format!("{}{}Method", first.to_uppercase(), cs.as_str())
}

struct Row {
    ty: String,
    /// Every bodyless method, instance and static, in declaration order.
    bodyless: Vec<String>,
    /// The bodyless methods that take `self`.
    instance: Vec<String>,
    /// The bodyless methods that take no `self`, reached through the type (`List.from_iter`), with
    /// their parameter count.
    statics: Vec<(String, usize)>,
    bodied: Vec<String>,
}

fn row(ty: &str, methods: &[NativeDecl], bodied: &[FnDecl]) -> Option<Row> {
    if methods.is_empty() && bodied.is_empty() {
        return None;
    }
    let takes_self = |m: &&NativeDecl| m.params.first().is_some_and(|p| p.is_self());
    Some(Row {
        ty: ty.to_string(),
        bodyless: methods.iter().map(|m| m.name.clone()).collect(),
        instance: methods
            .iter()
            .filter(takes_self)
            .map(|m| m.name.clone())
            .collect(),
        statics: methods
            .iter()
            .filter(|m| !takes_self(m))
            .map(|m| (m.name.clone(), m.params.len()))
            .collect(),
        bodied: bodied.iter().map(|m| m.name.clone()).collect(),
    })
}

/// The Rust spelling of a native parameter's default: a literal, or a negated number, that fits the
/// parameter's declared type (`int`, `float`, `bool`, `str`, or `T?` for `None` and for a literal of
/// `T`). Anything else fails the build: the checker fills a call from this table and types nothing
/// else about a native default.
fn default_literal(path: &Path, decl: &str, p: &Param) -> String {
    let fail = |why: &str| -> ! {
        panic!(
            "{}: `{decl}`, parameter `{}`: {why}",
            path.display(),
            p.name
        )
    };
    let e = p.default.as_ref().expect("a default");
    let (neg, lit) = if let ExprKind::Unary {
        op: UnaryOp::Neg,
        expr,
    } = &e.kind
    {
        ("-", &expr.kind)
    } else {
        ("", &e.kind)
    };
    // The declared base type and whether it is optional (`T?` parses to the private `Option`).
    let (base, optional): (Option<&str>, bool) = match &p.ty {
        None => (None, true),
        Some(t) => {
            if let Type::Named { name, .. } = t {
                (Some(name.as_str()), false)
            } else if let Type::Generic(head, args, _) = t
                && head.ends_with("Option")
                && let [Type::Named { name, .. }] = &args[..]
            {
                (Some(name.as_str()), true)
            } else {
                fail("a native default needs an `int`, `float`, `bool`, `str` or `T?` parameter")
            }
        }
    };
    let fits = |want: &str| base.is_none_or(|b| b == want);
    if let ExprKind::Int(n) = lit
        && fits("int")
    {
        return format!("DeclDefault::Int({neg}{n})");
    }
    if let ExprKind::Float(x) = lit
        && fits("float")
    {
        return format!("DeclDefault::Float({neg}{x:?})");
    }
    if neg.is_empty() {
        if let ExprKind::Bool(b) = lit
            && fits("bool")
        {
            return format!("DeclDefault::Bool({b})");
        }
        if let ExprKind::Str(s) = lit
            && fits("str")
        {
            return format!("DeclDefault::Str({s:?})");
        }
        if let ExprKind::NoneLit = lit
            && optional
        {
            return "DeclDefault::None".to_string();
        }
    }
    fail("a native default must be a literal of the parameter's type")
}

/// `DeclDefaults { .. }` for `decl`'s trailing defaulted parameters (`skip` drops a `self`).
fn decl_defaults(path: &Path, decl: &NativeDecl, skip: usize) -> String {
    let params = &decl.params[skip..];
    let min = chezzi_syntax::ast::min_callable_params(params, false);
    let values: Vec<String> = params[min..]
        .iter()
        .map(|p| {
            if p.default.is_none() {
                panic!(
                    "{}: `{}`: a default after the first",
                    path.display(),
                    decl.name
                );
            }
            default_literal(path, &decl.name, p)
        })
        .collect();
    format!(
        "DeclDefaults {{ min: {min}, values: &[{}] }}",
        values.join(", ")
    )
}

/// One `NATIVE_DEFAULTS` row for `decl` when a parameter has a default: (owner, name, parameter
/// names without `self`, defaults). `owner` is the native module (`std.io`) of a module-level fn,
/// or the type of a method or static.
fn defaults_row(
    path: &Path,
    owner: &str,
    decl: &NativeDecl,
) -> Option<(String, String, Vec<String>, String)> {
    if decl.params.iter().all(|p| p.default.is_none()) {
        return None;
    }
    let skip = usize::from(decl.params.first().is_some_and(|p| p.is_self()));
    let names = decl.params[skip..].iter().map(|p| p.name.clone()).collect();
    Some((
        owner.to_string(),
        decl.name.clone(),
        names,
        decl_defaults(path, decl, skip),
    ))
}

/// `std/a/b.chz` -> `std.a.b`, the name `native::native_members` takes.
fn module_name(std_dir: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(std_dir).expect("a std file");
    let stem = rel.with_extension("");
    let parts: Vec<String> = stem
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    format!("std.{}", parts.join("."))
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let std_dir = manifest.join("../../std");
    println!("cargo:rerun-if-changed={}", std_dir.display());
    let mut files = Vec::new();
    chz_files(&std_dir, &mut files);
    let mut rows: Vec<Row> = Vec::new();
    // (owner, native, its parameter names, its `DeclDefaults`) for each native decl with a default.
    let mut defaults: Vec<(String, String, Vec<String>, String)> = Vec::new();
    for path in &files {
        let src =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let toks =
            lexer::tokenize(&src).unwrap_or_else(|e| panic!("{}: lex error {e:?}", path.display()));
        let module =
            parser::parse(toks).unwrap_or_else(|e| panic!("{}: parse error {e:?}", path.display()));
        for stmt in &module.stmts {
            if let StmtKind::Native(decl) = &stmt.kind {
                defaults.extend(defaults_row(path, &module_name(&std_dir, path), decl));
            }
            if let StmtKind::NativeStruct { name, methods, .. }
            | StmtKind::NativeEnum { name, methods, .. } = &stmt.kind
            {
                for m in methods {
                    defaults.extend(defaults_row(path, name, m));
                }
            }
            let found = match &stmt.kind {
                StmtKind::NativeStruct {
                    name,
                    methods,
                    bodied_methods,
                    ..
                } => row(name, methods, bodied_methods),
                StmtKind::NativeEnum { name, methods, .. } => row(name, methods, &[]),
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(..)
                | StmtKind::Struct { .. }
                | StmtKind::Protocol { .. }
                | StmtKind::Enum { .. }
                | StmtKind::TypeAlias { .. }
                | StmtKind::If { .. }
                | StmtKind::For { .. }
                | StmtKind::While { .. }
                | StmtKind::Match { .. }
                | StmtKind::Return(..)
                | StmtKind::Yield(..)
                | StmtKind::Defer(..)
                | StmtKind::Parallel { .. }
                | StmtKind::Spawn(..)
                | StmtKind::Wait { .. }
                | StmtKind::Break
                | StmtKind::Continue
                | StmtKind::Pass
                | StmtKind::Import(..)
                | StmtKind::Extern { .. }
                | StmtKind::Native(..)
                | StmtKind::NativeType { .. }
                | StmtKind::Assert { .. }
                | StmtKind::Expr(..) => None,
            };
            if let Some(r) = found {
                assert!(
                    !rows.iter().any(|o| o.ty == r.ty),
                    "{}: builtin type `{}` is declared with methods twice",
                    path.display(),
                    r.ty
                );
                rows.push(r);
            }
        }
    }
    let mut out = String::new();
    for r in &rows {
        let en = enum_name(&r.ty);
        // A bodied method compiles to bytecode, so only the bodyless names have a VM arm.
        if r.instance.is_empty() {
            continue;
        }
        let mut variants: Vec<(String, &String)> = Vec::new();
        for n in &r.instance {
            let v = variant_name(n);
            assert!(
                !variants.iter().any(|(o, _)| *o == v),
                "type `{}`: two methods map to variant `{v}`",
                r.ty
            );
            variants.push((v, n));
        }
        let _ = writeln!(
            out,
            "/// The methods of `{}`, one variant per bodyless `native fn` declared in `std/`.",
            r.ty
        );
        let _ = writeln!(out, "#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]");
        let _ = writeln!(out, "pub enum {en} {{");
        for (v, _) in &variants {
            let _ = writeln!(out, "    {v},");
        }
        let _ = writeln!(out, "}}");
        let _ = writeln!(out, "impl {en} {{");
        let _ = writeln!(out, "    pub const ALL: &'static [Self] = &[");
        for (v, _) in &variants {
            let _ = writeln!(out, "        Self::{v},");
        }
        let _ = writeln!(out, "    ];");
        let _ = writeln!(out, "    pub fn from_name(name: &str) -> Option<Self> {{");
        let _ = writeln!(out, "        match name {{");
        for (v, n) in &variants {
            let _ = writeln!(out, "            {n:?} => Some(Self::{v}),");
        }
        let _ = writeln!(out, "            _ => None,");
        let _ = writeln!(out, "        }}");
        let _ = writeln!(out, "    }}");
        let _ = writeln!(out, "    pub fn name(self) -> &'static str {{");
        let _ = writeln!(out, "        match self {{");
        for (v, n) in &variants {
            let _ = writeln!(out, "            Self::{v} => {n:?},");
        }
        let _ = writeln!(out, "        }}");
        let _ = writeln!(out, "    }}");
        let _ = writeln!(out, "}}");
    }
    // The static methods: one variant per (type, method), `List.from_iter` -> `ListFromIter`.
    let mut statics: Vec<(String, &String, &String, usize)> = Vec::new();
    for r in &rows {
        for (n, arity) in &r.statics {
            let v = format!("{}{}", variant_name(&r.ty), variant_name(n));
            assert!(
                !statics.iter().any(|(o, ..)| *o == v),
                "two static methods map to variant `{v}`"
            );
            statics.push((v, &r.ty, n, *arity));
        }
    }
    let _ = writeln!(
        out,
        "/// The static methods of the builtin types, one variant per bodyless `native fn` with no `self`."
    );
    let _ = writeln!(out, "#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]");
    let _ = writeln!(out, "pub enum NativeStatic {{");
    for (v, ..) in &statics {
        let _ = writeln!(out, "    {v},");
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out, "impl NativeStatic {{");
    let _ = writeln!(out, "    pub const ALL: &'static [Self] = &[");
    for (v, ..) in &statics {
        let _ = writeln!(out, "        Self::{v},");
    }
    let _ = writeln!(out, "    ];");
    let _ = writeln!(
        out,
        "    pub fn from_key(ty: &str, method: &str) -> Option<Self> {{"
    );
    let _ = writeln!(out, "        match (ty, method) {{");
    for (v, t, n, ..) in &statics {
        let _ = writeln!(out, "            ({t:?}, {n:?}) => Some(Self::{v}),");
    }
    let _ = writeln!(out, "            _ => None,");
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "    /// The declaring type and the method name.");
    let _ = writeln!(
        out,
        "    pub fn key(self) -> (&'static str, &'static str) {{"
    );
    let _ = writeln!(out, "        match self {{");
    for (v, t, n, ..) in &statics {
        let _ = writeln!(out, "            Self::{v} => ({t:?}, {n:?}),");
    }
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "    /// The number of parameters the decl declares.");
    let _ = writeln!(out, "    pub fn arity(self) -> usize {{");
    let _ = writeln!(out, "        match self {{");
    for (v, _, _, a) in &statics {
        let _ = writeln!(out, "            Self::{v} => {a},");
    }
    let _ = writeln!(out, "        }}");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
    let _ = writeln!(
        out,
        "/// One row per builtin type with methods: (type name, bodyless method names, bodied method names), in declaration order."
    );
    let _ = writeln!(
        out,
        "pub const DECLARED_METHODS: &[(&str, &[&str], &[&str])] = &["
    );
    for r in &rows {
        let _ = writeln!(out, "    ({:?}, &{:?}, &{:?}),", r.ty, r.bodyless, r.bodied);
    }
    let _ = writeln!(out, "];");
    // The checker finds a row by (name, parameter names), so that pair names one decl.
    for (i, (o, n, ps, _)) in defaults.iter().enumerate() {
        assert!(
            !defaults[..i]
                .iter()
                .any(|(_, n2, ps2, _)| n2 == n && ps2 == ps),
            "two native decls with defaults share the name `{n}` and parameters {ps:?} (`{o}`): give one a distinct parameter name"
        );
    }
    let _ = writeln!(
        out,
        "/// (owner, native, parameter names without `self`, the defaults its decl declares), for each native decl with a default. The owner is the native module of a module-level fn, else the type."
    );
    let _ = writeln!(
        out,
        "pub const NATIVE_DEFAULTS: &[(&str, &str, &[&str], DeclDefaults)] = &["
    );
    for (o, n, ps, d) in &defaults {
        let _ = writeln!(out, "    ({o:?}, {n:?}, &{ps:?}, {d}),");
    }
    let _ = writeln!(out, "];");
    let dest = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("builtin_methods.rs");
    std::fs::write(&dest, out).unwrap_or_else(|e| panic!("{}: {e}", dest.display()));
}
