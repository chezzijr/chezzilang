// checker::exhaust — a Maranget usefulness/witness check over a pattern matrix, ORed into
// `has_wildcard` in the three arm loops (`src/checker/pattern.rs`), per DEC-065. Fixes the false
// rejection of a tuple/nested-Option match whose arms cover the full cartesian product: the old
// model (a flat top-level covered-key set + one irrefutability bool) cannot represent a product
// domain or a nested refutable payload. See `## Digest` / `## Decisions` in TICKET-076 for the
// full rationale; this module is intentionally conservative — anything it can't model maps to
// `Pat::Never`/`Dom::Open`, which only ever narrows what this check accepts, never widens it.

use super::*;

/// One constructor domain a column of the pattern matrix ranges over.
#[derive(Clone)]
pub(super) enum Dom {
    /// A closed sum type: enum/`Option`/`Result`. Leading `String` is the DISPLAY PREFIX (`""` for
    /// `Option`/`Result`, `bare_display(label)` for a user enum) — used only when rendering a
    /// witness. Each member is `(variant name, payload types)`, sorted by name for a deterministic
    /// witness.
    Sum(String, Vec<(String, Vec<Ty>)>),
    /// A tuple or struct: exactly one constructor. Leading `String` is the IDENTITY KEY — `""` for
    /// a tuple, else the struct's `label` (what `resolve_struct_ctor` compares against).
    Prod(String, Vec<Ty>),
    /// `bool`: the other closed literal domain.
    Bool,
    /// Anything without a finite, provably-complete constructor set (int/str literal/range, or a
    /// shape this check can't model). Never closed by construction alone.
    Open,
}

/// One column entry of the pattern matrix, lowered from a surface `Pattern`.
#[derive(Clone)]
enum Pat {
    /// Matches every value of its domain.
    Wild,
    /// Matches one constructor with its (already-lowered) sub-columns.
    Ctor(String, Vec<Pat>),
    /// Matches nothing — the sentinel for a shape this check can't model (int/str literal, range,
    /// a binder shape the arm-binder already rejected, or a product that blew `MAX_ROWS`). Dropped
    /// by both specialization and defaulting, so it can never make a column look covered.
    Never,
}

/// A rendered witness — an uncovered value, for the `help` line.
#[derive(Clone)]
enum Wit {
    Wild,
    Ctor(String, Vec<Wit>),
}

/// The accumulated one-column pattern matrix for one `match`.
pub(super) struct ExhCheck {
    rows: Vec<Vec<Pat>>,
    dom: Dom,
    overflow: bool,
}

const MAX_ROWS: usize = 512;
const MAX_DEPTH: usize = 12;

/// Map a matched-on `Ty` to its constructor domain, for a NESTED position (a tuple element or
/// variant/struct payload slot). Mirrors `match_kind`'s top-level classification.
fn dom_of_ty(chk: &Checker, ty: &Ty) -> Dom {
    match ty {
        Ty::Tuple(tys) => Dom::Prod(String::new(), tys.clone()),
        Ty::Struct(name, _) => match chk.struct_fields_of(ty) {
            Some(fields) => Dom::Prod(name.clone(), fields),
            None => Dom::Open,
        },
        Ty::Bool => Dom::Bool,
        _ => match chk.variants_of(ty) {
            Some(vmap) => {
                let prefix = match ty {
                    Ty::Option(_) | Ty::Result(_, _) => String::new(),
                    Ty::Enum(name, _) => crate::compiler::bare_display(name),
                    _ => String::new(),
                };
                let mut members: Vec<(String, Vec<Ty>)> = vmap.into_iter().collect();
                members.sort_by(|a, b| a.0.cmp(&b.0));
                Dom::Sum(prefix, members)
            }
            None => Dom::Open,
        },
    }
}

