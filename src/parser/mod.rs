//! Parser (M2): turns the lexer's `Tok` stream into an AST `Module`.
//!
//! Two interlocking techniques:
//!   - **recursive descent** for statements / declarations (one `parse_*` fn per form), and
//!   - a **Pratt** (precedence-climbing) parser for expressions, following the binding powers
//!     in `docs/syntax.md` §4.
//!
//! Layout tokens (`Newline`/`Indent`/`Dedent`) from the indentation-aware lexer drive block
//! structure: a block opens after a `:` as either an indented `Newline Indent … Dedent` group or
//! a single inline statement on the same line (used by `match` arms like `Circle(r): return …`).

use crate::ast::*;
use crate::lexer::{Span, Tok, Token};
use std::fmt;

/// Parsed call arguments: the positional args, then the named (`name = expr`) args, in source order.
type CallArgs = (Vec<Expr>, Vec<(String, Expr)>);

/// A syntax error, with the source location it was detected at.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error ({}): {}", self.span, self.message)
    }
}

type PResult<T> = Result<T, ParseError>;

/// Cap on parser **recursion** (nested expressions, types, patterns and blocks). Past this we return
/// a `ParseError` instead of letting native stack recursion overflow and abort the process.
///
/// This bounds ONE resource: the parser's own stack. It is no longer "sized against the ~2 MiB test
/// worker" — since W7-50 every production front-end entry point (CLI, resolver, `editor::diagnostics`
/// / `hover` / `semantic_tokens`) runs the parse on the dedicated [`crate::FRONTEND_STACK_BYTES`]
/// (1 GiB) thread, and the smaller of the two big stacks a parse can land on is the VM's 384 MiB
/// `vm::VM_STACK_BYTES` worker (`chezzi run` re-does `build_graph` on it).
///
/// **Derivation (measured, `checker::tests::stack_probe_frontend_walker_depth`, debug build):** one
/// `MAX_DEPTH` unit of recursive-descent costs **7 753 bytes** of parser stack. 384 MiB / 7 753 ≈
/// **51 900** units of budget; 512 spends ≈ 4 MB, a **~100× margin**. That buys ≥ 200 levels of every
/// CPython-comparable nesting form (`((1))`, `[[…]]`, `f(f(…))`, nested `if`) — CPython 3.14's own
/// limit is 200 — where 64 bought only 30.
///
/// **It is still a function of `sizeof` the AST node types** (Was 128 → 64 → briefly 48 when `Span`
/// grew 16→24 bytes for its `file` id in W7-49; `Span` is now 12 bytes, see [`crate::lexer::Span`]),
/// but at a ~100× margin a `sizeof` change no longer moves it — it moves the margin.
/// `tests::max_depth_boundary_accepts_then_rejects` is the SIZING ORACLE: it bisects the real accepted
/// nesting per production and fails with the measured number when a constant drifts. Re-run the stack
/// probe (`cargo test --lib stack_probe_frontend_walker_depth -- --ignored --nocapture`) to
/// re-derive, don't guess.
pub const MAX_DEPTH: usize = 512;

