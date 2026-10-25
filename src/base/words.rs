//! base: message words the checker and the VM both print.

/// How a diagnostic or a fault names an optional value: its kind in plain words, then its type as
/// the user writes it. Never `enum`.
pub const OPTION_VALUE_WORD: &str = "an optional value (`T?`)";

/// How a diagnostic or a fault names a result value.
pub const RESULT_VALUE_WORD: &str = "a result value (`T!E`)";

/// `word` (one of the two constants above) with `written` as its quoted type, for a reader that
/// knows the type the user wrote: ``an optional value (`int?`)``.
pub fn carrier_word_for(word: &str, written: &str) -> String {
    match (word.find('`'), word.rfind('`')) {
        (Some(a), Some(b)) if a < b => format!("{}{written}{}", &word[..=a], &word[b..]),
        _ => word.to_string(),
    }
}

/// D4 layer C (TICKET-169): the one copy of the fault message text, quoted once here so every write
/// site stays byte-identical. `copied_write_err` prefixes it with `this value`; the checker with
/// `'<name>'`.
pub const COPY_WRITE_TAIL: &str = "is this task's copy: a write to it would be lost at the join; share it through Shared/Channel, or make a task-local copy with .copy()";

/// Maximum structural-recursion depth for value display / equality — a cyclic data structure (e.g.
/// a struct with a `List[Self]` field forming a cycle) would otherwise recurse unbounded on the
/// HOST stack and SIGABRT (uncatchable). This bound turns that into a recoverable `RuntimeError`.
///
/// `pub(crate)` so `checker::eq::EQ_BOUNDS_MAX_IN_PROGRESS` can tie itself to this value directly
/// (W7-55 Important 4) instead of repeating the literal — the checker's `Eq`-bound cap and the
/// runtime's own equality depth cap must agree BY CONSTRUCTION, or the checker can grant a compare
/// the VM then can't perform (the `checker-superset-of-compiler` soundness class).
///
/// **Every FAULTING guard on this constant tests [`Vm::walk_base`] `+ depth`, not `depth` — that is
/// the whole of W8-43.** The two guards whose over-budget branch DEGRADES instead of faulting
/// ([`Vm::cyclic_walk`] and [`Vm::snapshot_value`], the map/set key-store pair) stay on the bare
/// `depth`: charging a degrading guard converts a stack-safety measure into a silent wrong answer
/// (an ordinary shallow key inserted from inside a hook running at depth would be stored by
/// reference instead of snapshotted). Only faulting guards may share the budget. The `MAX_CALL_DEPTH` / [`VM_STACK_BYTES`] co-tuning below assumes O(1) native frames
/// per call-depth level. That holds for `run_until`, and for the `hash`/`compare` hooks (one native
/// frame per `run_proto`). It does NOT hold for a structural walk started INSIDE a protocol hook: a
/// user `eq`/`str` that compares or stringifies re-enters the VM, and before `walk_base` existed
/// each re-entry restarted this counter at 0. Neither guard then fired — call depth stayed far below
/// 10 000 while the *product* of hook-nesting depth × per-hook walk depth grew without bound — and
/// checker-clean pure Chezzi killed the process by host stack overflow (rc=134, uncatchable by
/// `recover:`, measured), which is exactly the outcome this constant's first paragraph promises to
/// prevent. `walk_base` restores the O(1)-frames-per-level assumption by making the budget ONE
/// shared allowance across the whole nest, matching CPython's single recursion budget.
pub const MAX_STRUCTURAL_DEPTH: usize = 10_000;

/// M-C implicit nurseries: does this body contain a bare `spawn` that is NOT already inside an
/// explicit `parallel:` (so it would bind to the *implicit* function/module nursery)? Drives the
/// gate in `compile_fn`/`compile_module` — a body with no such spawn emits byte-identical bytecode to
/// pre-M-C (zero overhead). Recurses through control flow but **stops** at boundaries that are *their
/// own* function-like body (and so get their own implicit nursery, gated separately): `parallel:` (its
/// spawns belong to that explicit nursery), nested `fn`, a `spawn:` block, and a `defer:` block (each
/// runs in its own frame, so a bare `spawn` inside it joins at *that* body's end, not this one's).
/// Map an extern fn's surface [`Type`] annotation to its runtime [`CType`]. Only the v1 marshallable
/// set (`int`/`float`/`bool`/`str`/`ptr`) is supported, resolving transparent type aliases (`type Len = int`)
/// through `aliases` first. Everything else (incl. a `None` annotation) returns `None`. The checker
/// ROOT REDESIGN — strip a qualified identity key `<module-key>::Name` back to its bare `Name` for a
/// display fallback when no `StructDef` is registered (defensive). Splits on the LAST `::`.
pub fn bare_display(key: &str) -> String {
    key.rsplit("::").next().unwrap_or(key).to_string()
}

/// The text that closes a tuple's parts before its `)`: `,` for one part, nothing otherwise. The
/// comma is what makes one part a tuple and not a grouped value (S2-7). The one rule for a tuple
/// value, a tuple type, a match witness and an unpacking target list.
pub fn tuple_tail(n: usize) -> &'static str {
    if n == 1 { "," } else { "" }
}

/// A tuple's parts without the parentheses: `a,` for one part, `a, b` otherwise.
pub fn tuple_parts<S: AsRef<str>>(parts: impl IntoIterator<Item = S>) -> String {
    let mut out = String::new();
    let mut n = 0;
    for p in parts {
        if n > 0 {
            out.push_str(", ");
        }
        out.push_str(p.as_ref());
        n += 1;
    }
    out.push_str(tuple_tail(n));
    out
}

/// A tuple in parentheses: `(a,)` for one part, `(a, b)` otherwise.
pub fn tuple_text<S: AsRef<str>>(parts: impl IntoIterator<Item = S>) -> String {
    format!("({})", tuple_parts(parts))
}