/// Render one constructor of `dom` for a witness: bare for a tuple/`Dom::Sum` with an empty prefix,
/// `prefix.name` otherwise, `bare_display(label)` for a struct.
fn display_of(dom_prefix_or_label: &str, name: &str) -> String {
    if dom_prefix_or_label.is_empty() {
        name.to_string()
    } else {
        format!("{dom_prefix_or_label}.{name}")
    }
}

/// Render a witness tree into the `help` message's value.
fn render_wit(w: &Wit) -> String {
    match w {
        Wit::Wild => "_".to_string(),
        Wit::Ctor(name, args) if args.is_empty() => name.clone(),
        Wit::Ctor(name, args) if name.is_empty() => {
            format!(
                "({})",
                args.iter().map(render_wit).collect::<Vec<_>>().join(", ")
            )
        }
        Wit::Ctor(name, args) => {
            format!(
                "{name}({})",
                args.iter().map(render_wit).collect::<Vec<_>>().join(", ")
            )
        }
    }
}

impl Checker {
    /// Start a new pattern matrix for a match over `kind`, if this shape can be modelled. Mirrors
    /// `match_kind`'s cases; `Literal`/`Skip` return `None` (see the module-level doc: `bool` is
    /// already closed by `bool_domain_closed`, and `int`/`str` are open domains a matrix can't
    /// close anyway).
    pub(super) fn exh_new(&self, kind: &MatchKind) -> Option<ExhCheck> {
        let dom = match kind {
            MatchKind::Variants { label, variants } => {
                let prefix = if label == "Option" || label == "Result" {
                    String::new()
                } else {
                    crate::compiler::bare_display(label)
                };
                let mut members: Vec<(String, Vec<Ty>)> = variants.clone().into_iter().collect();
                members.sort_by(|a, b| a.0.cmp(&b.0));
                Dom::Sum(prefix, members)
            }
            MatchKind::Tuple(tys) => Dom::Prod(String::new(), tys.clone()),
            MatchKind::Struct { label, fields, .. } => Dom::Prod(label.clone(), fields.clone()),
            MatchKind::Literal(_) | MatchKind::Skip => return None,
        };
        Some(ExhCheck {
            rows: Vec::new(),
            dom,
            overflow: false,
        })
    }

    /// Lower one surface pattern into its matrix row(s) — one per or-alternative — against `dom`.
    fn exh_lower(&self, pattern: &Pattern, dom: &Dom, depth: usize) -> Vec<Pat> {
        if depth > MAX_DEPTH {
            return vec![Pat::Never];
        }
        match pattern {
            Pattern::Wildcard => vec![Pat::Wild],
            Pattern::Ident(name, _) => {
                if let Dom::Sum(_, members) = dom
                    && members.iter().any(|(n, p)| n == name && p.is_empty())
                    && crate::checker::is_builtin_variant(name)
                {
                    return vec![Pat::Ctor(name.clone(), vec![])];
                }
                if self.variant_owners.contains_key(name)
                    || crate::checker::is_builtin_variant(name)
                {
                    vec![Pat::Never]
                } else {
                    vec![Pat::Wild]
                }
            }
            Pattern::Literal(LitPattern::Bool(b)) => {
                if matches!(dom, Dom::Bool) {
                    vec![Pat::Ctor(b.to_string(), vec![])]
                } else {
                    vec![Pat::Never]
                }
            }
            Pattern::Literal(_) | Pattern::Range { .. } => vec![Pat::Never],
            Pattern::Tuple(subs) => match dom {
                Dom::Prod(p, tys) if p.is_empty() && tys.len() == subs.len() => {
                    self.exh_product("", subs, tys, depth)
                }
                _ => vec![Pat::Never],
            },
            Pattern::Variant {
                name,
                bindings,
                enum_name,
                module_name,
            } => match dom {
                Dom::Prod(p, tys) if !p.is_empty() => {
                    if self
                        .resolve_struct_ctor(p, name, enum_name.as_deref(), module_name.as_deref())
                        .is_ok()
                        && tys.len() == bindings.len()
                    {
                        self.exh_product(p, bindings, tys, depth)
                    } else {
                        vec![Pat::Never]
                    }
                }
                Dom::Sum(_, members) => match members.iter().find(|(n, _)| n == name) {
                    Some((_, tys)) if tys.len() == bindings.len() => {
                        self.exh_product(name, bindings, tys, depth)
                    }
                    _ => {
                        if enum_name.is_none()
                            && module_name.is_none()
                            && bindings.is_empty()
                            && !self.variant_owners.contains_key(name)
                            && !crate::checker::is_builtin_variant(name)
                        {
                            vec![Pat::Wild]
                        } else {
                            vec![Pat::Never]
                        }
                    }
                },
                _ => {
                    if enum_name.is_none()
                        && module_name.is_none()
                        && bindings.is_empty()
                        && !self.variant_owners.contains_key(name)
                        && !crate::checker::is_builtin_variant(name)
                    {
                        vec![Pat::Wild]
                    } else {
                        vec![Pat::Never]
                    }
                }
            },
            Pattern::Or(alts) => alts
                .iter()
                .flat_map(|alt| self.exh_lower(alt, dom, depth + 1))
                .collect(),
        }
    }