/// Cap on the **AST depth** the parser is willing to hand to the recursive front-end walkers
/// (`desugar::walk_expr`, the checker's inference walk, the compiler's lowering) — a different
/// resource from [`MAX_DEPTH`], which is why it is a different constant.
///
/// `MAX_DEPTH` cannot bound it: left-associative binary chains (`a + b + c …`) and postfix chains
/// (`x.f.f…`, `a[0][0]…`, `f().g()…`) fold in the `while`/`loop` bodies of `parse_bp` /
/// `parse_postfix`, which do NOT recurse per fold, so `self.depth` never sees them — yet each fold
/// adds one level to a LEFT-leaning AST.
///
/// **Derivation (measured, same probe, debug build):** the binding walker is `compile` at **12 162
/// bytes per AST node**; 384 MiB / 12 162 ≈ **33 100** nodes of budget. Measured directly end to end
/// (debug `chezzi run`, the shape below): 33 000 nodes is the last depth that survives, 33 500 aborts
/// — the derived and measured cliffs agree.
///
/// **This constant is a DELIBERATE NARROWING of what parses, not a no-op.** The worst AST the parser
/// accepted before it existed was ~**29 940** nodes. (An earlier writeup said 15 000 and claimed
/// nothing that parsed then stops parsing; both were wrong, because they measured a shape that spends
/// only ONE fold loop per nesting level.) A single paren level can spend **both** fold loops —
/// `parse_postfix`'s and `parse_bp`'s — for ~998 nodes per level, not 499. Measured, release binaries,
/// source `x := ` + *k* × `( … .f×499 +1×499 )`, bisected:
///
/// | | last accepted *k* | worst accepted AST | margin vs the 33 100-node cliff |
/// |---|---|---|---|
/// | `b1307258`, before | **30** | ~29 940 nodes | **1.11×** |
/// | today | **16** | ~15 968 nodes | **2.07×** |
///
/// So a program between ~16 000 and ~30 000 nodes deep that parsed at `b1307258` is now rejected with
/// a clean `expression nested too deeply` — machine-generated code is where those live. **That is the
/// correct call, and it is the whole point of the change:** at 1.11× the parser was accepting programs
/// within ~10% of a host stack overflow, i.e. an uncatchable SIGABRT on a well-typed program, which is
/// the exact failure W7-50 exists to prevent. A diagnostic is recoverable; an abort is not. Buying the
/// depth back needs a bigger `vm::VM_STACK_BYTES`, which is reserved **per M:N pool worker** — not a
/// bigger value here.
///
/// `tests::max_ast_depth_floor_holds` is the floor detector: a FIXED fold fixture, deliberately not
/// derived from this constant, so shrinking the constant below its promise fails a test instead of
/// passing silently.
///
/// **The bound is on a SYNTHESIZED (bottom-up) measure, not an ambient counter.** `Parser::fold_depth`
/// carries the fold-depth of the deepest subtree already built in the current scope, and each fold
/// loop checks `self.depth + self.fold_depth + chain`. An *ambient* per-path counter that is
/// decremented when its loop exits cannot work, because a fold loop runs AFTER the descent it sits
/// above: in `f(g(DEEP) + 1 + 1 + …)` the 498 folds are counted only once `DEEP` has finished and
/// popped its own counts, so the two never coexist in an ambient counter even though they add in the
/// tree. Measured: with an ambient bound and `MAX_DEPTH = 512`, that shape parses at 45 000 nodes deep
/// and **SIGABRTs `chezzi run`**. The synthesized measure composes across levels and rejects it.
///
/// **Breadth is never conflated with depth**: `fold_depth` is `max`-combined over siblings (a fold
/// loop restores `outer.max(self.fold_depth + chain)`), so a 5 000-element list or a wide argument
/// list costs the depth of its deepest element, not their sum. `tests::deep_iterative_chains_error_not_crash`
/// pins breadth, composition, and the double fold above.
///
/// **A hole is parsed by the SAME `Parser` as the code around it** (`Parser::parse_hole`), so the
/// bound above composes across an interpolated `{…}` hole: its `depth` and `fold_depth` carry on,
/// and `"{ <15 985 deep> }".len()` nested three times is refused, as one expression is. Until
/// TICKET-252 a second `Parser` re-parsed the hole text from depth zero and the budgets composed:
/// one fresh 16 000 per level, ~46 000 nodes at three levels, a SIGABRT in debug `chezzi run` on the
/// 384 MiB `vm::VM_STACK_BYTES` worker. `desugar::Walker::walk_expr` stays as the backstop: it
/// measures the finished tree and refuses at this same constant. A default argument filled in at the
/// call site composes with the caller's expression the same way (measured, release binary: `fn f(a:
/// int = <15 000-deep chain>) -> int: return a` called as `x := f() + <2000-deep chain>` was accepted
/// on `e1137096`; HEAD refuses it with `expression nested too deeply`).
/// `tests::nested_holes_share_the_parser_depth_budget` and
/// `check_errors_json::composed_interp_depth_is_bounded_globally` detect the hole seam.
pub const MAX_AST_DEPTH: usize = 16_000;

/// Parse a full token stream into a `Module`.
pub fn parse(tokens: Vec<Tok>) -> PResult<Module> {
    Parser::new(tokens).parse_module()
}

/// Parse a token stream into a `Module`, and return the tokens as the parser left them: the lexer's
/// tokens plus the layout a block expression inside brackets asked for ([`crate::lexer::open_layout`]).
/// The conformance test encodes that vector, which is the stream `docs/grammar.bnf` describes.
pub fn parse_with_tokens(tokens: Vec<Tok>) -> (PResult<Module>, Vec<Tok>) {
    let mut p = Parser::new(tokens);
    let module = p.parse_module();
    (module, p.toks)
}

