# Grammar conformance corpus

Annotated `.chz` files that serve as both **executable documentation** of the grammar and the
inputs for the conformance harness in `src/conformance.rs` (run via `cargo test conformance`).

The harness executes the canonical grammar (`docs/grammar.bnf`) with the `bnf` crate and asserts
its accept/reject decision matches the hand-written parser (`src/parser/mod.rs`) on every file here.

## Layout

- `accept/` — valid programs. Both the grammar and the parser must **accept** them.
- `reject/` — invalid programs. Both must **reject** them, and the parser's error message must
  contain the file's `# expect:` substring.

## Annotations (Chezzi `#` comments, so the lexer ignores them)

```
# rule: fnDecl, ifStmt        # which grammar nonterminals this file exercises (accept + reject)
# expect: end of line         # substring the parser's error must contain (reject only)
```

`corpus_covers_the_grammar` checks every `# rule:` names a real grammar rule and that the headline
constructs each have at least one example.

## Not covered here

Deeply-nested inputs (the parser's `MAX_DEPTH` cap) are **excluded** on purpose: that cap is a
parser resource limit, not a grammar rule, so the context-free grammar would *accept* what the
parser rejects — a deliberate, documented divergence. It stays a unit test in
`src/parser/mod.rs` (`deep_nesting_errors_not_crash`).