    /// Build the cartesian product of `subs`' or-alternatives (a tuple/variant/struct's positional
    /// sub-patterns), producing one `Pat::Ctor` row per combination. Bails to a single `Pat::Never`
    /// row if the product would exceed `MAX_ROWS` — never silently truncated as "covered".
    fn exh_product(&self, ctor: &str, subs: &[Pattern], tys: &[Ty], depth: usize) -> Vec<Pat> {
        let mut combos: Vec<Vec<Pat>> = vec![Vec::new()];
        for (sub, ty) in subs.iter().zip(tys.iter()) {
            let sub_dom = dom_of_ty(self, ty);
            let alts = self.exh_lower(sub, &sub_dom, depth + 1);
            let mut next = Vec::new();
            for combo in &combos {
                for alt in &alts {
                    if next.len() >= MAX_ROWS {
                        return vec![Pat::Never];
                    }
                    let mut c = combo.clone();
                    c.push(alt.clone());
                    next.push(c);
                }
            }
            combos = next;
        }
        combos
            .into_iter()
            .map(|args| Pat::Ctor(ctor.to_string(), args))
            .collect()
    }

    /// Add one arm's row(s) to the matrix (no row for a guarded arm — a guard can fail at runtime,
    /// so it never covers), then re-check usefulness. Returns whether the match is now fully
    /// covered (mirrors `has_wildcard`'s per-arm semantics: an arm that closes the domain flips it
    /// true from here on). No-ops (returns `false`) once `e` is `None` or has overflowed.
    pub(super) fn exh_add(
        &self,
        e: &mut Option<ExhCheck>,
        pattern: &Pattern,
        guarded: bool,
    ) -> bool {
        let Some(chk) = e else { return false };
        if chk.overflow {
            return false;
        }
        if !guarded {
            let dom = chk.dom.clone();
            for alt in self.exh_lower(pattern, &dom, 0) {
                if chk.rows.len() >= MAX_ROWS {
                    chk.overflow = true;
                    return false;
                }
                chk.rows.push(vec![alt]);
            }
        }
        self.exh_witness_rec(&chk.rows, std::slice::from_ref(&chk.dom), 0)
            .is_none()
    }