/// Parse a token stream into a `Module`, attaching doc-comments from the lexer side-channel.
///
/// `comments` is the `(line, stripped_text)` table from [`crate::lexer::tokenize_with_comments`]:
/// each comment-only source line. A declaration's doc is the contiguous run of comment lines
/// IMMEDIATELY above its keyword line (a blank/non-comment line breaks the run — only the adjacent
/// block attaches). All existing `parse` callers get `doc = None` (zero behavior change); only the
/// resolver routes docs in. The docs are purely informational (LSP hover) and runtime-inert.
pub fn parse_with_docs(
    tokens: Vec<Tok>,
    comments: Vec<crate::lexer::DocComment>,
) -> PResult<Module> {
    let mut p = Parser::new(tokens);
    p.docs = comments.into_iter().collect();
    p.parse_module()
}

/// Parses one `<item>`: the parser [`Parser::item_keyword`] pairs with an item keyword.
type ItemParser = fn(&mut Parser) -> PResult<StmtKind>;

/// TICKET-241 -- the one text for an `else` guard written where none exists (an assignment, a
/// compound assignment, `return`). [`Parser::reject_else_guard`] is its only user.
const ELSE_GUARD_PLACE: &str =
    "an `else` guard belongs on a `:=` binding or a bare call, not on an assignment or `return`";

const IF_BRANCH: &str = "a one-line `if`/`else` takes an expression in each branch; write a statement branch, or an `else:` guard, on its own indented line";
const ELIF_LINE: &str = "an `elif` body ends its line; write the next `elif` or `else:` on its own line, and an `else:` guard in an indented body";
/// The one message for a block expression written where no whole value starts (TICKET-260, rule U8).
const BLOCK_VALUE: &str = "a block expression (a multi-line `match`, a `recover:` block) is a whole value: write it after `:=`, `=`, `return` or `yield`, as a lambda or fn body, or as a default; bind it first: `v := match ...`";
const WHERE_BOUND: &str = "a `where` entry needs a bound (`where T: Proto`)";
const TYPE_LIST_HEAD: &str = "a type list in brackets needs a name before the '[' (`Name[T, U]`); on any other value '[' takes one index";

/// Each position where a whole value starts, and so where a block expression may start (rule U8).
/// [`Parser::open_value`] is the only way to open one (`parse_value` and `parse_fn` call it); a position that is not listed here rejects a
/// block expression with [`BLOCK_VALUE`]. The grid test `tests/block_value_position_grid.rs` writes
/// one row per variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSite {
    /// `name: T = value`.
    Let,
    /// `target = value`.
    Assign,
    /// `a, b := value`.
    DestructLet,
    /// `a, b = value`.
    TupleAssign,
    Return,
    Yield,
    /// The body of `fn(...): body`.
    Lambda,
    /// An inline fn body: `fn f() -> int: body`.
    FnBody,
    /// `field: T = value` in a struct.
    FieldDefault,
    /// `param: T = value`.
    ParamDefault,
    /// A statement that starts its line.
    Stmt,
    /// The value of an arm of a `match` expression.
    MatchArm,
}

impl ValueSite {
    /// Every variant. A new variant fails to compile in [`ValueSite::name`] until it is named.
    pub const ALL: [ValueSite; 12] = [
        ValueSite::Let,
        ValueSite::Assign,
        ValueSite::DestructLet,
        ValueSite::TupleAssign,
        ValueSite::Return,
        ValueSite::Yield,
        ValueSite::Lambda,
        ValueSite::FnBody,
        ValueSite::FieldDefault,
        ValueSite::ParamDefault,
        ValueSite::Stmt,
        ValueSite::MatchArm,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ValueSite::Let => "typed let",
            ValueSite::Assign => "assignment",
            ValueSite::DestructLet => "destructuring let",
            ValueSite::TupleAssign => "tuple assignment",
            ValueSite::Return => "return",
            ValueSite::Yield => "yield",
            ValueSite::Lambda => "lambda body",
            ValueSite::FnBody => "inline fn body",
            ValueSite::FieldDefault => "field default",
            ValueSite::ParamDefault => "parameter default",
            ValueSite::Stmt => "statement",
            ValueSite::MatchArm => "match arm value",
        }
    }
}

