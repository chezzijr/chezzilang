# Special cases in the language — audit of 2026-10-11

The owner's rule: the language stays consistent. A special case is a place where one construct, type or
name gets its own rule instead of the general one. Each special case is a future bug: the next feature is
added beside it, and the two rules drift apart. Bug-hunt wave 23 found five P0 that were exactly this
(`docs/history/bug-hunts/w23.md`).

This document is the map. It was built by six agents on main at `32808a80`: three syntax hunters (layout,
token roles, grammar against parser) and three auditors (builtin types against protocols, constructs legal
only in some positions, names and types hard-coded in the implementation). About 4,000 programs were run.
Every item below is a measured result, and the monitor re-ran the ones marked ✔. Ancestor output comes
from a program that was run (CPython 3.14, rustc, go).

Nothing in this document is filed as a ticket unless the "Status" column names one.

Classes used below:

| Class | Meaning |
|---|---|
| A | documented as deliberate, with a reason |
| B | a restriction with no stated reason |
| C | inconsistent: the rule is not applied uniformly, or the diagnostic is false, or check passes and the run faults |
| D | differs from the owning ancestor and is not documented |

## The pattern

One shape explains most of the map:

> A capability ("can this be iterated", "can this be added", "is this the receiver", "does the next line
> continue this expression") is decided by a hand-written list in each place that needs it. The lists were
> written at different times and name different members.

Measured size of that shape in the source:

| What | Count |
|---|---|
| tests of a user-facing name by string (`== "push"`, `"str" =>`) in non-test code | 509 (checker 229, vm 226, native 33, compiler 20, parser 1) |
| `match` arms on a builtin type in the checker | 888 |
| capabilities decided in more than one place, with the places disagreeing | 11 of 15 (iterable, indexable, index-assign, sliceable, membership, equatable, stringable, addable, "has method m", callable as a value, copyable) |
| capabilities with one decider | 4 (orderable, hashable, truthy, sendable) |

## Unifying rules

Each rule below removes a cluster of special cases. "Decision" says whether the owner has to choose
language behaviour before a ticket can be written.

| # | Rule | Removes | Status | Decision needed |
|---|---|---|---|---|
| U1 | A dedent ends a block expression; an `else` belongs to the statement whose header starts at its column | Family A of wave 23, S1-1, S1-2 | TICKET-249 | no (owner: indentation decides) |
| U2 | A hook is the method that satisfies the builtin protocol; a declared type with methods is one kind (struct and enum alike) | Family D of wave 23, enum half-hooks, `List(s)` for a bounded parameter | TICKET-250 | no |
| U3 | A receiver is the first parameter of a method and nothing else | the `self` universal cast (P0) | TICKET-250 (note added) | no (owner: `self` is the special word) |
| U4 | An un-hinted `?x` is a carrier from creation; a wrapper holds its operand | Families B and C of wave 23 | TICKET-248, TICKET-247 | no |
| U5 | An operator accepts a type exactly when the type satisfies the operator's protocol; builtin types declare their protocols like user types | `in`/`Contains`, `+`/`Add`, `==`/`Eq`, display/`Stringable`, sibling-type holes | not filed | yes: see "Builtin types against protocols" |
| U6 | One iteration source: `for`, comprehensions, `List()`/`Set()`/`Map()`, destructuring and `Iterable` ask one function | range, channel, Set of tuples, std containers, `std.iter` | not filed | yes: the owner is designing ranges, slices and channel iteration |
| U7 | A range is a value | every "a range is only valid as" site, `range()`, slice spelling | not filed | yes: same design as U6 |
| U8 | A block expression is an expression wherever layout exists, and layout exists inside brackets and interpolation holes | `match`/`recover:` rejected inside brackets, parameter default against field default, inline fn body against lambda body, line breaks in a hole | not filed | yes |
| U9 | One pattern grammar at every binding site; a bare tuple is a tuple wherever an expression list ends a statement | destructuring differences, `return a, b`, 1-tuple types | not filed | yes |
| U10 | A function is a function: one parameter-list grammar and one "is a value" rule for every callable | defaults/variadics/generics by function kind, constructors and builtins as values | not filed | yes |
| U11 | A boundary block (`spawn:`, `defer:`, `recover:`, generator body) states once what may cross it | six separate checks, three message families, `?` silently dropped in `defer:` | not filed | small |
| U12 | The string / hole seam has one owner: the hole scanner sees what was written, not what escapes produced | escapes live as syntax, nested literals unescaped twice | not filed | no |
| U13 | One name-resolution rule and one reserved set | user type named like a builtin, std import against user import, "unknown name 'str'" | not filed (wave 16 "Names" family) | small |
| U17 | `head[X]` has one reading, decided once | exponential check on `.m[T](../..)` chains, `t.0[i](5)` | not filed | no |
| U18 | Syntax-tree walkers are exhaustive (no wildcard arm) | generator ban bypass through `assert` | not filed | no |
| U14 | A physical line has one definition | CRLF blank line, CRLF in strings, tab on a blank line | not filed | no |
| U15 | Derive rules are stated once | hashable derivation, compound assignment in place, `for a, b in map` by static type | not filed | yes |
| U16 | Collection method conventions | `contains` against `has`, `remove` return shapes, miss behaviour | not filed | yes |

## Builtin types against protocols (U5, U15, U16)

The operator has a hand list of builtin types; the bound has a different one.

| Operator works ✔ | Matching bound fails ✔ | Sites |
|---|---|---|
| `2 in [1, 2]`, also Map, Set, str | `type List[int] does not satisfy Contains` (all four) | `src/checker/pattern.rs:4411-4483`, `sig.rs:5303`; no `Contains` arm in `proto.rs:1833` |
| `"a" + "b"`, `[1] + [2]`, `{1, 2} - {2}` | `type str does not satisfy Add`; List; `Set` and `Sub`. Set `\| & ^` have no protocol | `pattern.rs:4090`, `:4207`; `proto.rs:2222` |
| `print(x)`, `str(x)`, `"{x}"` for every type | `Stringable` accepts int, float, bool, str only | `proto.rs:2048`; `docs/reference/spec.md:957` says "anything" |
| `ch == ch` for Channel, Shared, RwShared, Atomic, Iterator; `[ch].contains(ch)` | `type Channel[int] does not satisfy Eq` | `proto.rs:1160`, `:1184` against `:2029` |
| `[1].iter()`, every struct's `.copy()`, `bytearray.extend` | a user protocol requiring that method rejects the type: `missing method 'iter'` | `expr.rs:2719`, `:3105`, `:3384` sit outside the method table |

Sibling types that differ with no stated reason:

| Works | Does not | Ancestor |
|---|---|---|
| `"a" + "b"`, `"a" < "b"`, `"b" in "abc"` | `b"a" + b"b"`, `b"a" < b"b"`, `98 in b"abc"` | CPython: all work |
| `[0] * 3` | `"ab" * 3` (`.repeat(3)` exists) | CPython `ababab` |
| `1 == 1.0`, `[1] == [1.0]` | `1 in [1.0]`, `[1.0].contains(1)`, `[1] < [1.5]` | CPython: all `True` |
| tuple and zero-field struct as a map key | enum without payload, struct with fields, `int?` | CPython: enum members and `None` are keys; Rust derives `Hash` |
| `int? < int?` | `int!str < int!str`; `false < true` | Rust orders both |
| struct has automatic `.copy()` | enum has none | Rust derives `Clone` on both |
| `for a, b in [(1, "x")]` | `for a, b in {(1, "x")}` | CPython works |
| `iter.islice({1, 2}, 1)`, `[1.5].sum()` | `iter.enumerate({1, 2})`, `iter.sum([1.5])`, `{1, 2}.map(f)`, `xs.extend({2})`, `",".join({"a"})` | CPython works |
| `bytearray()`, `bytearray(2)` | `bytes()`, `bytes(2)`, `bytes({1, 2})` | CPython works |
| `match` literal patterns for int, str, bool, tuple | float, bytes | CPython `case 1.5:` |

Hook spelling (U16): List and str have `.contains`, Map and Set have `.has`, ConcurrentMap and
ConcurrentSet have `.contains`. `remove` returns `V?` on Map, `bool` on Set, nothing on the concurrent
collections. A miss is `None` for `get`/`pop`/`first`, `-1` for `index_of`, a fault for `xs[i]`.
`List.reverse()` mutates; `str.reverse()` returns a new value. `count` has three meanings. No collection
has `clear`.

Derive rules (U15): `xs += [3]` mutates the list in place and an alias sees it; `s |= {3}`, `s &= ..`,
`s -= ..`, `xs *= 2` rebind and an alias does not (DEC-015, DEC-032; CPython mutates in every case).
`for a, b in m` over a `Map[(int, int), str]` gives `(1, 2) x` when the static type is a Map and `1 2`
when the same value is reached through `Iterable[(int, int)]` (DEC-113).

Consistent today: a condition is `bool` for every type (one site, `expr.rs:4483`). Indexing: the operator
and the bound agree in every cell except the range literal. Ordering: one answer everywhere.

Not in the language for any type, so not a special case: `//`, `**`, unary `+`, `~`, slice assignment,
`del`.

## Iteration sources and ranges (U6, U7)

| Source | `for` | comprehension | `List(v)` | `S: Iterable[T]` | `.iter()` | `in` | `.len()` |
|---|---|---|---|---|---|---|---|
| List | ok | ok | ok | ok | ok | ok | ok |
| Map, Set, str | ok | ok | ok | ok | ok | ok | ok |
| bytes | ok | ok | ok | ok | ok | no | ok |
| range literal `a..b` | ok | ok | no | no | no | no | no |
| `range(a, b)` (a List) | ok | ok | ok | ok | ok | ok | ok |
| Channel | ok | no | no | no | no | no | ok |
| generator / cursor | ok | ok | ok | ok | ok | no | no |
| struct with `iter` only | ok | ok | no | ok | ok | no | no |
| struct with `next` | ok | ok | ok | ok | no | no | no |
| enum with `iter` | no | no | no | no | — | — | — |
| tuple | no | no | no | no | no | no | no |
| Deque, Heap, Counter, ConcurrentList/Map/Set, `io.Reader` | no | no | no | no | no | some | some |

Four functions decide "what does this yield": `for_bindings` (`src/checker/sig.rs:5568`), `iter_elem`
(`:5212`), `iterable_elem` (`:5200`) and the `Iterable` arm of `satisfies_args_d` (`proto.rs:2109`), plus
a comprehension veto for channels (`pattern.rs:3885`) and a runtime switch in the compiler
(`src/compiler/mod.rs:2431`, `:2491-2527`).

Three spellings do one job:

| | `a..b` | `range(a, b, s)` | `x[a:b:c]` |
|---|---|---|---|
| is a value | no | yes, an eager `List[int]` | result yes |
| lazy | yes | no | — |
| step, descending | no (`3..0` yields nothing) | yes | yes |
| open ends | no | no | yes |
| usable as a slice spec (`xs[1..3]`) | no | no | — |
| sliceable | yes, `(0..10)[2:4]` | yes | yes |
| indexable | no, `(0..10)[2]` | yes | yes |
| `match` pattern | yes, int literals only | no | no |
| the name is a value | — | no (`unknown name 'range'`) | — |

Also: no `a..=b`, no open range, no step; `0..1<<2` parses as `(0..1)<<2` (Rust: `0..4`); the diagnostic
says "as a slice receiver" and rejects `xs[1..3]`; `docs/reference/syntax.md:139` lists `range` among what
`List(it)` accepts and `:1564` says it is not a value. Rust's `0..3` is a value with `.len()`, and
`&xs[1..3]` slices. Python's `range(0, 3)` is a lazy value with `in`, indexing, `len` and slicing.

The owner is designing one rule for range values, slices and channel iteration. No ticket patches these
until that design exists.

## Where a construct may appear (U8, U9, U10, U11)

Block and colon expressions by position ("no" is a parse or type error):

| Position | one-line `if` | `match` | `recover:` | lambda | range |
|---|---|---|---|---|---|
| `x := E`, typed let, `return E`, `yield E` | ok | ok | ok | ok | no |
| struct field default | ok | ok | ok | ok | no |
| parameter default | ok | no | no | ok | no |
| function argument | ok | no | ok | ok | no |
| list, tuple, map element | ok | no | no | ok | no |
| `(E)` | ok | no | ok | ok | no |
| lambda body | ok | ok | ok | ok | no |
| inline fn body `fn f(): E` | ok | no | ok | ok | no |
| inline `if c: E` body | no | no | ok | ok | no |
| interpolation hole | ok | no (`format spec: unknown type char`) | no | no | no |
| `for` iterable, comprehension source | — | — | — | — | ok |

Three cells stand out: a field default accepts what a parameter default rejects; a lambda body accepts
`match` and an inline fn body does not; a match-expression arm takes an expression only (Rust arms take
blocks and `return`).

Function kinds:

| Feature | top fn | nested fn | method | lambda | protocol signature | struct ctor | enum variant ctor | native method |
|---|---|---|---|---|---|---|---|---|
| defaults | ok | ok | ok | no | no | ok | no | n/a |
| variadic | ok | ok | ok | no | no | no | no | `print` only |
| named arguments at the call | ok | ok | ok | ok | ok | ok | no | no |
| own generic parameters | ok | no | ok | no | no | on the type | on the type | ok |
| usable as a value | ok | ok | `T.m` | ok | no | **no** | **yes** | no |
| statement body, `yield` | ok | ok | ok | no | — | — | — | — |
| self-recursion | ok | ok | ok | only when the binding is global | — | — | — | — |
| redefinition | error | allowed, last wins | — | — | — | — | — | — |

Callable names as values: `[1, 2].map(E.V)` runs; `[1, 2].map(P)` for a struct says "constructors are not
values" (Rust and CPython accept). `xs.map(str)`, `f := int`, `g := range`, `List.len(xs)` say
`unknown name`. `p := print` becomes a fixed one-argument function (`p(1, 2)` is an error), while a user
variadic keeps its shape as a value.

Destructuring by binding site:

| Form | `:=` | `for` | `match` | fn / lambda parameter | `wait` arm |
|---|---|---|---|---|---|
| flat `a, b` | ok | ok | `(a, b)` | no | no |
| nested `a, (b, c)` | no | no | ok | no | no |
| struct or enum payload | no | no | ok | no | no |
| list `[a, b]`, rest `*r` | no | no | no | no | no |

A bare tuple: `a, b := 1, 2` and `a, b = b, a` work; `x := 1, 2`, `return a, b` ✔, `yield a, b`,
`for x in 1, 2` do not (CPython and Go accept `return a, b`). A 1-tuple `(1,)` can be built but its type
`(int,)` and its pattern `(a,)` do not parse, and the type prints as `(int)`.

`Self` is a type only: `-> Self` works, `Self(1)`, `Self.mk()` and a pattern `Self.A` do not (Rust accepts
all three).

Boundary blocks: what may cross `spawn:`, `defer:`, `recover:` and a generator body is decided by six or
more separate checks with three message families. One hole: `?` inside a `defer:` block is accepted in
any function and silently abandons the rest of the block (`docs/reference/syntax.md:3923` documents the discard,
`:4001` says it is rejected). A generator body rejects `defer`, `spawn`, `parallel:`, `wait:` as
"experimental" with no stated reason.

Top level against a function body: a recursive lambda works as a global and is `unknown name` as a local;
a nested `fn` may be redefined silently and a top-level one may not. The other differences (`?`, `return`,
hoisting, one storage slot per name) are documented.

## Parser and lexer (U1, U8, U12, U14)

| ID | Sev | Finding | Chezzi | Ancestor | Status |
|---|---|---|---|---|---|
| S1-1 ✔ | P0 | inline `wait:` arm ending in a block expression takes the `wait`'s `else:` as its guard | prints `1`; false `deadlock` | Go `3` | TICKET-249 (note) |
| S1-2 ✔ | P2 | `if c: return match ...` then `else:` | false error "an `else` guard belongs on a `:=` binding" | Rust `10 20 3` | TICKET-249 (note) |
| S2-8 ✔ | P0 | a parameter named `self` needs no type and is unchecked: `fn cast(self) -> int: self`, `x: int = cast(2.5)` | check OK; prints `2.5 3.5 1.25 false` | rustc: `self` only in associated functions | TICKET-250 (note) |
| S2-1 ✔ | P0/P1 | escapes are resolved before the hole scan: `"\u{7B}x\u{7D}"` interpolates; a literal nested in a hole is unescaped twice | `"{'\\n'.len()}"` prints `1`; `"{line.replace('\\n', ' ')}"` replaces nothing | CPython `{x}`, `2`, `a b` | not filed |
| S1-5, S2-2 ✔ | P2 | a line break between operands inside a hole | `unexpected end of line in expression` | CPython `6` | not filed |
| S1-3 ✔ | P1 | CRLF file with a blank line inside a block | `unexpected indented block in expression` | CPython runs | not filed |
| S1-4 ✔ | P2 | CRLF inside a string literal keeps `\r` | `"a⏎b".len()` is `4` | CPython `3` | not filed |
| S2-3 ✔ | P2 | postfix `?` lost to maximal munch: `oo(x)??`, `o(x)?..5` | parse error; `oo(x)?? - 1` parses as coalesce | Rust runs both | not filed |
| S2-4 ✔ | P2 | `return a / b, a % b` | `expected end of line, found ','` | CPython, Go `3 1` | not filed |
| S2-7 | P3 | 1-tuple type and pattern | parse error; renders `(int)` | Rust `(i64,)` | not filed |
| S3-1 ✔ | P1 | check and run are exponential in a `recv.name[X](args)` chain: `.map[int](inc)` repeated | 12 links 224 ms, 16 links 3.5 s, 18 links over 10 s; without `[int]` 30 links take 8 ms | rustc: 30 links in 81 ms | not filed |
| S3-3 ✔ | P2 | `t.0[i](5)` on a tuple whose element 0 is a list of functions | `unknown type 'i'` (read as a type argument); `t.0[0](5)` works | Rust, CPython `4` | not filed |
| S3-4 ✔ | P2 | the generator ban on `spawn`/`defer`/`parallel:`/`wait:` is bypassed inside a `recover:` block in an `assert`; same gap gives a false "`yield` can only appear inside a generator" | check OK; the task outlives the generator | the checker's own rule | not filed |
| S3-6 | P3 | grammar and parser drift that the conformance test cannot see: negative literal in a sub-pattern, `(x) := 5`, `1 \|> f(2,)`, `enum E: A()`, `where T: None` | parser and `docs/reference/grammar.bnf` disagree on accept/reject | — | not filed |

S2-1 is silent wrong output in plausible code (a `replace('\\n', ' ')` inside a hole does nothing), so it
meets the P0 definition; the escaped-brace forms are rare. Sites: the lexer stores the post-escape payload
(`src/lexer/mod.rs:1532`, `:1607`), `src/interpolation.rs:46` scans that payload, and `tokenize_frag`
(`src/lexer/mod.rs:1925`) re-lexes each fragment, running escapes again and starting with bracket depth 0.

More facts with several deciders (from the grammar hunter's table, `/home/chezzijr/.cache/w24-syntax/s3/`):

| Fact | Deciders | Rule that removes it |
|---|---|---|
| what `head[X]` means (index or type argument) | three parser copies of the "headed" test (`src/parser/mod.rs:3233`, `:3412`, `:3547`; the first lacks the tuple-index exclusion) and a checker probe that infers the receiver twice per link (`src/checker/expr.rs:234`, `:1199`) | one `bracket_head` function; the checker decides from the receiver type it already has (S3-1, S3-3) |
| what a statement contains | `stmt_expr_recover_blocks` (`src/ast/mod.rs:1435`) is a partial hand copy of the exhaustive `ids_in_stmt`; it ends in `_ => {}` and omits `assert` and match-arm guards | one exhaustive visitor with no wildcard arm (S3-4) |
| a fused two-character token that means two operators | `??` is split by hand in type, prefix and pattern position, not in postfix position | split it in one place (S2-3) |
| the shape of a tuple, a negative literal, a bound head | parsed by separate code in expression, type and pattern position | one helper per form, parameterised by the element parser (S2-7, S3-6) |
| the grammar is the specification | `cargo test conformance` compares accept/reject on a corpus; it never compares the tree and never checks that a parser-accepted program is derived by the grammar | a mutation differential in the suite (a working model is `s3/final/fuzz.py`, about 146,000 mutants, no parser crash) |

Clean: the expression parser (3,000-expression parenthesisation differential, 0 differences); statements
never join across lines; dangling `else`/`elif` matched CPython on about 4,300 generated programs; bracket
line joining; every pattern form; numeric literal edges; all 143 code blocks of `docs/reference/syntax.md`.

## Names (U13)

| Finding | Detail |
|---|---|
| a user type named like a builtin function is accepted and its constructor is dead | `struct print: x: int` checks clean; `print(1)` still prints. `enum chr: A` then `chr(65)` prints `A`. But `print := 3` shadows, `fn print` is rejected, `struct range` is rejected. Order today: variable > builtin > user type (`src/checker/mod.rs:208` against `:386`, `resolve.rs:172`) |
| ~~whole-module import differs for std and user modules~~ | FIXED 2026-10-16: an import binds only the names it lists, for a std module too (`tests/module_member_grid.rs`) |
| divergence is decided by name | `panic` and `os.exit` are bottom-typed; a user `fn die(msg: str): panic(msg)` is not, so its caller "can fall off the end". No `never` type |
| `Error.line()`, `.col()`, `.file()` | exist only on the bare `Error` existential |

Builtins the language itself cannot express: `int()`/`float()`/`bool()` (an untyped parameter, overloaded
by argument type); `bytes()`/`bytearray()` (overloaded by type and arity); `List()`/`Set()`/`Map()` (an internal union, not `Iterable[T]`); `Channel`,
`Shared`, `RwShared`, `Atomic`, `Executor` (dedicated opcodes and type variants); `json.decode[T]`;
the signatures of `Shared.read` and `Executor.submit` (attached in Rust).

## Owner design decisions (2026-10-12)

These settle rules U5 to U10, U15 and U16. Ancestor output below is from programs that were run
(`/home/chezzijr/.cache/design-probes/`). No ticket implements them yet unless a row names one.

| Rule | Decision |
|---|---|
| U5 | An operator requires its protocol. `a in b` requires `b: Contains[A]`; `for a in b` requires `b: Iterable[A]`. Builtin types declare their protocols like user types, with conditions where needed (`List[T]: Contains[T] where T: Eq`). |
| U6, U7 | A range is a value of type `Range`. `a..b` and `a..=b` (inclusive, as Rust) build one. Fields: `start: int?`, `end: int?`, `step: int = 1`, `inclusive: bool = false`. The constructor and the literal build the same value (`Range(5, None) == 5..`, `Range(0, 3, inclusive=true) == 0..=3`), and every rule is a rule about the value, not the spelling. Open ends `..b` and `a..` exist for slices. `Range` satisfies `Iterable[int]` (not `Iterator`: one range can be iterated twice) and `Contains[int]`. The literal has no step syntax; `Range(a, b, step)` is the one place for a step. `range()` is deleted: `range(n)` becomes `0..n`, `range(a, b, c)` becomes `Range(a, b, c)`, and a use that needs a list writes `List(0..n)`. |
| U7, slices | `xs[a..b]` replaces `xs[a:b]`. A type is sliceable through `Slice` with `slice(self, r: Range)`. Open ends: `xs[..2]`, `xs[3..]`. Negative bounds stay (`xs[-2..]`). `xs[::-1]` becomes `xs.reversed()`. A stepped range slices: `xs[Range(0, 10, 2)]` obeys the step, so no new method is needed. Out-of-range bounds clamp as CPython. No `r[i]` on a `Range`; `Range` is `Eq` and `Hashable`. `..` binds below arithmetic and above comparison and `in`. |
| U7, open ends | `for i in 5..` is lazy and legal; at the top of `int` it faults with the integer-overflow fault. The collectors do not test for a range: `List(5..)` behaves as `List` of any infinite iterator does and runs until memory ends (Rust does the same). The rules live in `Range`'s own methods: `.len()` on a range with a missing bound faults, and a range with no start cannot be iterated. Open ends work in a slice and with `in`. A general memory fault (CPython raises `MemoryError`) is a possible later feature, not planned. |
| U7, negative bounds | A negative bound means "count from the end" in every slice form. One function resolves a `Range` against a length `n`: a missing start is 0 and a missing end is `n`; a negative bound `b` becomes `b + n`; an inclusive end adds 1 after that; both bounds clamp to `0..n`; start >= end gives an empty result. `xs[0..=-1]` is the whole list. A plain index stays strict: `xs[-4]` faults. Rust has no negative index; CPython owns this rule. |
| U16, reverse | `reversed()` returns a new value on `List`, `str`, `bytes`, `bytearray`. `reverse()` changes in place and exists on `List` and `bytearray` only. `str.reverse()` is deleted. |
| U6, channels | `Channel[T]` satisfies `Iterable[T]`. `for v in ch` ends when the channel is closed and empty; `List(ch)` blocks until then (Rust: `rx.iter().collect()`). The comprehension veto and the `for`-only arms go away. |
| U8 | A block expression is legal in value position only: the right side of `:=`, `=`, `return`, `yield`, and a lambda body. The `if` form is an expression only on one line (`if a: b else: c`, with `elif`); an `if` with an indented block is a statement, because a Chezzi block has no tail value. A `match` used as a value has expression arms. Open: the `recover:` block (see below). |
| U9 | One unpacking grammar at `:=`, `for`, comprehensions and `match`. No outer parentheses (`a, b := t`, as Python); a nested pattern takes parentheses (`a, (b, c) := t`); `_` skips a slot; a bare tuple is a tuple at the end of a statement (`return a, b`, `x := 1, 2`). Only shapes whose size the checker knows unpack outside `match`: tuples and struct fields. No list unpack and no `*rest`. An enum variant unpacks in `match` only. |
| U10 | A type name is never a value. A parameter of type `fn(T) -> U` accepts a function; `xs.map(MyStruct)`, `xs.map(E.Variant)` and `xs.map(str)` are errors that say the name is a type, and the user writes `fn(e): MyStruct(e)`. |
| U15 | An operator returns a new value and never mutates an operand, for every builtin and std type. `x OP= v` means `x = x OP v` for every type; `xs += [v]` no longer mutates in place (this reverses DEC-015 and DEC-032). In-place change is a method: `xs.push(v)`, `xs.extend(ys)`. A user type can break the rule; the docs state it as a convention. |
| U16 | One name per job across collections, so that each collection satisfies the protocol. Membership is `contains` (149 uses against 56 of `.has`). A full table for `remove`, miss behaviour and `reverse` is still to be drafted. |
| Sibling types | `bytes` gets what `str` has: `+`, `<`, `in` (CPython: `b'ab'`, `True`, `True`). |
| Interpolation (TICKET-252, merged) | The same quote is legal inside a hole; a backslash in hole code is an error; the first `:` outside brackets starts the format spec. An `extern` library name takes no hole. |

**Range subscripts (decided 2026-10-13, after TICKET-253 merged at `877283d7`).**
- A type that declares both `slice(self, r: Range)` and `index(self, k: Range)` is a declaration error:
  `x[r]` would be ambiguous. No precedence rule. A `Map` with range keys has no `slice`, so `m[0..3]`
  is a key lookup.
- "Inclusive" means "include the end value" for each step direction: `List(Range(2, 0, -1,
  inclusive=true))` is `[2, 1, 0]`, and a slice selects the positions the range yields.
- Open inconsistency: `g[0..2].push([9])` is silent at top level (it changes a temporary copy that the
  program drops) and is the airlock type error about `g` inside a spawned function. The consistent
  rule is one warning in each context ("this changes a temporary copy; the result is dropped"), which
  today exists for `Shared` / `RwShared` / `Atomic` reads only, and no airlock error.

**`recover:` (decided 2026-10-12).** The block form (`recover:` followed by an indented block) has no
tail value: its type is `!` (`None!`, "the error, or nothing"), like Rust's `Result<(), Error>`. The
inline form `recover: expr` has type `T!` (the value, or the error). To take a value out of a block,
assign inside it. Today the last expression of a block is its value (`s := recover:` + `x := 10` +
`x / 2` gives `5`, type `int!`); the repo has 144 block uses and 360 inline uses, and the number of
blocks that read a tail value is not measured yet. `None` in `None!` means "returns nothing": it is not
a value type (`x: None = None` is an error), `None?` is not a type, and success is a bare `return`.
Docs and diagnostics use the short spelling `!` for this type.

## Why this keeps happening

Most P0 and P1 findings of waves 16 to 23 have one cause: a feature was added and one of the sites that
decide a fact about it was not updated. The build stayed green, the tests stayed green, and the bug
shipped. A smaller group is two sites that decided the same thing differently from the start (`in`
against `Contains`). A few are real logic or design defects (the nursery hang of TICKET-245, the `Shared`
copy cost of TICKET-246).

| P0 | The site that was not updated |
|---|---|
| `yield ?buf` lost write (TICKET-247) | TICKET-227 added `?x` and `!x`; a wildcard arm in the airlock walker absorbed them |
| `double(?5)` skips its bound (TICKET-248) | one of five readers saw the raw type variable |
| a hook written with `yield` ends the program (TICKET-250) | 12 of 13 runtime hook calls skipped the generator path |
| `fn cast(self)` is a universal cast (TICKET-250) | four sites test the spelling `self`, one tests the position |
| an arm `?_:` after a nested `match` returns early (TICKET-249) | 14 parser sites decide "does the next line continue", two with a different test |

Why the tests did not catch them:

| Reason | Effect |
|---|---|
| the test is written from the same list as the fix | a missed site is missed twice |
| the compiler is type-blind | the checker accepts, the run time does something else, and nothing reports it |
| a Rust `_ =>` arm | a new enum variant silently takes the old behaviour |
| earlier fixes were point patches (34 of 51 in TICKET-120..176) | one cell was fixed and the family came back |

Measured on main at `03422eaf` with `cargo clippy --lib -- -W clippy::wildcard_enum_match_arm`:
**461 wildcard arms over enums.** By file: `src/vm/call.rs` 46, `src/checker/sig.rs` 42,
`src/vm/arith.rs` 34, `src/compiler/mod.rs` 33, `src/checker/pattern.rs` 32, `src/checker/setup.rs` 28,
`src/checker/expr.rs` 27, `src/parser/mod.rs` 25, `src/checker/fn_writes.rs` 25, `src/vm/stmt.rs` 20,
`src/checker/proto.rs` 20.

### Gates that make a missed site a failure

None of these is built yet.

| # | Gate | Catches | Does not catch |
|---|---|---|---|
| G1 | the wildcard-arm lint is an error for the language enums (operators, expression kinds, statement kinds, types, tokens, opcodes); each of the 461 arms becomes an explicit variant list or a justified allow | a new variant with no decision at a site: the build fails there | a hand list of builtin types that is not an enum match |
| G2 | family grid tests loop over the enum's variants, not over a written list | a new variant with no test row | — |
| G3 | an agreement test generated for every type x operator: the operator accepts exactly when the bound accepts | `in`/`Contains`, `+`/`Add`, half-hook types | disagreement outside protocols |
| G4 | count gates: the number of name-string tests of user-facing names (509 today) and of "mirrors" / "keep in sync" comments may only fall | new by-name dispatch | existing sites |
| G5 | a tree differential for the grammar: token-mutated programs, grammar derivation against parser tree (model: `/home/chezzijr/.cache/w24-syntax/s3/final/fuzz.py`) | "both accept, different tree", which `cargo test conformance` cannot see | — |

Gates stop new disagreement. They do not repair what exists; the unifying rules above do that.

### Structural changes that remove the mechanism

| Change | Removes |
|---|---|
| builtin types declare their protocols in `std/*.chz` like user types, and every operator asks the one conformance decider (TICKET-250 builds it for user types) | most of the 888 builtin-type arms; every operator-against-bound disagreement |
| one method table per type; the run time dispatches from the table, not from name strings | the 226 name tests in `src/vm` |
| no wildcard arm on a language enum (G1) | the "new construct silently absorbed" class |

Recommended before the JIT: a JIT adds one more copy of every by-name dispatch, so each defect above
would get a second home.

### How the agent pipeline contributed

The pipeline did not create the hand lists. It multiplied their effect.

| Trait | Effect here |
|---|---|
| one ticket is one symptom | the agent fixes the reported cell and adds a clause beside it |
| a fresh session per stage | the agent does not know the other sites that decide the same fact |
| the same agent writes the fix and its test | a missed site is missed in the test too |
| review reads the diff | a diff cannot show the site that was not touched |
| gates check green tests and greps | no gate checks that all sites agree |
| review and plan-validation cannot run the binary | findings are "read from the code, not run"; the nested-nursery regression of TICKET-246 was caught only because the monitor ran the reviewer's probe |

The pipeline amplifies the shape of the ticket: a symptom ticket produces a point patch, a family ticket
produces a structural fix (TICKET-247 to 250 each named a single source and landed a grid of 400 to
3,800 cells).

| Fix on the pipeline side | Kind |
|---|---|
| review and plan-validation stages may run the `chezzi` binary under `timeout` | logic/config (the stage guard's allow-list) plus one prompt line |
| triage answers "which other sites decide this fact", by grep, before planning | prompt; stronger with a gate that rejects a ticket without the section |
| a post-merge neighbour probe on the release binary (same shape, other token, type, position) | logic: a new stage |
| the family test is written by a stage that has not seen the fix | logic: a new stage and stage order |

A prompt tells; a gate enforces. The `## Single source` rule was followed reliably only after a gate
checked for it.

Owner direction (2026-10-11): fixes are done outside the pipeline for now.

## Documented as deliberate (class A) — listed so the owner can reconfirm

- A tuple is not iterable, indexable or sized; only `.N` and destructuring (`docs/reference/syntax.md:179-190`).
- `float` is not Hashable. `Map(m)` is rejected (TICKET-160).
- Int `/` truncates and `%` follows the dividend (`-7 / 2` is `-3`; CPython `-4`).
- A present `T?` prints as its bare payload.
- Types, `import`, `test fn`, `extern`, `native` are top-level only (owner decision).
- Intrinsic hook methods are callable only through a bound: `(5).str()` is "no method" (`docs/reference/syntax.md:2941`).
- A reserved protocol and a same-shaped user protocol disagree: user `protocol MyStr: fn str(self) -> str`
  rejects `int` (`docs/reference/syntax.md:2284`). Handles never witness a user protocol (`:2288`).
- Protocol conformance includes parameter names (TICKET-187; Go accepts any names).
- `defer` is block-scoped (Go: function-scoped).
- Native methods refuse named arguments (DEC-182).
- Return type is inferred only for an inline-expression body (`docs/reference/syntax.md:1030`).
- `recover:` and `match` are not allowed inside brackets because layout is off there (`docs/reference/syntax.md:3766`).

Several of these are the documented half of a rule in the table above (U5, U8). Documenting a special
case does not make it consistent.

## Doc drift found on the way

- `docs/reference/syntax.md:1680`, `:1687` and `examples/generators_inferred.chz` say a generator's annotation is
  optional; the binary and `docs/reference/spec.md:204` require `-> Iterator[T]`.
- `docs/reference/grammar.bnf:412` says `spawn` is legal only inside `parallel:`.
- `docs/reference/syntax.md:139` against `:1564` on ranges; `:3923` against `:4001` on `?` in `defer:`.
- `docs/reference/syntax.md:1646` says `iter()` on a struct with `next` returns self; the method does not exist.
- False diagnostics: "cannot match on non-enum type float"; "named arguments are only supported on
  functions, struct constructors, and struct methods"; "`wait` needs at least one `recv` arm" (a send-only
  `wait:` is accepted); every parse error is labelled "resolve error"; `t := (1, None)` suggests
  `(int, int?)`.

## Evidence

| Agent | Directory |
|---|---|
| layout hunter | `/home/chezzijr/.cache/w24-syntax/s1/` |
| token-role hunter | `/home/chezzijr/.cache/w24-syntax/s2/find/` |
| grammar against parser | `/home/chezzijr/.cache/w24-syntax/s3/` |
| builtin types against protocols | `/home/chezzijr/.cache/w24-special/x1/` (grids `g/gN.matrix.txt`), summary `x1-summary.md` |
| positional rules | `/home/chezzijr/.cache/w24-special/x2/` (`grids/grid1.txt`, `grid2.txt`, `sgrid.txt`) |
| hard-coded names | `/home/chezzijr/.cache/w24-special/x3/` (`TABLES.txt`), summary `x3-summary.md` |