    /// The witness a completely un-matched value would take, or `None` if the current rows already
    /// cover every value of `doms`. `depth > MAX_DEPTH` and an empty `rows` both return
    /// conservatively "covered" (`Some`) rather than risk an unbounded recursion or a false
    /// rejection on a shape this check can't fully explore.
    fn exh_witness_rec(&self, rows: &[Vec<Pat>], doms: &[Dom], depth: usize) -> Option<Vec<Wit>> {
        if depth > MAX_DEPTH || rows.is_empty() {
            return Some(vec![Wit::Wild; doms.len()]);
        }
        let (dom0, rest_doms) = doms.split_first()?;
        let ctors: Vec<(String, Vec<Ty>)> = match dom0 {
            Dom::Sum(_, members) => members.clone(),
            Dom::Prod(label, tys) => vec![(label.clone(), tys.clone())],
            Dom::Bool => vec![("true".to_string(), vec![]), ("false".to_string(), vec![])],
            Dom::Open => vec![],
        };
        let used: std::collections::HashSet<&str> = rows
            .iter()
            .filter_map(|r| match r.first() {
                Some(Pat::Ctor(n, _)) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        let complete = !ctors.is_empty() && ctors.iter().all(|(n, _)| used.contains(n.as_str()));
        if complete {
            for (name, tys) in &ctors {
                let mut sub_doms: Vec<Dom> = tys.iter().map(|t| dom_of_ty(self, t)).collect();
                sub_doms.extend(rest_doms.iter().cloned());
                let specialized = specialize(rows, name, tys.len());
                if let Some(mut w) = self.exh_witness_rec(&specialized, &sub_doms, depth + 1) {
                    let args: Vec<Wit> = w.drain(..tys.len()).collect();
                    let mut out = vec![Wit::Ctor(display_of(dom_label(dom0), name), args)];
                    out.extend(w);
                    return Some(out);
                }
            }
            None
        } else {
            let defaulted = default_matrix(rows);
            let w = self.exh_witness_rec(&defaulted, rest_doms, depth + 1)?;
            let missing = ctors.iter().find(|(n, _)| !used.contains(n.as_str()));
            let head = match missing {
                Some((name, tys)) => Wit::Ctor(
                    display_of(dom_label(dom0), name),
                    vec![Wit::Wild; tys.len()],
                ),
                None => Wit::Wild,
            };
            let mut out = vec![head];
            out.extend(w);
            Some(out)
        }
    }

    /// The current uncovered-value witness, rendered for `help`, or `None` when the match is fully
    /// covered or the check overflowed (never asserts anything in that case).
    pub(super) fn exh_help(&self, e: &Option<ExhCheck>) -> Option<String> {
        let chk = e.as_ref()?;
        if chk.overflow {
            return None;
        }
        let w = self.exh_witness_rec(&chk.rows, std::slice::from_ref(&chk.dom), 0)?;
        let rendered = w.first().map(render_wit)?;
        Some(format!("pattern `{rendered}` is not covered"))
    }
}

/// The display prefix/label of a domain, for `display_of`.
fn dom_label(dom: &Dom) -> &str {
    match dom {
        Dom::Sum(prefix, _) => prefix,
        Dom::Prod(label, _) => label,
        Dom::Bool | Dom::Open => "",
    }
}

/// Specialize the matrix for constructor `ctor` of arity `arity`: a `Wild`-headed row becomes
/// `arity` wildcards (it matches every constructor), a matching `Ctor`'s args are kept, a
/// non-matching `Ctor` row and every `Never` row are dropped.
fn specialize(rows: &[Vec<Pat>], ctor: &str, arity: usize) -> Vec<Vec<Pat>> {
    let mut out = Vec::new();
    for row in rows {
        let Some((head, rest)) = row.split_first() else {
            continue;
        };
        match head {
            Pat::Wild => {
                let mut r = vec![Pat::Wild; arity];
                r.extend(rest.iter().cloned());
                out.push(r);
            }
            Pat::Ctor(n, args) if n == ctor => {
                let mut r = args.clone();
                r.extend(rest.iter().cloned());
                out.push(r);
            }
            Pat::Ctor(_, _) | Pat::Never => {}
        }
    }
    out
}

/// The default matrix: only `Wild`-headed rows, with their head dropped.
fn default_matrix(rows: &[Vec<Pat>]) -> Vec<Vec<Pat>> {
    let mut out = Vec::new();
    for row in rows {
        let Some((head, rest)) = row.split_first() else {
            continue;
        };
        if matches!(head, Pat::Wild) {
            out.push(rest.to_vec());
        }
    }
    out
}
