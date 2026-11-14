//! Parse every std/**/*.chz at build time and write $OUT_DIR/std_files.rs.
//!
//! The generated file holds the embedded text table, one encoded AST per file (the artifact the
//! resolver decodes instead of lexing and parsing at run time) and the encoded `protocol`
//! statements of all files, plus `PROTOCOL_METHODS`, the method names each `protocol` declares
//! itself, `NATIVE_STRUCT_LAYOUTS`, the field names of each `native struct` that has fields, and
//! `JsonVariant`, the variants of `std/json.chz`'s `enum Json`. `std/` is the one source: a new or
//! edited file reruns this script.

use chezzi_syntax::ast::StmtKind;
use chezzi_syntax::ast::codec;
use chezzi_syntax::parser::parse_module_source;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn collect_chz(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let rel_name = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        if path.is_dir() {
            collect_chz(&path, &rel_name, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("chz") {
            out.push(rel_name);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let std_dir = manifest
        .join("../../std")
        .canonicalize()
        .expect("std/ exists next to crates/");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets it"));
    println!("cargo:rerun-if-changed={}", std_dir.display());

    let mut files = Vec::new();
    collect_chz(&std_dir, "", &mut files);
    files.sort();

    let mut table = String::new();
    let mut asts = String::new();
    let mut protocols = Vec::new();
    let mut protocol_methods = String::new();
    let mut protocol_names = std::collections::HashSet::new();
    let mut layouts = String::new();
    let mut json_variants: Vec<String> = Vec::new();
    let (all_ids, ()) = codec::with_id_range(|| {
        for (i, rel) in files.iter().enumerate() {
            let path = std_dir.join(rel);
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let _ = writeln!(
                table,
                "    ({rel:?}, include_str!({:?})),",
                path.display().to_string()
            );
            let (ids, parsed) =
                codec::with_id_range(|| parse_module_source(&src, i as u32 + 1, &|_: &str| false));
            match parsed {
                Ok((module, words)) => {
                    for stmt in &module.stmts {
                        if let StmtKind::Protocol { name, methods, .. } = &stmt.kind {
                            assert!(
                                protocol_names.insert(name.clone()),
                                "std/{rel}: protocol `{name}` is declared in two std files"
                            );
                            for m in methods {
                                let _ = writeln!(protocol_methods, "    ({name:?}, {:?}),", m.name);
                            }
                            protocols.push((i as u32, stmt.clone()));
                        }
                        if let StmtKind::NativeStruct { name, fields, .. } = &stmt.kind
                            && !fields.is_empty()
                        {
                            let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                            let _ = writeln!(layouts, "    ({name:?}, &{names:?}),");
                        }
                        if let StmtKind::Enum { name, variants, .. } = &stmt.kind
                            && rel == "json.chz"
                            && name == "Json"
                        {
                            json_variants = variants.iter().map(|v| v.name.clone()).collect();
                        }
                    }
                    let bin = out_dir.join(format!("std_ast_{i}.bin"));
                    std::fs::write(&bin, codec::encode(ids, &(module, words)))
                        .expect("write the std artifact");
                    let _ = writeln!(
                        asts,
                        "    Some(include_bytes!({:?})),",
                        bin.display().to_string()
                    );
                }
                Err(_) => {
                    println!("cargo:warning=std/{rel} does not parse; a run parses its text");
                    asts.push_str("    None,\n");
                }
            }
        }
    });
    assert!(
        !json_variants.is_empty(),
        "std/json.chz must declare `enum Json`"
    );
    let json_enum = json_variants.join(", ");
    let json_all: Vec<String> = json_variants
        .iter()
        .map(|v| format!("JsonVariant::{v}"))
        .collect();
    let json_names: String = json_variants
        .iter()
        .map(|v| format!("            JsonVariant::{v} => {v:?},\n"))
        .collect();
    let bin = out_dir.join("std_protocols.bin");
    std::fs::write(&bin, codec::encode(all_ids, &protocols)).expect("write the protocol artifact");

    let generated = format!(
        "/// Every `std/**/*.chz`, keyed by its path relative to `std/` (forward slashes, `.chz` included).\n\
         pub const STD_FILES: &[(&str, &str)] = &[\n{table}];\n\n\
         /// The encoded AST of each [`STD_FILES`] entry, in the same order; `None` when the file does not parse.\n\
         pub const STD_ASTS: &[Option<&[u8]>] = &[\n{asts}];\n\n\
         /// The encoded `protocol` statements of all [`STD_FILES`], each with its file's index.\n\
         pub const STD_PROTOCOLS: &[u8] = include_bytes!({:?});\n\n\
         /// One row per method a std `protocol` declares itself, `(protocol, method)`, in decl order:\n\
         /// the owner of every protocol method name (`Hook::method` reads it).\n\
         pub const PROTOCOL_METHODS: &[(&str, &str)] = &[\n{protocol_methods}];\n\n\
         /// One row per `native struct` that declares fields, `(struct, field names in decl order)`:\n\
         /// the runtime layout of every native struct value a native fn builds.\n\
         pub const NATIVE_STRUCT_LAYOUTS: &[(&str, &[&str])] = &[\n{layouts}];\n\n\
         /// The variants of `std/json.chz`'s `enum Json`, in decl order.\n\
         #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n\
         pub enum JsonVariant {{ {json_enum} }}\n\n\
         impl JsonVariant {{\n\
         \x20   /// Every variant, in decl order.\n\
         \x20   pub const ALL: &[JsonVariant] = &[{}];\n\n\
         \x20   /// The variant's name as `std/json.chz` spells it.\n\
         \x20   pub fn name(self) -> &'static str {{\n\
         \x20       match self {{\n{json_names}        }}\n\
         \x20   }}\n\n\
         \x20   /// The variant named `name`, if `enum Json` declares one.\n\
         \x20   pub fn from_name(name: &str) -> Option<JsonVariant> {{\n\
         \x20       Self::ALL.iter().copied().find(|v| v.name() == name)\n\
         \x20   }}\n\
         }}\n",
        bin.display().to_string(),
        json_all.join(", ")
    );
    std::fs::write(out_dir.join("std_files.rs"), generated).expect("write std_files.rs");
}