struct Parser {
    /// The token index where the value being parsed starts, set by [`Parser::parse_value`]. A block
    /// expression may start only at this exact token ([`Parser::block_ok`]).
    value_start: Option<usize>,
    /// The inline body of the `if` / `elif` being parsed: the token index where it starts, and the
    /// `depth` of its one statement.
    if_line: Option<(usize, usize)>,
    /// The statement being parsed is the one statement of an inline body (`parse_stmt_in(true)`),
    /// so it does not start its line.
    stmt_inline: bool,
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
    /// SYNTHESIZED (bottom-up) fold-depth: how many *fold* levels the deepest expression already
    /// built in the current fold-loop scope contributes to the AST. Raised only by the three fold
    /// loops (`parse_bp`'s infix `while`, `parse_postfix`'s `loop`, and `parse_type_postfix`'s `loop`
    /// — whose scope is opened by its caller `parse_type`, since the base type is parsed before it is
    /// entered), each of which resets it to `0`
    /// before parsing its children and restores `outer.max(self.fold_depth + chain)` on exit — so it
    /// is a `max` over siblings (breadth costs nothing) and a `sum` up the spine (a chain folded on
    /// top of an already-deep subtree pays for both). Together with [`MAX_AST_DEPTH`] this is what
    /// keeps the recursive front-end walkers off the stack cliff; see that constant for why an
    /// ambient, decremented-on-loop-exit counter cannot do the job.
    fold_depth: usize,
    /// Doc-comment side table: source-line → stripped comment text, for every comment-only line.
    /// Empty for `parse` (so every existing caller gets `doc = None`); populated by `parse_with_docs`
    /// from the lexer side-channel. Consulted by `doc_above` at each declaration's keyword line.
    docs: std::collections::HashMap<usize, String>,
    /// The enums this module declares as a `native enum`. A module that declares the owner of a
    /// removed name may still spell it; that is how `std/prelude.chz` parses for every reader. It
    /// is no user escape: the checker rejects a `native enum` outside the standard library.
    native_enums: Vec<String>,
    /// Position of a `!=` whose `!` the type before it has taken: the lexer fuses `T!= v`, and
    /// [`Parser::parse_type_postfix`] reads it as `T! = v` without consuming or rewriting the token
    /// (a failed speculative type parse restores `pos` only). [`Parser::eat_type_assign`] is the
    /// only reader.
    bang_eq: Option<usize>,
    /// A hole has been parsed on this parser. A depth error after that says so: a hole spends the
    /// same budget as the code around it, and a bare "too deeply" would not tell the user why.
    hole_parsed: bool,
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        let native_enums = toks
            .windows(3)
            .filter_map(|w| match (&w[0].kind, &w[1].kind, &w[2].kind) {
                (Token::Native, Token::Enum, Token::Ident(n)) => Some(n.clone()),
                _ => None,
            })
            .collect();
        Parser {
            value_start: None,
            if_line: None,
            stmt_inline: false,
            toks,
            pos: 0,
            depth: 0,
            fold_depth: 0,
            docs: std::collections::HashMap::new(),
            native_enums,
            bang_eq: None,
            hole_parsed: false,
        }
    }

    /// The error for an expression past a depth bound. The one place that words it.
    fn too_deep(&self) -> ParseError {
        let mut message = "expression nested too deeply".to_string();
        if self.hole_parsed {
            message.push_str(&format!(
                " (limits: {MAX_DEPTH} nested levels, {MAX_AST_DEPTH} levels of tree depth); an interpolated `{{…}}` fragment nests INSIDE the expression around it and spends the same budget"
            ));
        }
        self.err(message)
    }

    /// The doc-comment attached to a declaration whose keyword sits on `decl_line`: the contiguous
    /// run of comment-only lines immediately above it (`decl_line - 1`, `decl_line - 2`, …) while
    /// each is present in the side table. A blank/non-comment line is absent → the run stops there
    /// (so a blank line detaches earlier comments, and stacked `#` lines join). Lines join top-down
    /// with `\n`. Returns `None` when there is no adjacent comment (incl. the empty-map `parse` path).
    fn doc_above(&self, decl_line: u32) -> Option<String> {
        if self.docs.is_empty() {
            return None;
        }
        let mut lines: Vec<&str> = Vec::new();
        // `as usize`: widening a `Span` line (u32) to the doc side-table's key — never truncates.
        let mut line = decl_line as usize;
        while line > 1 {
            line -= 1;
            match self.docs.get(&line) {
                Some(text) => lines.push(text.as_str()),
                None => break,
            }
        }
        if lines.is_empty() {
            return None;
        }
        lines.reverse(); // collected bottom-up; restore source order
        Some(lines.join("\n"))
    }

    // ----- cursor helpers -----
}

mod control;
mod cursor;
mod decl;
mod expr;
mod interp;
mod pattern;
mod stmt;
#[cfg(test)]
mod tests;
mod types;
