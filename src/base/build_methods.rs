//! Build script of chezzi-base: reads every `native struct` / `native enum` with methods in `std/**/*.chz`
//! with the real parser and writes `$OUT_DIR/builtin_methods.rs` — one `<Type>Method` enum per type
//! (its instance methods), the `NativeStatic` enum (every static method, one variant per type and
//! method) and the `DECLARED_METHODS` table. `std/*.chz` is the one owner of a builtin type's method set.

use chezzi_syntax::ast::{FnDecl, NativeDecl, StmtKind};
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

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let std_dir = manifest.join("../../std");
    println!("cargo:rerun-if-changed={}", std_dir.display());
    let mut files = Vec::new();
    chz_files(&std_dir, &mut files);
    let mut rows: Vec<Row> = Vec::new();
    for path in &files {
        let src =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let toks =
            lexer::tokenize(&src).unwrap_or_else(|e| panic!("{}: lex error {e:?}", path.display()));
        let module =
            parser::parse(toks).unwrap_or_else(|e| panic!("{}: parse error {e:?}", path.display()));
        for stmt in &module.stmts {
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
    for (v, t, n, _) in &statics {
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
    for (v, t, n, _) in &statics {
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
    let dest = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("builtin_methods.rs");
    std::fs::write(&dest, out).unwrap_or_else(|e| panic!("{}: {e}", dest.display()));
}
