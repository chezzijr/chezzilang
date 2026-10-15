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
use crate::fmtspec::SpecItem;
use crate::lexer::{Span, SpecPart, StrLit, StrPart, Tok, Token};
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

/// TICKET-228 (D6) — the long carrier spellings removed from the surface: (name, the prelude enum
/// that owns it, the message a bare use gets, the near-miss hint of a TYPE name). The enums stay
/// inside the compiler; [`Parser::reject_removed_name`] is the one place a bare use is rejected.
pub const REMOVED_NAMES: [(&str, &str, &str, Option<&str>); 5] = [
    (
        "Option",
        "Option",
        "`Option` is removed; write `T?` for the type and `None` for the absent value",
        Some("an optional type is written `T?`"),
    ),
    (
        "Result",
        "Result",
        "`Result[T, E]` is removed; write `T!E` (`None!E` when there is no value)",
        Some("a fallible type is written `T!E`"),
    ),
    (
        "Some",
        "Option",
        "`Some(x)` is removed; write `x` or `?x` (pattern `?v`)",
        None,
    ),
    (
        "Ok",
        "Result",
        "`Ok(x)` is removed; write `x` or `?x` (pattern `?v`); for `Ok()` fall off the end or write a bare `return`",
        None,
    ),
    (
        "Err",
        "Result",
        "`Err(e)` is removed; write `!e` (pattern `!e`)",
        None,
    ),
];

/// TICKET-253 -- the message of a call to the removed builtin `range`. `range` stays a reserved
/// name only so that this message is reachable; nothing builds on it.
const REMOVED_RANGE: &str = "`range()` is removed; write `0..n`, `a..b` or `Range(a, b, step)`; for a list write `List(0..n)`";

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

    /// TICKET-228 (D6) — reject a removed long carrier name read as a BARE name (expression
    /// primary, type head, pattern head, import). A name after `.`, a declaration name, a field
    /// and a keyword-argument label never reach here, so a user variant `Shadow.Some` stays legal.
    fn reject_removed_name(&self, name: &str, span: Span) -> PResult<()> {
        match REMOVED_NAMES.iter().find(|row| row.0 == name) {
            Some(row) if !self.native_enums.iter().any(|n| n == row.1) => Err(ParseError {
                message: row.2.to_string(),
                span,
            }),
            _ => Ok(()),
        }
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

    /// The current token kind. The stream always ends with `Eof`, so this never goes out of range.
    fn peek(&self) -> &Token {
        &self.toks[self.pos].kind
    }

    /// The token kind `n` positions ahead (or `Eof` past the end).
    fn peek_at(&self, n: usize) -> &Token {
        self.toks
            .get(self.pos + n)
            .map(|t| &t.kind)
            .unwrap_or(&Token::Eof)
    }

    /// The span of the current token.
    fn cur_span(&self) -> Span {
        self.toks[self.pos].span
    }

    /// Consume and return the current token, clamping at the trailing `Eof`.
    fn advance(&mut self) -> Tok {
        let tok = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        tok
    }

    fn check(&self, kind: &Token) -> bool {
        self.peek() == kind
    }

    /// Consume the current token if it matches `kind`; report whether it did.
    fn eat(&mut self, kind: &Token) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// At the `=` that follows a type: a plain `=`, or the `!=` whose `!` that type has taken.
    fn at_type_assign(&self) -> bool {
        self.check(&Token::Assign) || (self.check(&Token::NotEq) && self.bang_eq == Some(self.pos))
    }

    /// Consume the `=` that follows a type (see [`Self::at_type_assign`]); report whether it did.
    fn eat_type_assign(&mut self) -> bool {
        if self.at_type_assign() {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consume the current token, erroring if it isn't `kind`.
    fn expect(&mut self, kind: &Token) -> PResult<Tok> {
        if self.check(kind) {
            Ok(self.advance())
        } else if let Some(open) = self.unclosed_opener(kind) {
            Err(ParseError {
                message: format!("{} was never closed", describe(&open.kind)),
                span: open.span,
            })
        } else {
            Err(self.err(format!(
                "expected {}, found {}",
                describe(kind),
                describe(self.peek())
            )))
        }
    }

    /// When `close` is a closing delimiter (`)`/`]`/`}`) and the token stream never closes the
    /// nearest matching opener before `self.pos`, return that opener. STATELESS by design — three
    /// backtracking sites restore `self.pos` by hand, so a delimiter stack maintained in `advance`
    /// would desync there. A forward scan runs FIRST: `f(a b)` must keep reporting `expected ')'`,
    /// because its `(` IS closed later, just not where `expect` looked.
    fn unclosed_opener(&self, close: &Token) -> Option<Tok> {
        let open_kind = match close {
            Token::RParen => Token::LParen,
            Token::RBracket => Token::LBracket,
            Token::RBrace => Token::LBrace,
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Assert
            | Token::Test
            | Token::Spawn
            | Token::Parallel
            | Token::Wait
            | Token::Yield
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::LBracket
            | Token::LBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => return None,
        };
        let mut depth: usize = 0;
        for t in &self.toks[self.pos..] {
            if t.kind == open_kind {
                depth += 1;
            } else if &t.kind == close {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
        }
        let mut depth: usize = 0;
        for t in self.toks[..self.pos].iter().rev() {
            if &t.kind == close {
                depth += 1;
            } else if t.kind == open_kind {
                if depth == 0 {
                    return Some(t.clone());
                }
                depth -= 1;
            }
        }
        None
    }

    /// `expect_ident`, but the `None` keyword is a name too. Only for the two prelude-only
    /// productions that DECLARE the variant (a `native enum` variant, an import member in a file
    /// that declares a native enum) and for a pattern head.
    fn expect_ident_or_none(&mut self) -> PResult<String> {
        if self.eat(&Token::NoneKw) {
            return Ok(crate::lexer::NONE.to_string());
        }
        self.expect_ident()
    }

    /// Consume an identifier, returning its name.
    fn expect_ident(&mut self) -> PResult<String> {
        match self.peek() {
            Token::Ident(_) => {
                if let Token::Ident(name) = self.advance().kind {
                    Ok(name)
                } else {
                    unreachable!()
                }
            }
            t @ Token::Int(_)
            | t @ Token::IntMinMagnitude
            | t @ Token::Float(_)
            | t @ Token::Str(_)
            | t @ Token::Bytes(_)
            | t @ Token::RawStr(_)
            | t @ Token::Fn
            | t @ Token::Return
            | t @ Token::If
            | t @ Token::Else
            | t @ Token::Elif
            | t @ Token::For
            | t @ Token::While
            | t @ Token::In
            | t @ Token::Break
            | t @ Token::Continue
            | t @ Token::Pass
            | t @ Token::Struct
            | t @ Token::Enum
            | t @ Token::Protocol
            | t @ Token::Type
            | t @ Token::Match
            | t @ Token::Recover
            | t @ Token::Defer
            | t @ Token::Assert
            | t @ Token::Test
            | t @ Token::Spawn
            | t @ Token::Parallel
            | t @ Token::Wait
            | t @ Token::Yield
            | t @ Token::Import
            | t @ Token::Extern
            | t @ Token::Native
            | t @ Token::From
            | t @ Token::As
            | t @ Token::Const
            | t @ Token::And
            | t @ Token::Or
            | t @ Token::Not
            | t @ Token::True
            | t @ Token::False
            | t @ Token::NoneKw
            | t @ Token::Where
            | t @ Token::Plus
            | t @ Token::Minus
            | t @ Token::Star
            | t @ Token::Slash
            | t @ Token::Percent
            | t @ Token::Assign
            | t @ Token::Walrus
            | t @ Token::EqEq
            | t @ Token::NotEq
            | t @ Token::Lt
            | t @ Token::LtEq
            | t @ Token::Gt
            | t @ Token::GtEq
            | t @ Token::PlusEq
            | t @ Token::MinusEq
            | t @ Token::StarEq
            | t @ Token::SlashEq
            | t @ Token::PercentEq
            | t @ Token::AmpEq
            | t @ Token::PipeEq
            | t @ Token::CaretEq
            | t @ Token::ShlEq
            | t @ Token::ShrEq
            | t @ Token::Arrow
            | t @ Token::Pipe
            | t @ Token::Question
            | t @ Token::QuestionDot
            | t @ Token::QuestionQuestion
            | t @ Token::Bang
            | t @ Token::Amp
            | t @ Token::Caret
            | t @ Token::BitOr
            | t @ Token::Shl
            | t @ Token::Shr
            | t @ Token::LParen
            | t @ Token::RParen
            | t @ Token::LBracket
            | t @ Token::RBracket
            | t @ Token::LBrace
            | t @ Token::RBrace
            | t @ Token::Comma
            | t @ Token::Colon
            | t @ Token::Dot
            | t @ Token::DotDot
            | t @ Token::DotDotEq
            | t @ Token::DotDotDot
            | t @ Token::Newline
            | t @ Token::Indent
            | t @ Token::Dedent
            | t @ Token::Eof => Err(self.keyword_name_error(t).unwrap_or_else(|| {
                self.err(format!(
                    "expected identifier, found {}",
                    describe(self.peek())
                ))
            })),
        }
    }

    /// The one error for a keyword in a name position; `None` when `t` is not a keyword.
    fn keyword_name_error(&self, t: &Token) -> Option<ParseError> {
        let (w, _) = crate::lexer::KEYWORDS.iter().find(|(_, k)| k == t)?;
        Some(self.err(format!(
            "expected identifier, found reserved keyword '{w}' (a keyword cannot be used as a name)"
        )))
    }

    /// The error for an expression at `:=` that `try_parse_walrus_target` did not take: the one
    /// keyword-name error for a literal keyword (`None`, `true`, `false`), else `otherwise`.
    fn bind_target_error(&self, target: Expr, otherwise: &str) -> ParseError {
        let kw = match target.kind {
            ExprKind::NoneLit => Some(Token::NoneKw),
            ExprKind::Bool(true) => Some(Token::True),
            ExprKind::Bool(false) => Some(Token::False),
            ExprKind::Ident(_)
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Pass
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => None,
        };
        let mut e = kw
            .and_then(|t| self.keyword_name_error(&t))
            .unwrap_or_else(|| self.err(otherwise.to_string()));
        e.span = target.span;
        e
    }

    /// Expect an integer literal (used for the `end` of a range pattern `start..end`).
    fn expect_int(&mut self) -> PResult<i64> {
        match self.peek() {
            Token::Int(_) => {
                if let Token::Int(n) = self.advance().kind {
                    Ok(n)
                } else {
                    unreachable!()
                }
            }
            // A bare 2^63 in integer position (range bound etc.) is out of i64 range. (The negated
            // i64::MIN form is folded upstream in `expect_pattern_int`, never reaching here.)
            Token::IntMinMagnitude => {
                Err(self.err("number too large to fit in target type".to_string()))
            }
            Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Assert
            | Token::Test
            | Token::Spawn
            | Token::Parallel
            | Token::Wait
            | Token::Yield
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => {
                Err(self.err(format!("expected integer, found {}", describe(self.peek()))))
            }
        }
    }

    /// Expect an integer literal in pattern position, accepting an optional leading `-` for a
    /// negative int literal/range bound (int-only — a leading `-` before a float still routes to
    /// `expect_int`, which rejects with "expected integer, found float"; no float pattern is added).
    fn expect_pattern_int(&mut self) -> PResult<i64> {
        if self.eat(&Token::Minus) {
            // i64::MIN carve-out in pattern/range position: `-9223372036854775808` folds to i64::MIN
            // directly (negating the 2^63 magnitude would overflow).
            if matches!(self.peek(), Token::IntMinMagnitude) {
                self.advance();
                return Ok(i64::MIN);
            }
            Ok(-self.expect_int()?)
        } else {
            self.expect_int()
        }
    }

    /// Expect a string literal (used for the library name in an `extern "lib":` header). A raw
    /// string (`r"lib"`) is an ordinary `str` literal and is accepted here too.
    fn expect_str(&mut self) -> PResult<String> {
        match self.peek() {
            Token::Str(_) | Token::RawStr(_) => match self.advance().kind {
                Token::Str(s) => match s.plain() {
                    Some(text) => Ok(text.to_string()),
                    None => Err(self.err("an extern library name cannot interpolate".to_string())),
                },
                Token::RawStr(s) => Ok(s),
                Token::Int(_)
                | Token::IntMinMagnitude
                | Token::Float(_)
                | Token::Bytes(_)
                | Token::Ident(_)
                | Token::Fn
                | Token::Return
                | Token::If
                | Token::Else
                | Token::Elif
                | Token::For
                | Token::While
                | Token::In
                | Token::Break
                | Token::Continue
                | Token::Pass
                | Token::Struct
                | Token::Enum
                | Token::Protocol
                | Token::Type
                | Token::Match
                | Token::Recover
                | Token::Defer
                | Token::Assert
                | Token::Test
                | Token::Spawn
                | Token::Parallel
                | Token::Wait
                | Token::Yield
                | Token::Import
                | Token::Extern
                | Token::Native
                | Token::From
                | Token::As
                | Token::Const
                | Token::And
                | Token::Or
                | Token::Not
                | Token::True
                | Token::False
                | Token::NoneKw
                | Token::Where
                | Token::Plus
                | Token::Minus
                | Token::Star
                | Token::Slash
                | Token::Percent
                | Token::Assign
                | Token::Walrus
                | Token::EqEq
                | Token::NotEq
                | Token::Lt
                | Token::LtEq
                | Token::Gt
                | Token::GtEq
                | Token::PlusEq
                | Token::MinusEq
                | Token::StarEq
                | Token::SlashEq
                | Token::PercentEq
                | Token::AmpEq
                | Token::PipeEq
                | Token::CaretEq
                | Token::ShlEq
                | Token::ShrEq
                | Token::Arrow
                | Token::Pipe
                | Token::Question
                | Token::QuestionDot
                | Token::QuestionQuestion
                | Token::Bang
                | Token::Amp
                | Token::Caret
                | Token::BitOr
                | Token::Shl
                | Token::Shr
                | Token::LParen
                | Token::RParen
                | Token::LBracket
                | Token::RBracket
                | Token::LBrace
                | Token::RBrace
                | Token::Comma
                | Token::Colon
                | Token::Dot
                | Token::DotDot
                | Token::DotDotEq
                | Token::DotDotDot
                | Token::Newline
                | Token::Indent
                | Token::Dedent
                | Token::Eof => unreachable!(),
            },
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Bytes(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Assert
            | Token::Test
            | Token::Spawn
            | Token::Parallel
            | Token::Wait
            | Token::Yield
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => Err(self.err(format!(
                "expected string literal, found {}",
                describe(self.peek())
            ))),
        }
    }

    /// The cursor is where a simple statement's line ends: a layout token, or the bare `else:` /
    /// `elif` that ends the one statement of an inline `if` body ([`Self::if_line_else`]).
    fn at_line_end(&self) -> bool {
        matches!(self.peek(), Token::Newline | Token::Dedent | Token::Eof) || self.if_line_else()
    }

    /// The previous token is a `Dedent`: a block expression (`match`, `recover:` with an indented
    /// body) just ended, so the expression and its statement end here. The lexer emits no `Newline`
    /// after that `Dedent`, so the next token is the first token of the next line.
    fn ended_at_block(&self) -> bool {
        self.pos > 0 && matches!(self.toks[self.pos - 1].kind, Token::Dedent)
    }

    /// Eat `t` as a continuation of the statement on this line (a `,` of a value or target list, the
    /// `:=` / `=` of a target list or a `wait` arm); never across a block's end.
    fn eat_in_line(&mut self, t: &Token) -> bool {
        !self.ended_at_block() && self.eat(t)
    }

    /// A simple statement must end the logical line; otherwise trailing tokens are a syntax error
    /// (e.g. `x := 5 y := 6` packed onto one line).
    fn expect_stmt_end(&mut self) -> PResult<()> {
        if self.at_line_end() {
            return Ok(());
        }
        // A simple statement whose value is a block-valued expression (`x := match s:` with
        // indented arms) ended with that block ([`Self::ended_at_block`]): there is no separate
        // line terminator left to require.
        if self.ended_at_block() {
            return Ok(());
        }
        Err(self.err(format!(
            "expected end of line, found {}",
            describe(self.peek())
        )))
    }

    /// A member of a `struct`/`enum`/native body must end its logical line: `docs/grammar.bnf`'s
    /// `<field>`, `<variant>` and `<nativeMethodDecl>` all end in `"NEWLINE"`. A block-valued field
    /// default ends with its block ([`Self::ended_at_block`]), with no NEWLINE of its own.
    fn expect_member_end(&self, what: &str, name: &str) -> PResult<()> {
        if matches!(self.peek(), Token::Newline | Token::Dedent | Token::Eof)
            || self.ended_at_block()
        {
            return Ok(());
        }
        Err(self.err(format!(
            "expected newline after {what} '{name}', found {}",
            describe(self.peek())
        )))
    }

    fn err(&self, message: String) -> ParseError {
        ParseError {
            message,
            span: self.cur_span(),
        }
    }

    /// Skip any run of `Newline` tokens (blank separation between statements).
    fn skip_newlines(&mut self) {
        while self.check(&Token::Newline) {
            self.advance();
        }
    }

    /// Does the rest of the current logical line contain `kind`? (Used to tell the two `import`
    /// forms apart: the `… from …` form always has a `from` before the line's newline.)
    fn rest_of_line_has(&self, kind: &Token) -> bool {
        let mut i = self.pos;
        while let Some(t) = self.toks.get(i) {
            match &t.kind {
                Token::Newline | Token::Indent | Token::Dedent | Token::Eof => return false,
                k @ Token::Int(_)
                | k @ Token::IntMinMagnitude
                | k @ Token::Float(_)
                | k @ Token::Str(_)
                | k @ Token::Bytes(_)
                | k @ Token::RawStr(_)
                | k @ Token::Ident(_)
                | k @ Token::Fn
                | k @ Token::Return
                | k @ Token::If
                | k @ Token::Else
                | k @ Token::Elif
                | k @ Token::For
                | k @ Token::While
                | k @ Token::In
                | k @ Token::Break
                | k @ Token::Continue
                | k @ Token::Pass
                | k @ Token::Struct
                | k @ Token::Enum
                | k @ Token::Protocol
                | k @ Token::Type
                | k @ Token::Match
                | k @ Token::Recover
                | k @ Token::Defer
                | k @ Token::Assert
                | k @ Token::Test
                | k @ Token::Spawn
                | k @ Token::Parallel
                | k @ Token::Wait
                | k @ Token::Yield
                | k @ Token::Import
                | k @ Token::Extern
                | k @ Token::Native
                | k @ Token::From
                | k @ Token::As
                | k @ Token::Const
                | k @ Token::And
                | k @ Token::Or
                | k @ Token::Not
                | k @ Token::True
                | k @ Token::False
                | k @ Token::NoneKw
                | k @ Token::Where
                | k @ Token::Plus
                | k @ Token::Minus
                | k @ Token::Star
                | k @ Token::Slash
                | k @ Token::Percent
                | k @ Token::Assign
                | k @ Token::Walrus
                | k @ Token::EqEq
                | k @ Token::NotEq
                | k @ Token::Lt
                | k @ Token::LtEq
                | k @ Token::Gt
                | k @ Token::GtEq
                | k @ Token::PlusEq
                | k @ Token::MinusEq
                | k @ Token::StarEq
                | k @ Token::SlashEq
                | k @ Token::PercentEq
                | k @ Token::AmpEq
                | k @ Token::PipeEq
                | k @ Token::CaretEq
                | k @ Token::ShlEq
                | k @ Token::ShrEq
                | k @ Token::Arrow
                | k @ Token::Pipe
                | k @ Token::Question
                | k @ Token::QuestionDot
                | k @ Token::QuestionQuestion
                | k @ Token::Bang
                | k @ Token::Amp
                | k @ Token::Caret
                | k @ Token::BitOr
                | k @ Token::Shl
                | k @ Token::Shr
                | k @ Token::LParen
                | k @ Token::RParen
                | k @ Token::LBracket
                | k @ Token::RBracket
                | k @ Token::LBrace
                | k @ Token::RBrace
                | k @ Token::Comma
                | k @ Token::Colon
                | k @ Token::Dot
                | k @ Token::DotDot
                | k @ Token::DotDotEq
                | k @ Token::DotDotDot
                    if k == kind =>
                {
                    return true;
                }
                Token::Int(_)
                | Token::IntMinMagnitude
                | Token::Float(_)
                | Token::Str(_)
                | Token::Bytes(_)
                | Token::RawStr(_)
                | Token::Ident(_)
                | Token::Fn
                | Token::Return
                | Token::If
                | Token::Else
                | Token::Elif
                | Token::For
                | Token::While
                | Token::In
                | Token::Break
                | Token::Continue
                | Token::Pass
                | Token::Struct
                | Token::Enum
                | Token::Protocol
                | Token::Type
                | Token::Match
                | Token::Recover
                | Token::Defer
                | Token::Assert
                | Token::Test
                | Token::Spawn
                | Token::Parallel
                | Token::Wait
                | Token::Yield
                | Token::Import
                | Token::Extern
                | Token::Native
                | Token::From
                | Token::As
                | Token::Const
                | Token::And
                | Token::Or
                | Token::Not
                | Token::True
                | Token::False
                | Token::NoneKw
                | Token::Where
                | Token::Plus
                | Token::Minus
                | Token::Star
                | Token::Slash
                | Token::Percent
                | Token::Assign
                | Token::Walrus
                | Token::EqEq
                | Token::NotEq
                | Token::Lt
                | Token::LtEq
                | Token::Gt
                | Token::GtEq
                | Token::PlusEq
                | Token::MinusEq
                | Token::StarEq
                | Token::SlashEq
                | Token::PercentEq
                | Token::AmpEq
                | Token::PipeEq
                | Token::CaretEq
                | Token::ShlEq
                | Token::ShrEq
                | Token::Arrow
                | Token::Pipe
                | Token::Question
                | Token::QuestionDot
                | Token::QuestionQuestion
                | Token::Bang
                | Token::Amp
                | Token::Caret
                | Token::BitOr
                | Token::Shl
                | Token::Shr
                | Token::LParen
                | Token::RParen
                | Token::LBracket
                | Token::RBracket
                | Token::LBrace
                | Token::RBrace
                | Token::Comma
                | Token::Colon
                | Token::Dot
                | Token::DotDot
                | Token::DotDotEq
                | Token::DotDotDot => i += 1,
            }
        }
        false
    }

    // ----- top level -----

    fn parse_module(&mut self) -> PResult<Module> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Eof) {
            stmts.push(self.parse_item()?);
            self.skip_newlines();
        }
        Ok(Module { stmts })
    }

    /// THE list of top-level-only declarations (`<item>` in `docs/grammar.bnf`): each item keyword,
    /// the name its block-position diagnostic uses, and its parser. [`Self::parse_item`] dispatches
    /// through it at module level; [`Self::parse_stmt`], which parses every block, rejects every
    /// keyword on it. The checker, compiler and resolver harvest declarations from module-level
    /// statements only, so an item in a block would otherwise parse and do nothing (TICKET-178).
    /// `fn` is not listed: a nested `fn` is a first-class local function, owned by `parse_stmt`.
    fn item_keyword(tok: &Token) -> Option<(&'static str, ItemParser)> {
        let item: (&'static str, ItemParser) = match tok {
            Token::Test => ("test fn", |p| Ok(StmtKind::Fn(p.parse_test_fn(true)?))),
            Token::Struct => ("struct", Parser::parse_struct),
            Token::Enum => ("enum", Parser::parse_enum),
            Token::Protocol => ("protocol", Parser::parse_protocol),
            Token::Type => ("type", |p| {
                let k = p.parse_type_alias()?;
                p.expect_stmt_end()?;
                Ok(k)
            }),
            Token::Import => ("import", |p| {
                let k = StmtKind::Import(p.parse_import()?);
                p.expect_stmt_end()?;
                Ok(k)
            }),
            Token::Extern => ("extern block", Parser::parse_extern),
            Token::Native => ("native declaration", Parser::parse_native_decl),
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Assert
            | Token::Spawn
            | Token::Parallel
            | Token::Wait
            | Token::Yield
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => return None,
        };
        Some(item)
    }

    /// `<topLevelStmt>`: an `<item>` at an item keyword, else a `<stmt>`. Charges `depth` like
    /// `parse_stmt`, so a method body nests exactly as deep as it did before the split.
    fn parse_item(&mut self) -> PResult<Stmt> {
        let Some((_, parse)) = Self::item_keyword(self.peek()) else {
            return self.parse_stmt();
        };
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("statement nested too deeply".to_string()));
        }
        let span = self.cur_span();
        let kind = parse(self)?;
        self.depth -= 1;
        Ok(Stmt { kind, span })
    }

    // ----- statements -----

    /// THE list of `<compoundStmt>` starts (`docs/grammar.bnf`): the parser of each statement that
    /// owns a block and ends at its `Dedent`. [`Self::parse_stmt_in`] dispatches through it, and
    /// refuses every start on it as an inline body. `next` is the token after `tok`: `fn (` opens a
    /// closure literal (an expression) and `spawn <call>` is a simple statement, so neither is here.
    /// Both `defer` forms are: `defer` is block-scoped, so an inline body would be its whole block.
    fn compound_keyword(tok: &Token, next: &Token) -> Option<ItemParser> {
        let parse: ItemParser = match tok {
            Token::Fn if next != &Token::LParen => |p| Ok(StmtKind::Fn(p.parse_fn(true)?)),
            Token::If => Parser::parse_if,
            Token::For => Parser::parse_for,
            Token::While => Parser::parse_while,
            Token::Match => Parser::parse_match,
            Token::Parallel => Parser::parse_parallel,
            Token::Wait => Parser::parse_wait,
            Token::Defer => Parser::parse_defer,
            Token::Spawn if next == &Token::Colon => Parser::parse_spawn,
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::Else
            | Token::Elif
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Recover
            | Token::Assert
            | Token::Test
            | Token::Spawn
            | Token::Yield
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => return None,
        };
        Some(parse)
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        self.parse_stmt_in(false)
    }

    /// `<stmt>`, or with `inline` the one statement written on the line of its block's `:`, which
    /// is exactly a `<simpleStmt>`.
    fn parse_stmt_in(&mut self, inline: bool) -> PResult<Stmt> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("statement nested too deeply".to_string()));
        }
        // A block never holds an item: `<stmts>` does not reach `<item>` (`docs/grammar.bnf`).
        if let Some((kind, _)) = Self::item_keyword(self.peek()) {
            return Err(self.err(format!("{kind} must be a top-level declaration")));
        }
        let span = self.cur_span();
        let outer_inline = std::mem::replace(&mut self.stmt_inline, inline);
        // Compound statements own a block and end at its `Dedent`; line-oriented statements
        // (let/assign/expr/return) must be followed by a line terminator.
        let kind = match Self::compound_keyword(self.peek(), self.peek_at(1)) {
            // `if a: if b: x` would make a trailing `else` ambiguous (which `if`?), and a nested
            // block has no indentation to end at. Force such nesting to use an indented block.
            Some(_) if inline && matches!(self.peek(), Token::If | Token::Match) => {
                self.parse_line_stmt()?
            }
            Some(_) if inline => {
                return Err(self.err(
                    "a nested block must be indented, not written inline after ':'".to_string(),
                ));
            }
            Some(parse) => parse(self)?,
            None => self.parse_line_stmt()?,
        };
        self.stmt_inline = outer_inline;
        self.depth -= 1;
        Ok(Stmt { kind, span })
    }

    /// `<simpleStmt>`: a line-oriented statement, followed by a line terminator.
    fn parse_line_stmt(&mut self) -> PResult<StmtKind> {
        let kind = match self.peek() {
            Token::Spawn => self.parse_spawn_call()?,
            Token::Return => {
                let k = self.parse_return()?;
                self.expect_stmt_end()?;
                k
            }
            Token::Yield => {
                let k = self.parse_yield()?;
                self.expect_stmt_end()?;
                k
            }
            Token::Assert => {
                let k = self.parse_assert()?;
                self.expect_stmt_end()?;
                k
            }
            Token::Break => {
                self.advance();
                self.expect_stmt_end()?;
                StmtKind::Break
            }
            Token::Continue => {
                self.advance();
                self.expect_stmt_end()?;
                StmtKind::Continue
            }
            Token::Pass => {
                // `pass` is a reserved keyword; the likely misuse is trying to use it as a name
                // (`pass := 5`, `pass = 5`, `pass: int`). Give a targeted message for those forms;
                // otherwise it's a no-op statement.
                if matches!(
                    self.peek_at(1),
                    Token::Walrus | Token::Assign | Token::Colon
                ) {
                    return Err(self
                        .keyword_name_error(&Token::Pass)
                        .expect("pass is a keyword"));
                }
                self.advance();
                self.expect_stmt_end()?;
                StmtKind::Pass
            }
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Test
            | Token::Parallel
            | Token::Wait
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => {
                let k = self.parse_simple_stmt()?;
                self.expect_stmt_end()?;
                k
            }
        };
        Ok(kind)
    }

    /// `let` (`:=` or typed `name: T = …`), assignment (`= += -=`), or a bare expression statement.
    fn parse_simple_stmt(&mut self) -> PResult<StmtKind> {
        // Doc-comment for a `let`: the contiguous comment run above this statement's first line.
        // (Computed for every simple-stmt; only consumed by the `Let` arms below.)
        let stmt_doc = self.doc_above(self.cur_span().line);
        // typed let: `name: Type = value`
        if matches!(
            self.peek(),
            Token::Ident(_) | Token::True | Token::False | Token::NoneKw
        ) && self.peek_at(1) == &Token::Colon
        {
            // Capture the binding-name token span (parallel to `names`) for decl-site hover.
            let name_span = self.cur_span();
            let name = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            // `PI: const T = …` — the immutable-binding modifier in the type slot. `const` is
            // consumed only here; `parse_type` never eats it, so it is a parse error in any other
            // type position.
            let is_const = self.eat(&Token::Const);
            let ty = self.parse_type()?;
            if !self.eat_type_assign() {
                self.expect(&Token::Assign)?;
            }
            let (value, listed) = self.parse_value_list(ValueSite::Let)?;
            let value = if listed {
                value
            } else {
                self.parse_else_guard(value)?
            };
            return Ok(StmtKind::Let {
                target: bind_leaf(name, name_span),
                ty: Some(ty),
                value,
                is_const,
                doc: stmt_doc,
            });
        }

        // `a := v`, `_ := v`, `a, (b, c) := v`: the one binding-target parse. The right side of a
        // single name is one value (`ValueSite::Let`); a target list takes `DestructLet`. A comma
        // list is a tuple written without parentheses and takes no `else` guard (DEC-241).
        if let Some(target) = self.try_parse_walrus_target()? {
            let site = if matches!(target, Pattern::Tuple(_)) {
                ValueSite::DestructLet
            } else {
                ValueSite::Let
            };
            let (value, listed) = self.parse_value_list(site)?;
            let value = if listed {
                value
            } else {
                self.parse_else_guard(value)?
            };
            return Ok(StmtKind::Let {
                target,
                ty: None,
                value,
                is_const: false,
                doc: stmt_doc,
            });
        }

        let expr = if self.stmt_inline {
            self.parse_expr()?
        } else {
            self.parse_value(ValueSite::Stmt)?
        };

        // `f() else e: <block>` -- the guard in statement form: the success value is discarded.
        if self.else_is_guard() {
            let value = self.parse_else_guard(expr)?;
            return Ok(StmtKind::Let {
                target: Pattern::Wildcard,
                ty: None,
                value,
                is_const: false,
                doc: None,
            });
        }

        // A comma after the first lvalue introduces a *multi-target* form. `a, (b, c) := expr`
        // never reaches here (`try_parse_walrus_target` took it); a `:=` here has a target that is
        // not a name list, and `bind_target_error` reports it. The other shape is tuple assignment
        // `a, b = b, a` / `data[0], (data[1], x.f) = …` (lvalues, `=`; op==Eq only).
        // `for k, v in m:` never reaches here (parse_for handles it); a parenthesized `(a, b)` is a
        // single expr (no top-level comma).
        if self.ended_at_block() {
            return Ok(StmtKind::Expr(expr));
        }
        if self.peek() == &Token::Comma {
            let mut targets = vec![expr];
            while self.eat_in_line(&Token::Comma) {
                targets.push(self.parse_expr()?);
            }
            if self.eat_in_line(&Token::Walrus) {
                let _ = self.parse_value_list(ValueSite::DestructLet)?;
                let msg = "expected an identifier on the left of ':=' (destructuring binds names)";
                // The first target that is no name; `try_parse_walrus_target` took an all-name list.
                let at = targets
                    .iter()
                    .position(|t| !matches!(t.kind, ExprKind::Ident(_)))
                    .unwrap_or(0);
                return Err(self.bind_target_error(targets.swap_remove(at), msg));
            }
            // `a, b = b, a` — tuple assignment. Only `=` is allowed (compound `+=`, … with multiple
            // targets is rejected). The checker owns the arity of the right side.
            if self.ended_at_block() || self.peek() != &Token::Assign {
                let msg = if is_compound_assign(self.peek()) {
                    "compound assignment is not allowed with multiple targets; assign each separately"
                } else {
                    "expected '=' after a multi-target assignment list"
                };
                return Err(ParseError {
                    message: msg.to_string(),
                    span: self.cur_span(),
                });
            }
            let target_span = targets[0].span;
            let target = Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Tuple(targets),
                span: target_span,
            };
            if !is_assign_target(&target, true) {
                return Err(ParseError {
                    message: "invalid assignment target".to_string(),
                    span: target_span,
                });
            }
            self.advance(); // '='
            let (value, _) = self.parse_value_list(ValueSite::TupleAssign)?;
            self.reject_else_guard()?;
            return Ok(StmtKind::Assign {
                target,
                op: AssignOp::Eq,
                value,
            });
        }

        let op = match self.peek() {
            // `try_parse_walrus_target` took every valid `:=` target, so this arm only reports.
            Token::Walrus => {
                return Err(self.bind_target_error(expr, "left side of ':=' must be a name"));
            }
            Token::Assign => AssignOp::Eq,
            Token::PlusEq => AssignOp::PlusEq,
            Token::MinusEq => AssignOp::MinusEq,
            Token::StarEq => AssignOp::StarEq,
            Token::SlashEq => AssignOp::SlashEq,
            Token::PercentEq => AssignOp::PercentEq,
            Token::AmpEq => AssignOp::AmpEq,
            Token::PipeEq => AssignOp::PipeEq,
            Token::CaretEq => AssignOp::CaretEq,
            Token::ShlEq => AssignOp::ShlEq,
            Token::ShrEq => AssignOp::ShrEq,
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Assert
            | Token::Test
            | Token::Spawn
            | Token::Parallel
            | Token::Wait
            | Token::Yield
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => return Ok(StmtKind::Expr(expr)),
        };
        // only an assignable place — a name, field, or index — can be on the left of `= += -=`
        if !is_assign_target(&expr, false) {
            return Err(ParseError {
                message: "invalid assignment target".to_string(),
                span: expr.span,
            });
        }
        self.advance(); // the assignment operator
        let value = if matches!(op, AssignOp::Eq) {
            self.parse_value_list(ValueSite::Assign)?.0
        } else {
            self.parse_expr()?
        };
        self.reject_else_guard()?;
        Ok(StmtKind::Assign {
            target: expr,
            op,
            value,
        })
    }

    /// `test fn …` — consume the `test` modifier, parse the following `fn`, and tag it `is_test`.
    /// Shared by the free-fn site (`parse_stmt`) and the struct-method site (`parse_struct`).
    fn parse_test_fn(&mut self, allow_defaults: bool) -> PResult<FnDecl> {
        self.expect(&Token::Test)?;
        if !self.check(&Token::Fn) {
            return Err(self.err("`test` must be followed by `fn`".to_string()));
        }
        let mut decl = self.parse_fn(allow_defaults)?;
        decl.is_test = true;
        Ok(decl)
    }

    fn parse_fn(&mut self, allow_defaults: bool) -> PResult<FnDecl> {
        // doc = comment run immediately above the `fn` keyword line (covers methods too — they all
        // route through here). For `test fn` the modifier is on the same line, so the `fn` line works.
        let doc = self.doc_above(self.cur_span().line);
        self.expect(&Token::Fn)?;
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.expect(&Token::LParen)?;
        let params = self.parse_params(allow_defaults)?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        // Optional `where T: Bound, …` clause after the return type (before the body colon). The
        // checker merges these bounds into the matching `type_params` entry.
        let where_bounds = self.parse_where_bounds()?;
        // The body opens with a `:`; an INLINE body (`: <stmt>` on the same line) has a non-`Newline`
        // token immediately after that colon (an indented block is `Colon Newline Indent …`). `peek`
        // is the body `Colon` here, so `peek_at(1)` is the token right after it.
        let inline = self.peek() == &Token::Colon && self.peek_at(1) != &Token::Newline;
        // An inline body is one statement; a body that starts with `if` or `match` is an expression
        // statement (`parse_stmt_in`), and the first body token starts a whole value.
        if inline {
            self.open_value(ValueSite::FnBody, self.pos + 1);
        }
        let body = self.parse_block()?;
        let is_generator = body_contains_yield(&body);
        // An inline body whose single statement is a bare expression implicitly returns that
        // expression (mirroring a closure `fn(x): expr`). Inline non-expr statements (`: x = 5`,
        // `: return e`) stay as-is; a generator never implicitly returns its expr.
        let inline_expr_body = inline
            && !is_generator
            && matches!(body.as_slice(), [s] if matches!(s.kind, StmtKind::Expr(_)));
        Ok(FnDecl {
            name,
            name_span,
            type_params,
            where_bounds,
            params,
            ret,
            body,
            is_generator,
            is_test: false,
            inline_expr_body,
            doc,
        })
    }

    /// Optional `[T, U: Bound, …]` generic-parameter list immediately after a `fn`/`struct` name.
    /// Returns an empty vec when there's no `[`. Decl-site only — distinct from `parse_type`'s use
    /// of `[` for generic *arguments* (`List[int]`).
    fn parse_type_params(&mut self) -> PResult<Vec<TypeParam>> {
        let mut params = Vec::new();
        if self.eat(&Token::LBracket) {
            loop {
                let name_span = self.cur_span();
                let name = self.expect_ident()?;
                if params.iter().any(|p: &TypeParam| p.name == name) {
                    return Err(self.err(format!("duplicate type parameter '{name}'")));
                }
                // `T`, `T: Comparable`, multi-bound `T: Add + Mul`, or parameterized `S: Iterator[T]`.
                let mut bounds = Vec::new();
                if self.eat(&Token::Colon) {
                    bounds.push(self.parse_bound()?);
                    while self.eat(&Token::Plus) {
                        bounds.push(self.parse_bound()?);
                    }
                }
                params.push(TypeParam {
                    name,
                    name_span,
                    bounds,
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::RBracket)?;
        }
        Ok(params)
    }

    /// Optional `where T: Bound, U: Add + Mul, …` clause after a fn/native-fn signature. Returns an
    /// empty vec when there's no `where`. Each entry is `IDENT : bound (+ bound)*`, comma-separated,
    /// and the bound is required ([`WHERE_BOUND`])
    /// (no surrounding brackets) — the same per-entry shape as one `parse_type_params` entry, reusing
    /// [`parse_bound`]. Purely a bound-list: the checker (`fn_sig`) merges each entry's bounds into the
    /// matching declared type parameter, so a where entry is meaningful only for a `[T]`-declared param.
    fn parse_where_bounds(&mut self) -> PResult<Vec<TypeParam>> {
        let mut params = Vec::new();
        if self.eat(&Token::Where) {
            loop {
                let name_span = self.cur_span();
                let name = self.expect_ident()?;
                let mut bounds = Vec::new();
                if !self.eat(&Token::Colon)
                    || !matches!(self.peek(), Token::Ident(_) | Token::NoneKw)
                {
                    return Err(self.err(WHERE_BOUND.to_string()));
                }
                bounds.push(self.parse_bound()?);
                while self.eat(&Token::Plus) {
                    bounds.push(self.parse_bound()?);
                }
                params.push(TypeParam {
                    name,
                    name_span,
                    bounds,
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }
        Ok(params)
    }

    /// A single protocol bound on a type parameter: a name, optionally with `[T, …]` type arguments
    /// (`Iterator[T]`). The args reuse `parse_type`, so any type expression is accepted syntactically;
    /// only `Iterator` gives its args meaning in the checker. The name may be module-qualified
    /// (`m.Factory`, TICKET-173): the parser reads any dotted path and keeps its dotted source
    /// spelling, which `Checker::protocol_key` resolves; the checker refuses a 3+ segment path.
    fn parse_bound(&mut self) -> PResult<Bound> {
        let mut name = self.expect_ident()?;
        while self.eat(&Token::Dot) {
            name = format!("{name}.{}", self.expect_ident()?);
        }
        let mut args = Vec::new();
        if self.eat(&Token::LBracket) {
            args.push(self.parse_type()?);
            while self.eat(&Token::Comma) {
                args.push(self.parse_type()?);
            }
            self.expect(&Token::RBracket)?;
        }
        Ok(Bound { name, args })
    }

    /// A body-less method signature inside a `protocol` block: `fn name(params) -> ret` then NEWLINE.
    fn parse_fn_sig(&mut self) -> PResult<MethodSig> {
        self.expect(&Token::Fn)?;
        let name = self.expect_ident()?;
        self.expect(&Token::LParen)?;
        let params = self.parse_params(false)?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok(MethodSig { name, params, ret })
    }

    fn parse_protocol(&mut self) -> PResult<StmtKind> {
        let doc = self.doc_above(self.cur_span().line);
        self.expect(&Token::Protocol)?;
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        // `protocol Container[T]:` — optional type parameters, reusing the generic fn/struct parser.
        let type_params = self.parse_type_params()?;
        self.open_block()?;
        let mut methods = Vec::new();
        let mut embeds = Vec::new();
        self.skip_newlines();
        // A sole `pass` line = an empty protocol body (zero methods, zero embeds): an accept-all top
        // type (structural ⇒ satisfied by every type, like Go's `any`). `pass` must be the ONLY line:
        // a following method/embed OR a second `pass` is a parse error (modeled as the body being
        // exactly `pass NEWLINE DEDENT`).
        if self.check(&Token::Pass) {
            self.advance();
            self.expect_stmt_end()?;
            self.skip_newlines();
            if !self.check(&Token::Dedent) {
                return Err(
                    self.err("`pass` must be the only line in an empty protocol body".to_string())
                );
            }
            self.expect(&Token::Dedent)?;
            return Ok(StmtKind::Protocol {
                name,
                name_span,
                type_params,
                methods,
                embeds,
                doc,
            });
        }
        // A protocol body line is EITHER an `fn` signature OR an embed line (M22). An embed line is
        // one-or-more protocol refs joined by `+` (`Add + Sub`, `Iterator[T]`) — order-free, may
        // interleave with `fn` sigs. The two are unambiguous: an `fn` line starts with `FN`, an embed
        // line with an IDENT (a protocol name).
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            if self.check(&Token::Fn) {
                methods.push(self.parse_fn_sig()?);
            } else {
                embeds.push(self.parse_bound()?);
                while self.eat(&Token::Plus) {
                    embeds.push(self.parse_bound()?);
                }
            }
            self.expect_stmt_end()?;
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Protocol {
            name,
            name_span,
            type_params,
            methods,
            embeds,
            doc,
        })
    }

    /// A body-less C function signature inside an `extern "lib":` block: `fn name(params) -> ret`
    /// then NEWLINE. Mirrors [`parse_fn_sig`] but produces an [`ExternFn`] carrying its own span (for
    /// per-fn marshallability diagnostics) and forbids default arguments.
    fn parse_extern_fn(&mut self) -> PResult<ExternFn> {
        let span = self.cur_span();
        self.expect(&Token::Fn)?;
        let name = self.expect_ident()?;
        self.expect(&Token::LParen)?;
        let (params, c_variadic) = self.parse_params_ext(false, true)?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok(ExternFn {
            name,
            params,
            ret,
            span,
            c_variadic,
        })
    }

    /// Dispatch a top-level `native …` declaration: `native struct Name:` and `native enum Name:` own
    /// an indented block (like `struct`/`enum`); the bodyless `native fn`/`native ctor` forms are
    /// line-terminated. Dispatch on the word after `native`. `#[inline(never)]` keeps its
    /// `StmtKind`-sized call slots out of the recursive [`parse_stmt`] frame (see the call site).
    #[inline(never)]
    fn parse_native_decl(&mut self) -> PResult<StmtKind> {
        if self.peek_at(1) == &Token::Struct {
            self.parse_native_struct()
        } else if self.peek_at(1) == &Token::Enum {
            self.parse_native_enum()
        } else if self.peek_at(1) == &Token::Type {
            self.parse_native_type()
        } else {
            let k = self.parse_native(false)?;
            self.expect_stmt_end()?;
            Ok(k)
        }
    }

    /// `native type NAME` / `native type NAME = <type>` (TICKET-218): a line-terminated std-only
    /// type export. The checker restricts the underlying type to `int`/`float`.
    fn parse_native_type(&mut self) -> PResult<StmtKind> {
        let span = self.cur_span();
        self.expect(&Token::Native)?;
        self.expect(&Token::Type)?;
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        let underlying = if self.eat(&Token::Assign) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect_stmt_end()?;
        Ok(StmtKind::NativeType {
            name,
            name_span,
            underlying,
            span,
        })
    }

    /// A body-less `native fn NAME(params) -> ret` / `native ctor NAME(params) -> ret` declaration
    /// (prelude/std-only, top-level). Mirrors [`parse_extern_fn`] (bodyless, `parse_params(false)`,
    /// optional `-> ret`, NEWLINE-terminated by the caller) but discriminates on the `fn`/`ctor`
    /// keyword after `native` into a [`NativeKind`]. `ctor` is a plain identifier (not a keyword), so
    /// it is matched via `expect_ident`. An UNANNOTATED param → dynamic (`Ty::Unknown` in the
    /// checker); a missing `-> ret` → native/never (`Ty::Unknown` return).
    fn parse_native(&mut self, in_struct: bool) -> PResult<StmtKind> {
        let span = self.cur_span();
        self.expect(&Token::Native)?;
        let kind = if self.eat(&Token::Fn) {
            NativeKind::Fn
        } else {
            let word_span = self.cur_span();
            let word = self.expect_ident()?;
            if word != "ctor" {
                return Err(ParseError {
                    message: "expected 'fn' or 'ctor' after 'native'".to_string(),
                    span: word_span,
                });
            }
            NativeKind::Ctor
        };
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        // Optional method-/fn-own generic params: `native fn map[U](self, f: fn(T) -> U) -> List[U]`.
        // `parse_type_params` only consumes on a leading `[`, so a plain `native fn f(...)` is
        // unaffected. On a native METHOD these are the method's own `[U]` (distinct from the enclosing
        // `native struct`'s `[T]`); the harvest routes them through the generic-method path.
        let type_params = self.parse_type_params()?;
        self.expect(&Token::LParen)?;
        // Phase 4f — accept a DEFAULTED trailing param (`timeout_ms: int = 0`), the file-backed
        // spelling of std.request's optional-tail sig. The grammar already permits it (`<params>`
        // in `<nativeDecl>` admits `IDENT COLON <type> ASSIGN <expr>`); the default is a marker
        // only (desugar's `collect_module_reg` ignores `StmtKind::Native`, so it is never injected
        // at a call site). The checker still rejects a `native` decl in a USER module outright.
        let params = self.parse_params(true)?;
        // Phase 4c-followup — a `native fn` INSIDE a `native struct` body is an INSTANCE method and
        // MUST declare a leading bare `self` (untyped), mirroring user struct methods; harvest strips
        // it so the method-table sig is byte-identical to the pre-`self` spelling. A module-level
        // `native fn`/`native ctor` is a free function and must NOT take `self`. `self` is a plain
        // identifier, detected here by name — and it is valid ONLY as the first parameter.
        let first_is_self = matches!(params.first(), Some(p) if p.name == "self");
        if in_struct {
            if kind == NativeKind::Fn && !first_is_self {
                return Err(ParseError {
                    message: "native instance method must declare `self` as its first parameter \
                              (static native methods are not yet supported)"
                        .to_string(),
                    span: name_span,
                });
            }
        } else if first_is_self {
            return Err(ParseError {
                message: "module-level `native fn` cannot take `self`".to_string(),
                span: name_span,
            });
        }
        // `self` is the receiver and is valid ONLY as the first parameter (mirror user struct rules).
        if let Some(p) = params.iter().skip(1).find(|p| p.name == "self") {
            return Err(ParseError {
                message: "`self` is only valid as the first parameter".to_string(),
                span: p.name_span,
            });
        }
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        // Optional `where T: Bound` clause — a native METHOD sig (`native fn sort(self) -> nil where
        // T: Comparable`) whose bounds name the enclosing native struct's type param, enforced at each
        // call site by the checker's List/Map/Set method-dispatch arm.
        let where_bounds = self.parse_where_bounds()?;
        Ok(StmtKind::Native(NativeDecl {
            name,
            name_span,
            kind,
            type_params,
            params,
            ret,
            where_bounds,
            span,
        }))
    }

    /// `native struct NAME[T…]:` then an INDENT block of body-less field declarations — the TYPE-level
    /// analog of [`parse_native`] (`native fn`/`native ctor`). Prelude/std-only (the checker's hoist
    /// rejects it in a user module); a nested one is already rejected in [`parse_stmt`], which refuses
    /// every [`Self::item_keyword`] (`native` included) in a block. Fields-only for phase 4a: a method sig (`fn`/`test`) or a field
    /// `= default` inside the body is a parse error (bodyless native method sigs are a phase-4b
    /// follow-up). Mirrors [`parse_struct`]'s field loop but with no methods/defaults.
    fn parse_native_struct(&mut self) -> PResult<StmtKind> {
        let span = self.cur_span();
        self.expect(&Token::Native)?;
        self.expect(&Token::Struct)?;
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.open_block()?;
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut bodied_methods = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            // Phase 4c — a body-less `native fn` inside the struct body is a METHOD sig, harvested into
            // the native type's method table. Its runtime dispatch stays native (name-keyed).
            if self.check(&Token::Native) {
                let StmtKind::Native(decl) = self.parse_native(true)? else {
                    return Err(self.err(
                        "native struct methods must be `native fn` declarations".to_string(),
                    ));
                };
                self.expect_member_end("native method", &decl.name)?;
                methods.push(decl);
                self.skip_newlines();
                continue;
            }
            // Phase 4c-followup — a BODIED `fn name(self, …) -> T: <body>` is a Chezzi method on the
            // native handle (mixed with the bodyless `native fn` sigs). Unlike them it IS compiled to
            // bytecode (routed via `Program::native_methods`), so `parse_fn` (which flags a generator
            // via a `yield` in the body) parses it into `bodied_methods`. A `test fn` stays rejected.
            if self.check(&Token::Fn) {
                bodied_methods.push(self.parse_fn(true)?);
                self.skip_newlines();
                continue;
            }
            if self.check(&Token::Test) {
                return Err(self.err(
                    "a native struct cannot carry a `test fn` (declare fields, `native fn` sigs, or bodied `fn` methods)"
                        .to_string(),
                ));
            }
            let fname_span = self.cur_span();
            let fname = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            let ty = self.parse_type()?;
            if self.at_type_assign() {
                return Err(self.err(
                    "native struct fields cannot have default values (the layout is native)"
                        .to_string(),
                ));
            }
            self.expect_member_end("native struct field", &fname)?;
            fields.push(Field {
                name: fname,
                name_span: fname_span,
                ty,
                default: None,
            });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::NativeStruct {
            name,
            name_span,
            type_params,
            fields,
            methods,
            bodied_methods,
            span,
        })
    }

    /// `native enum NAME[T…]:` then an INDENT block of body-less variants (optionally followed by
    /// body-less `native fn` methods) — the ENUM-level analog of [`parse_native_struct`]. Prelude/std-
    /// only (the checker's hoist rejects it in a user module); a nested one is already rejected in
    /// [`parse_stmt`], which refuses every [`Self::item_keyword`] (`native` included) in a block. Reuses [`parse_enum`]'s variant loop (an IDENT
    /// with an optional `(typeList)` payload) and [`parse_native`]`(true)` for the self-declaring
    /// methods; like `parse_enum`, variants must precede methods, and a plain `fn`/`test` (with a body)
    /// is a parse error. Phase 5b: file-backs the reserved `Option`/`Result` variant shape.
    ///
    /// `#[inline(never)]` — like a cold decl-parsing helper, this must NOT be inlined into the mutually
    /// recursive [`parse_stmt`] (its several `Vec` locals would enlarge every `parse_stmt` frame on the
    /// nested-block recursion path, tightening the `MAX_DEPTH`-vs-stack margin the `deep_nesting_errors_not_crash`
    /// guard test relies on). It is called at most once per top-level `native enum`, so out-of-lining is free.
    #[inline(never)]
    fn parse_native_enum(&mut self) -> PResult<StmtKind> {
        let span = self.cur_span();
        self.expect(&Token::Native)?;
        self.expect(&Token::Enum)?;
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.open_block()?;
        let mut variants = Vec::new();
        let mut methods = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            // A body-less `native fn` inside the enum body is a METHOD sig, harvested into the enum's
            // method table (leading bare `self`, which harvest strips). A PLAIN `fn`/`test` (with a
            // body) is rejected: the runtime construction/dispatch stay native, so a native enum never
            // carries a compiled method.
            if self.check(&Token::Native) {
                let StmtKind::Native(decl) = self.parse_native(true)? else {
                    return Err(self
                        .err("native enum methods must be `native fn` declarations".to_string()));
                };
                self.expect_member_end("native method", &decl.name)?;
                methods.push(decl);
                self.skip_newlines();
                continue;
            }
            if self.check(&Token::Fn) || self.check(&Token::Test) {
                return Err(self.err(
                    "native enum methods are not supported (declare variants or `native fn` sigs only)"
                        .to_string(),
                ));
            }
            // A variant after a method is a structuring error (mirror `parse_enum`).
            if !methods.is_empty() {
                return Err(self.err("enum variants must be declared before methods".to_string()));
            }
            let vname_span = self.cur_span();
            let vname = self.expect_ident_or_none()?;
            let mut payload = Vec::new();
            if self.eat(&Token::LParen) {
                if !self.check(&Token::RParen) {
                    loop {
                        payload.push(self.parse_type()?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Token::RParen)?;
            }
            self.expect_member_end("enum variant", &vname)?;
            variants.push(Variant {
                name: vname,
                name_span: vname_span,
                payload,
            });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::NativeEnum {
            name,
            name_span,
            type_params,
            variants,
            methods,
            span,
        })
    }

    /// `extern "lib":` then an INDENT block of body-less C function signatures. Mirrors
    /// [`parse_protocol`]: the library name is a string literal, the body is `open_block` + a loop of
    /// `parse_extern_fn` until `Dedent`.
    fn parse_extern(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Extern)?;
        let lib = self.expect_str()?;
        self.open_block()?;
        let mut fns = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            fns.push(self.parse_extern_fn()?);
            self.expect_stmt_end()?;
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Extern { lib, fns })
    }

    /// Comma-separated `name[: Type]` until (but not consuming) the closing `)`.
    /// Parse a parameter list. `allow_defaults` is true for free `fn` declarations, for struct /
    /// enum / native-struct methods (every `parse_fn` / `parse_test_fn` caller passes `true`) and
    /// for `native fn` decls; closures, `extern` fns and protocol signatures pass `false` and reject
    /// `= default`. A defaulted param may not be followed by
    /// a required (non-defaulted) one. A default may be any expression; the desugar pass rejects one
    /// that references another parameter (a non-literal default is compiled as a zero-arg fn in its
    /// DEFINING module, where no parameter is in scope).
    fn parse_params(&mut self, allow_defaults: bool) -> PResult<Vec<Param>> {
        Ok(self.parse_params_ext(allow_defaults, false)?.0)
    }

    /// [`parse_params`], plus whether the list ended in a bare `...` (C varargs). That `...` is
    /// legal only as the LAST parameter, and only where `allow_c_variadic` (an `extern` fn).
    fn parse_params_ext(
        &mut self,
        allow_defaults: bool,
        allow_c_variadic: bool,
    ) -> PResult<(Vec<Param>, bool)> {
        let mut params = Vec::new();
        let mut seen_default = false;
        let mut seen_variadic = false;
        let mut c_variadic = false;
        if !self.check(&Token::RParen) {
            loop {
                // A bare `...` (no name follows): C varargs.
                if self.check(&Token::DotDotDot)
                    && matches!(self.peek_at(1), Token::RParen | Token::Comma)
                {
                    if !allow_c_variadic {
                        return Err(self.err(
                            "a bare `...` (C varargs) is only allowed as the last parameter of an \
                             extern fn"
                                .to_string(),
                        ));
                    }
                    if self.peek_at(1) == &Token::Comma {
                        return Err(self.err(
                            "C varargs `...` must be the last parameter of an extern fn"
                                .to_string(),
                        ));
                    }
                    self.advance();
                    c_variadic = true;
                    break;
                }
                let name_span = self.cur_span();
                // A leading `...` marks a variadic parameter (`fn f(...args: T)`). It collects the
                // surplus trailing positional args into a `List[T]`; everything after it is
                // keyword-only. At most one per signature; it must carry an element type and may not
                // carry a default. Variadics are allowed only where defaults are (free fns, methods,
                // native decls) — closures / extern / protocol sigs reject them (fixed-arity).
                let is_variadic = self.eat(&Token::DotDotDot);
                if is_variadic {
                    if !allow_defaults {
                        return Err(
                            self.err("variadic parameters are not supported here".to_string())
                        );
                    }
                    if seen_variadic {
                        return Err(
                            self.err("at most one variadic parameter is allowed".to_string())
                        );
                    }
                    seen_variadic = true;
                }
                let name = self.expect_ident()?;
                let ty = if self.eat(&Token::Colon) {
                    // `const` is a binding modifier (locals + module globals), never valid on a
                    // parameter — reject it explicitly rather than letting `parse_type` fail obscurely.
                    if self.peek() == &Token::Const {
                        return Err(self.err(
                            "`const` is a binding modifier, not valid on parameters".to_string(),
                        ));
                    }
                    Some(self.parse_type()?)
                } else {
                    None
                };
                if is_variadic && ty.is_none() {
                    return Err(self.err(format!(
                        "variadic parameter '{name}' must declare an element type (`...{name}: T`)"
                    )));
                }
                let default = if self.eat_type_assign() {
                    if !allow_defaults {
                        return Err(
                            self.err("default arguments are not supported here".to_string())
                        );
                    }
                    if is_variadic {
                        return Err(
                            self.err(format!("variadic parameter '{name}' cannot have a default"))
                        );
                    }
                    // Any expression is allowed as a default; the desugar pass rejects one that
                    // references another parameter (it is cloned into the caller's scope at the call
                    // site, where parameters are not bound).
                    let e = self.parse_value(ValueSite::ParamDefault)?;
                    Some(e)
                } else {
                    None
                };
                if default.is_some() {
                    seen_default = true;
                } else if seen_default && !seen_variadic {
                    return Err(self.err(format!(
                        "required parameter '{name}' cannot follow a default parameter"
                    )));
                }
                params.push(Param {
                    name,
                    name_span,
                    ty,
                    default,
                    is_variadic,
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
                // optional trailing comma: a `)` right after a comma ends the parameter list
                if self.check(&Token::RParen) {
                    break;
                }
            }
        }
        Ok((params, c_variadic))
    }

    fn parse_struct(&mut self) -> PResult<StmtKind> {
        let doc = self.doc_above(self.cur_span().line);
        self.expect(&Token::Struct)?;
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.open_block()?;
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut seen_default = false;
        self.skip_newlines();
        // A sole `pass` line = an empty struct body (zero fields, zero methods): the ctor `S()` takes
        // no args. `pass` must be the ONLY line — a following field/method OR a second `pass` errors.
        if self.check(&Token::Pass) {
            self.advance();
            self.expect_stmt_end()?;
            self.skip_newlines();
            if !self.check(&Token::Dedent) {
                return Err(
                    self.err("`pass` must be the only line in an empty struct body".to_string())
                );
            }
            self.expect(&Token::Dedent)?;
            return Ok(StmtKind::Struct {
                name,
                name_span,
                type_params,
                fields,
                methods,
                doc,
            });
        }
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            if self.check(&Token::Fn) {
                // Methods accept default params (like free fns); the desugar pass fills omitted args
                // / reorders named args at method call sites and rejects param-referencing defaults.
                methods.push(self.parse_fn(true)?);
            } else if self.check(&Token::Test) {
                // `test fn name(self)` — a suite test method.
                methods.push(self.parse_test_fn(true)?);
            } else {
                let fname_span = self.cur_span();
                let fname = self.expect_ident()?;
                self.expect(&Token::Colon)?;
                let ty = self.parse_type()?;
                let default = if self.eat_type_assign() {
                    // Any expression is allowed; the desugar pass rejects one that references another
                    // field (it is cloned into the caller's scope at the constructor call site).
                    let e = self.parse_value(ValueSite::FieldDefault)?;
                    Some(e)
                } else {
                    None
                };
                self.expect_member_end("struct field", &fname)?;
                if default.is_some() {
                    seen_default = true;
                } else if seen_default {
                    return Err(self.err(format!(
                        "required field '{fname}' cannot follow a default field"
                    )));
                }
                fields.push(Field {
                    name: fname,
                    name_span: fname_span,
                    ty,
                    default,
                });
            }
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Struct {
            name,
            name_span,
            type_params,
            fields,
            methods,
            doc,
        })
    }

    /// `type Name = <type>` — a transparent type alias (one line, terminated by the caller).
    fn parse_type_alias(&mut self) -> PResult<StmtKind> {
        let doc = self.doc_above(self.cur_span().line);
        self.expect(&Token::Type)?;
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        self.expect(&Token::Assign)?;
        let ty = self.parse_type()?;
        Ok(StmtKind::TypeAlias {
            name,
            name_span,
            ty,
            doc,
        })
    }

    fn parse_enum(&mut self) -> PResult<StmtKind> {
        let doc = self.doc_above(self.cur_span().line);
        self.expect(&Token::Enum)?;
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.open_block()?;
        let mut variants = Vec::new();
        let mut methods = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            if self.check(&Token::Pass) {
                // An empty enum (uninhabited type) is deliberately unsupported: an enum needs at
                // least one variant. Reject `pass` here with a clear message (unlike struct/protocol,
                // where a sole `pass` is a valid empty body).
                return Err(self.err(
                    "empty enums are not supported; an enum needs at least one variant".to_string(),
                ));
            }
            if self.check(&Token::Fn) {
                // Methods accept default params (like free fns / struct methods); the desugar pass
                // fills omitted args / reorders named args at method call sites by method name.
                methods.push(self.parse_fn(true)?);
            } else if self.check(&Token::Test) {
                // Enum test *suites* are not supported: the compiler/test-runner build SuiteInfo +
                // discovery only for structs, so an enum `test fn` would silently never run (a
                // false-green). Reject at parse time until enum suites are wired (follow-up).
                return Err(self
                    .err("test methods are not supported on enums (only on structs)".to_string()));
            } else {
                // Variants must precede methods: a variant after a method is a structuring error.
                if !methods.is_empty() {
                    return Err(
                        self.err("enum variants must be declared before methods".to_string())
                    );
                }
                let vname_span = self.cur_span();
                let vname = self.expect_ident()?;
                let mut payload = Vec::new();
                if self.eat(&Token::LParen) {
                    if !self.check(&Token::RParen) {
                        loop {
                            payload.push(self.parse_type()?);
                            if !self.eat(&Token::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(&Token::RParen)?;
                }
                self.expect_member_end("enum variant", &vname)?;
                variants.push(Variant {
                    name: vname,
                    name_span: vname_span,
                    payload,
                });
            }
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Enum {
            name,
            name_span,
            type_params,
            variants,
            methods,
            doc,
        })
    }

    /// The body of an `if` / `elif` (the cursor is on its ':'), and whether a bare `else:` or `elif`
    /// follows its one statement on the same line.
    fn if_body(&mut self) -> PResult<(Block, bool)> {
        // the cursor is on the ':'; `parse_stmt_in` runs the body's one statement one level down
        let start = self.pos + 1;
        let outer = self.if_line.replace((start, self.depth + 1));
        let body = self.parse_block();
        self.if_line = outer;
        let body = body?;
        Ok((body, self.bare_else_on_line(start)))
    }

    fn parse_if(&mut self) -> PResult<StmtKind> {
        let span = self.cur_span();
        self.expect(&Token::If)?;
        let cond = self.parse_expr()?;
        let (mut body, on_line) = self.if_body()?;
        if on_line {
            // TICKET-242 -- `if c: a() else: b()` on one line is the if-expression with its value
            // discarded: the body is its first branch, and the expression form's tail reads the rest.
            let Some(Stmt {
                kind: StmtKind::Expr(then),
                ..
            }) = body.pop()
            else {
                return Err(self.err(IF_BRANCH.to_string()));
            };
            let expr = self.parse_if_expr_tail(span, cond, then)?;
            // anything left on the line means the else branch was not an expression
            // (`else: x = 2`, `else: return 2`)
            self.expect_stmt_end().map_err(|e| ParseError {
                message: IF_BRANCH.to_string(),
                span: e.span,
            })?;
            return Ok(StmtKind::Expr(expr));
        }
        let mut branches = vec![(cond, body)];
        let mut else_block = None;
        loop {
            // An indented body lands the cursor on `elif`/`else` directly (the `Dedent` precedes it);
            // an inline body (`if x: y`) leaves a `Newline` first, so step over newlines before testing.
            self.skip_newlines();
            if self.check(&Token::Elif) {
                // `elif` → another branch (Python-style single keyword; `else if` no longer parses)
                self.expect(&Token::Elif)?;
                let cond = self.parse_expr()?;
                let (body, on_line) = self.if_body()?;
                if on_line {
                    return Err(self.err(ELIF_LINE.to_string()));
                }
                branches.push((cond, body));
            } else if self.check(&Token::Else) {
                self.expect(&Token::Else)?;
                else_block = Some(self.parse_block()?);
                break;
            } else {
                break;
            }
        }
        Ok(StmtKind::If {
            branches,
            else_block,
        })
    }

    fn parse_for(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::For)?;
        // One binding (`for x in …`), or a comma-separated target (`for k, (a, b) in m:`) that
        // unpacks each element. The checker enforces which iterands yield which shapes.
        let target = self.parse_loop_target()?;
        self.expect(&Token::In)?;
        let iter = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(StmtKind::For { target, iter, body })
    }

    /// Parse one comprehension `for` clause: `for <target> in <iter> [if <guard>]…`.
    /// Zero or more `if` guards may follow (Python allows an `if` after any clause, even a chain).
    /// The caller loops this while the next token is `for`, then consumes the closing bracket/brace.
    /// Mirrors `parse_for`'s var/`in`/iter parsing.
    fn parse_comp_clause(&mut self) -> PResult<CompClause> {
        self.expect(&Token::For)?;
        let target = self.parse_loop_target()?;
        self.expect(&Token::In)?;
        let iter = Box::new(self.parse_expr()?);
        let mut guards = Vec::new();
        while self.eat(&Token::If) {
            guards.push(self.parse_expr()?);
        }
        Ok(CompClause {
            target,
            iter,
            guards,
        })
    }

    /// Parse one or more comprehension `for` clauses (the caller has already parsed the element and
    /// confirmed the next token is `for`). Returns clauses in source order (first = outermost loop).
    fn parse_comp_clauses(&mut self) -> PResult<Vec<CompClause>> {
        let mut clauses = vec![self.parse_comp_clause()?];
        while self.check(&Token::For) {
            clauses.push(self.parse_comp_clause()?);
        }
        Ok(clauses)
    }

    fn parse_while(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::While)?;
        let cond = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(StmtKind::While { cond, body })
    }

    fn parse_match(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Match)?;
        let scrutinee = self.parse_expr()?;
        self.open_block()?;
        let mut arms = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            let arm_span = self.cur_span();
            let pattern = self.parse_pattern()?;
            let guard = if self.eat(&Token::If) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            let body = self.parse_block()?;
            arms.push(MatchArm {
                span: arm_span,
                pattern,
                guard,
                body,
            });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Match { scrutinee, arms })
    }

    /// Expression-position `match` (keyword already consumed): `match scrut:` then indented
    /// `pattern: value-expr` arms. Each arm body is a single expression (not a block).
    fn parse_match_expr(&mut self, span: Span) -> PResult<Expr> {
        if !self.block_ok(self.pos - 1) {
            return Err(ParseError {
                message: BLOCK_VALUE.to_string(),
                span,
            });
        }
        let scrutinee = self.parse_expr()?;
        self.expect(&Token::Colon)?;
        self.request_layout(self.pos - 1)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        let mut arms = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            let arm_span = self.cur_span();
            let pattern = self.parse_pattern()?;
            let guard = if self.eat(&Token::If) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect(&Token::Colon)?;
            let body = self.parse_value(ValueSite::MatchArm)?;
            self.expect_stmt_end()?;
            arms.push(MatchExprArm {
                span: arm_span,
                pattern,
                guard,
                body,
            });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span,
        })
    }

    /// Expression-position `if` (keyword already consumed): inline `if cond: then else: els`.
    /// `else` is mandatory; the `then`/`els` expressions stop at the `else` keyword / line end.
    fn parse_if_expr(&mut self, span: Span) -> PResult<Expr> {
        let cond = self.parse_expr()?;
        self.expect(&Token::Colon)?;
        let then = self.if_branch()?;
        self.parse_if_expr_tail(span, cond, then)
    }

    /// One branch of an if-expression. A branch that does not start an expression (`return`, `break`)
    /// reports [`IF_BRANCH`].
    fn if_branch(&mut self) -> PResult<Expr> {
        let start = self.pos;
        self.parse_expr().map_err(|e| {
            if self.pos <= start + 1 && e.span == self.toks[start].span && e.message != BLOCK_VALUE
            {
                ParseError {
                    message: IF_BRANCH.to_string(),
                    span: e.span,
                }
            } else {
                e
            }
        })
    }

    /// The `elif` / `else:` tail of an if-expression whose condition and first branch are parsed.
    /// The one-line statement form ([`Self::parse_if`]) enters here too.
    fn parse_if_expr_tail(&mut self, span: Span, cond: Expr, then: Expr) -> PResult<Expr> {
        // `elif ...` chains: recurse into another expression-`if` for the else-branch
        // (right-associative nesting), mirroring statement-form `parse_if`. Otherwise the
        // plain `else: <expr>` tail — `else` is mandatory (no optional-else leak).
        let els = if self.check(&Token::Elif) {
            let elif_span = self.cur_span();
            self.expect(&Token::Elif)?;
            self.parse_if_expr(elif_span)?
        } else {
            self.expect(&Token::Else)?;
            self.expect(&Token::Colon)?;
            self.if_branch()?
        };
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::IfElse {
                cond: Box::new(cond),
                then: Box::new(then),
                els: Box::new(els),
            },
            span,
        })
    }

    /// A bare `else:` or `elif` on the logical line that started at token `start`. The lexer emits no
    /// layout token inside brackets, so "no `Newline` / `Indent` / `Dedent` since `start`" is "same
    /// logical line". A body that ends in a block expression (`x := match n:` + arms) ends at a
    /// `Dedent`, and an indented body starts with a `Newline`; both make it false.
    fn bare_else_on_line(&self, start: usize) -> bool {
        (self.check(&Token::Elif) || (self.check(&Token::Else) && self.peek_at(1) == &Token::Colon))
            && !self.toks[start..self.pos]
                .iter()
                .any(|t| matches!(t.kind, Token::Newline | Token::Indent | Token::Dedent))
    }

    /// TICKET-242 -- a bare `else:` or `elif` that ends the one statement of an inline `if` body: it
    /// belongs to the `if`. The one test the statement-level `else` readers share. The depth test
    /// keeps a statement of a nested block (a `recover:` body, a guard body) out: it runs deeper,
    /// and keeps the `else` reading it has everywhere else.
    fn if_line_else(&self) -> bool {
        self.if_line
            .is_some_and(|(start, depth)| self.depth == depth && self.bare_else_on_line(start))
    }

    /// An `else` here is THIS statement's guard. After a block expression the `else` starts a line at
    /// the column of the line that holds the block's head; an inline-body statement starts deeper, so
    /// that `else` belongs to the header (`if`, `elif`, a `wait` arm), not to the statement.
    fn else_is_guard(&self) -> bool {
        self.check(&Token::Else)
            && !self.if_line_else()
            && !(self.ended_at_block() && self.stmt_inline)
    }

    /// `<value> else e: <block>` -- the `else` guard, read only after a let's whole value (an if
    /// expression has already taken its own `else` inside `parse_expr`). Returns `value` unchanged
    /// when no `else` follows.
    fn parse_else_guard(&mut self, value: Expr) -> PResult<Expr> {
        if !self.else_is_guard() {
            return Ok(value);
        }
        let span = self.cur_span();
        self.advance();
        let err = match self.peek() {
            Token::Ident(_) => {
                let name_span = self.cur_span();
                Some((self.expect_ident()?, name_span))
            }
            Token::Colon => None,
            t @ Token::Int(_)
            | t @ Token::IntMinMagnitude
            | t @ Token::Float(_)
            | t @ Token::Str(_)
            | t @ Token::Bytes(_)
            | t @ Token::RawStr(_)
            | t @ Token::Fn
            | t @ Token::Return
            | t @ Token::If
            | t @ Token::Else
            | t @ Token::Elif
            | t @ Token::For
            | t @ Token::While
            | t @ Token::In
            | t @ Token::Break
            | t @ Token::Continue
            | t @ Token::Pass
            | t @ Token::Struct
            | t @ Token::Enum
            | t @ Token::Protocol
            | t @ Token::Type
            | t @ Token::Match
            | t @ Token::Recover
            | t @ Token::Defer
            | t @ Token::Assert
            | t @ Token::Test
            | t @ Token::Spawn
            | t @ Token::Parallel
            | t @ Token::Wait
            | t @ Token::Yield
            | t @ Token::Import
            | t @ Token::Extern
            | t @ Token::Native
            | t @ Token::From
            | t @ Token::As
            | t @ Token::Const
            | t @ Token::And
            | t @ Token::Or
            | t @ Token::Not
            | t @ Token::True
            | t @ Token::False
            | t @ Token::NoneKw
            | t @ Token::Where
            | t @ Token::Plus
            | t @ Token::Minus
            | t @ Token::Star
            | t @ Token::Slash
            | t @ Token::Percent
            | t @ Token::Assign
            | t @ Token::Walrus
            | t @ Token::EqEq
            | t @ Token::NotEq
            | t @ Token::Lt
            | t @ Token::LtEq
            | t @ Token::Gt
            | t @ Token::GtEq
            | t @ Token::PlusEq
            | t @ Token::MinusEq
            | t @ Token::StarEq
            | t @ Token::SlashEq
            | t @ Token::PercentEq
            | t @ Token::AmpEq
            | t @ Token::PipeEq
            | t @ Token::CaretEq
            | t @ Token::ShlEq
            | t @ Token::ShrEq
            | t @ Token::Arrow
            | t @ Token::Pipe
            | t @ Token::Question
            | t @ Token::QuestionDot
            | t @ Token::QuestionQuestion
            | t @ Token::Bang
            | t @ Token::Amp
            | t @ Token::Caret
            | t @ Token::BitOr
            | t @ Token::Shl
            | t @ Token::Shr
            | t @ Token::LParen
            | t @ Token::RParen
            | t @ Token::LBracket
            | t @ Token::RBracket
            | t @ Token::LBrace
            | t @ Token::RBrace
            | t @ Token::Comma
            | t @ Token::Dot
            | t @ Token::DotDot
            | t @ Token::DotDotEq
            | t @ Token::DotDotDot
            | t @ Token::Newline
            | t @ Token::Indent
            | t @ Token::Dedent
            | t @ Token::Eof => {
                return Err(self
                    .keyword_name_error(t)
                    .unwrap_or_else(|| self.err("expected ':'".to_string())));
            }
        };
        let body = self.parse_block()?;
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::ElseGuard {
                value: Box::new(value),
                err,
                body,
            },
            span,
        })
    }

    /// An `else` after the value of an assignment or a `return`: no guard exists there.
    fn reject_else_guard(&self) -> PResult<()> {
        if self.else_is_guard() {
            return Err(self.err(ELSE_GUARD_PLACE.to_string()));
        }
        Ok(())
    }

    /// Expression-position `recover:` (keyword already consumed). `recover:` then a line break is the
    /// block form (it has type `!`); `recover:` then an expression on the same line is the inline
    /// form (`recover: expr`, type `T!`), an ordinary expression that extends to the right like a
    /// lambda body.
    fn parse_recover_expr(&mut self, span: Span) -> PResult<Expr> {
        let kw = self.pos - 1;
        let next = &self.toks[(self.pos + 1).min(self.toks.len() - 1)];
        let block =
            self.check(&Token::Colon) && (next.kind == Token::Newline || next.indent.is_some());
        let (body, form) = if block {
            if !self.block_ok(kw) {
                return Err(ParseError {
                    message: BLOCK_VALUE.to_string(),
                    span,
                });
            }
            self.request_layout(self.pos)?;
            (self.parse_block()?, RecoverForm::Block)
        } else {
            self.expect(&Token::Colon)?;
            let body_span = self.cur_span();
            let value = self.parse_expr()?;
            (
                vec![Stmt {
                    kind: StmtKind::Expr(value),
                    span: body_span,
                }],
                RecoverForm::Inline,
            )
        };
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Recover(body, form),
            span,
        })
    }

    /// Parse a top-level match-arm pattern. A bare identifier here names a variant (`None`,
    /// `Point`); `(...)` is a tuple pattern. Nested positions use [`parse_subpattern`], where a bare
    /// identifier is a binding instead.
    fn parse_pattern(&mut self) -> PResult<Pattern> {
        self.parse_pattern_impl(true)
    }

    /// Parse a sub-pattern (a variant payload slot or a tuple element). A bare identifier here is a
    /// binding name; `Name(...)` is a nested variant; `(...)` a nested tuple.
    fn parse_subpattern(&mut self) -> PResult<Pattern> {
        self.parse_pattern_impl(false)
    }

    /// Parse one-or-more primaries separated by `|` (Token::BitOr), the or-alternation level.
    /// Returns `Pattern::Or` only when there is more than one alternative; a single primary is
    /// returned unchanged (zero AST regression). Works at both top-arm and sub-pattern positions
    /// (the `top` flag flows into each primary so every alternative disambiguates identically).
    fn parse_pattern_impl(&mut self, top: bool) -> PResult<Pattern> {
        // Depth guard — the fifth recursive entry point (mirrors parse_stmt/parse_type/parse_bp/
        // parse_unary). Nested variant payloads (`Some(Some(...))`) and tuple elements (`((( ... )))`)
        // recurse through here per level; without this a deeply-nested pattern overflows the native
        // stack (SIGABRT) before check/run. Leaks the increment on the `?`/guard-Err paths — the
        // pattern callers (parse_match/parse_match_expr) call with `?` and never backtrack, so that
        // matches the four existing sites.
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("pattern nested too deeply".to_string()));
        }
        let mut alts = vec![self.parse_pattern_primary(top)?];
        while self.eat(&Token::BitOr) {
            alts.push(self.parse_pattern_primary(top)?);
        }
        let result = if alts.len() == 1 {
            alts.pop().expect("one alternative")
        } else {
            Pattern::Or(alts)
        };
        self.depth -= 1;
        Ok(result)
    }

    fn parse_pattern_primary(&mut self, top: bool) -> PResult<Pattern> {
        // Literal patterns: int / str / bool. Float is intentionally not a pattern. A leading `-`
        // begins a negative int literal/range pattern (int-only; `-3.0` routes through
        // `expect_pattern_int` → `expect_int` and is rejected as "expected integer, found float").
        match self.peek() {
            Token::Int(_) | Token::Minus => {
                let start = self.expect_pattern_int()?;
                // `start..end` — a half-open integer range pattern (matches `start <= v < end`);
                // `start..=end` is the inclusive form.
                let inclusive = self.check(&Token::DotDotEq);
                if self.eat(&Token::DotDot) || self.eat(&Token::DotDotEq) {
                    let end = self.expect_pattern_int()?;
                    return Ok(Pattern::Range {
                        start,
                        end,
                        inclusive,
                    });
                }
                return Ok(Pattern::Literal(LitPattern::Int(start)));
            }
            // A string pattern is decoded exactly like the same literal in expression position
            // (TICKET-139/W14-22): `{{`/`}}` are literal braces and an all-digit hole is text
            // (DEC-018), but a real `{expr}` hole is a parse error -- a pattern matches literals,
            // it never interpolates. This must stay HERE: `Token::RawStr` below lands in the same
            // `LitPattern::Str`, so no later phase can tell `r"{x}"` from a holed `"{x}"`.
            Token::Str(s) => {
                let Some(text) = s.plain() else {
                    return Err(self.err(
                        "a string pattern cannot interpolate: `{...}` is a hole, not text; write `{{` and `}}` for literal braces, or bind the value and compare it in a guard".to_string(),
                    ));
                };
                let text = text.to_string();
                self.advance();
                return Ok(Pattern::Literal(LitPattern::Str(text)));
            }
            // A raw string is an ordinary `str` literal in pattern position too.
            Token::RawStr(s) => {
                let s = s.clone();
                self.advance();
                return Ok(Pattern::Literal(LitPattern::Str(s)));
            }
            Token::True => {
                self.advance();
                return Ok(Pattern::Literal(LitPattern::Bool(true)));
            }
            Token::False => {
                self.advance();
                return Ok(Pattern::Literal(LitPattern::Bool(false)));
            }
            // A parenthesised group is a tuple pattern (gap #15).
            Token::LParen => return self.parse_tuple_pattern(),
            // `?p` / `!p` -- a carrier pattern. The prefixes are collected in a loop (`??` lexes as
            // one token and is two nested `?`), so a long run costs no native stack. The operand
            // is always a sub-position: a bare name there binds.
            Token::Question | Token::QuestionQuestion | Token::Bang => {
                use crate::ast::CarrierTag;
                let mut tags = Vec::new();
                loop {
                    match self.peek() {
                        Token::Question => tags.push(CarrierTag::Present),
                        Token::QuestionQuestion => {
                            tags.push(CarrierTag::Present);
                            tags.push(CarrierTag::Present);
                        }
                        Token::Bang => tags.push(CarrierTag::Error),
                        Token::Int(_)
                        | Token::IntMinMagnitude
                        | Token::Float(_)
                        | Token::Str(_)
                        | Token::Bytes(_)
                        | Token::RawStr(_)
                        | Token::Ident(_)
                        | Token::Fn
                        | Token::Return
                        | Token::If
                        | Token::Else
                        | Token::Elif
                        | Token::For
                        | Token::While
                        | Token::In
                        | Token::Break
                        | Token::Continue
                        | Token::Pass
                        | Token::Struct
                        | Token::Enum
                        | Token::Protocol
                        | Token::Type
                        | Token::Match
                        | Token::Recover
                        | Token::Defer
                        | Token::Assert
                        | Token::Test
                        | Token::Spawn
                        | Token::Parallel
                        | Token::Wait
                        | Token::Yield
                        | Token::Import
                        | Token::Extern
                        | Token::Native
                        | Token::From
                        | Token::As
                        | Token::Const
                        | Token::And
                        | Token::Or
                        | Token::Not
                        | Token::True
                        | Token::False
                        | Token::NoneKw
                        | Token::Where
                        | Token::Plus
                        | Token::Minus
                        | Token::Star
                        | Token::Slash
                        | Token::Percent
                        | Token::Assign
                        | Token::Walrus
                        | Token::EqEq
                        | Token::NotEq
                        | Token::Lt
                        | Token::LtEq
                        | Token::Gt
                        | Token::GtEq
                        | Token::PlusEq
                        | Token::MinusEq
                        | Token::StarEq
                        | Token::SlashEq
                        | Token::PercentEq
                        | Token::AmpEq
                        | Token::PipeEq
                        | Token::CaretEq
                        | Token::ShlEq
                        | Token::ShrEq
                        | Token::Arrow
                        | Token::Pipe
                        | Token::QuestionDot
                        | Token::Amp
                        | Token::Caret
                        | Token::BitOr
                        | Token::Shl
                        | Token::Shr
                        | Token::LParen
                        | Token::RParen
                        | Token::LBracket
                        | Token::RBracket
                        | Token::LBrace
                        | Token::RBrace
                        | Token::Comma
                        | Token::Colon
                        | Token::Dot
                        | Token::DotDot
                        | Token::DotDotEq
                        | Token::DotDotDot
                        | Token::Newline
                        | Token::Indent
                        | Token::Dedent
                        | Token::Eof => break,
                    }
                    self.advance();
                }
                let mut pat = if self.check(&Token::LParen) {
                    self.parse_tuple_pattern()?
                } else {
                    self.parse_pattern_primary(false)?
                };
                for tag in tags.into_iter().rev() {
                    pat = Pattern::Carrier {
                        tag,
                        inner: Box::new(pat),
                        id: crate::ast::NodeId::fresh(),
                    };
                }
                return Ok(pat);
            }
            // `_` is an identifier; it's a wildcard unless followed by `(` (a payload, i.e. a
            // variant literally named `_`).
            Token::Ident(name) if name == "_" && self.peek_at(1) != &Token::LParen => {
                self.advance();
                return Ok(Pattern::Wildcard);
            }
            Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Bytes(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Assert
            | Token::Test
            | Token::Spawn
            | Token::Parallel
            | Token::Wait
            | Token::Yield
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::QuestionDot
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => {}
        }
        // Span of the binding-name token (used only when this resolves to a bare-ident binding,
        // below) for the decl-site hover. A qualified/variant pattern ignores it.
        let name_span = self.cur_span();
        let none_kw = self.check(&Token::NoneKw);
        let name = self.expect_ident_or_none()?;
        self.reject_removed_name(&name, name_span)?;
        // `Enum.Variant` — a qualified variant pattern. The first ident is the enum qualifier; the
        // ident after `.` is the variant. A qualified pattern is always a variant (never a binding),
        // even in a sub-position. A THIRD ident after a second `.` (`module.Enum.Variant`) shifts the
        // first ident to a module binder, the second to the enum, the third to the variant —
        // symmetric with module-qualified construction (`geo.Color.Red`). This is unambiguous: a
        // 2-part variant pattern can't be followed by a `.` today, so a 3rd dot deterministically
        // means module-qualified. Four or more segments (`pkg.deep.Color.Red`) name the module by
        // its full dotted path (TICKET-175): the last two are the enum and the variant.
        let mut segments = vec![name];
        while self.eat(&Token::Dot) {
            segments.push(self.expect_ident()?);
        }
        let (name, enum_name, module_name) = match segments.len() {
            1 => (segments.pop().unwrap_or_default(), None, None),
            2 => {
                // `Enum.Variant`
                let variant = segments.pop().unwrap_or_default();
                (variant, segments.pop(), None)
            }
            _ => {
                // `module.Enum.Variant`, where `module` may be a dotted full path
                let variant = segments.pop().unwrap_or_default();
                let enum_name = segments.pop();
                (variant, enum_name, Some(segments.join(".")))
            }
        };
        if self.eat(&Token::LParen) {
            // `Name(p, …)` — a variant with (possibly nested) sub-patterns.
            let mut bindings = Vec::new();
            if !self.check(&Token::RParen) {
                loop {
                    bindings.push(self.parse_subpattern()?);
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Token::RParen)?;
            return Ok(Pattern::Variant {
                id: crate::ast::NodeId::fresh(),
                name,
                bindings,
                enum_name,
                module_name,
            });
        }
        // A bare identifier: a nullary variant at the top of an arm, or a binding in a sub-position.
        // A qualified `Enum.Variant` is unambiguously a nullary variant in either position.
        if top || none_kw || enum_name.is_some() {
            Ok(Pattern::Variant {
                id: crate::ast::NodeId::fresh(),
                name,
                bindings: Vec::new(),
                enum_name,
                module_name,
            })
        } else {
            Ok(Pattern::Ident(name, name_span, crate::ast::NodeId::fresh()))
        }
    }

    /// Parse `( p1, p2, … )`. A single parenthesised pattern `(p)` is grouping (returns `p`);
    /// two-or-more elements form a `Tuple` matching a tuple value of that arity.
    fn parse_tuple_pattern(&mut self) -> PResult<Pattern> {
        self.parse_paren_list(Self::parse_subpattern)
    }

    /// Parse `( elem (, elem)* )`, the one parenthesized pattern list (a match tuple pattern and a
    /// nested binding target share it). One element is grouping and returns it; two or more form a
    /// `Tuple`.
    fn parse_paren_list(&mut self, elem: fn(&mut Self) -> PResult<Pattern>) -> PResult<Pattern> {
        self.expect(&Token::LParen)?;
        let mut elems = vec![elem(self)?];
        while self.eat(&Token::Comma) {
            elems.push(elem(self)?);
        }
        self.expect(&Token::RParen)?;
        if elems.len() == 1 {
            Ok(elems.pop().expect("one element"))
        } else {
            Ok(Pattern::Tuple(elems))
        }
    }

    /// One element of a binding target: a name, `_` (skips a slot), or a parenthesized nested
    /// target. A keyword in a name slot gets DEC-237's one message from `expect_ident`.
    fn parse_bind_target(&mut self) -> PResult<Pattern> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("pattern nested too deeply".to_string()));
        }
        let target = if self.check(&Token::LParen) {
            // A one-name group `(x)` is that name, as in CPython.
            self.parse_paren_list(Self::parse_bind_target)?
        } else {
            let span = self.cur_span();
            let name = self.expect_ident()?;
            bind_leaf(name, span)
        };
        self.depth -= 1;
        Ok(target)
    }

    /// A comma list of binding-target elements (`a, (b, c)`), as the elements.
    fn parse_bind_target_list(&mut self) -> PResult<Vec<Pattern>> {
        let mut elems = vec![self.parse_bind_target()?];
        while self.eat_in_line(&Token::Comma) {
            elems.push(self.parse_bind_target()?);
        }
        Ok(elems)
    }

    /// `for` and comprehension targets: a bare name, `_`, or a comma list. A target that is one
    /// parenthesized tuple (`for (a, b) in xs`) is an error: an unpacking target takes no outer
    /// parentheses. One element is itself; two or more are a `Tuple`.
    fn parse_loop_target(&mut self) -> PResult<Pattern> {
        let span = self.cur_span();
        let parenthesized = self.check(&Token::LParen);
        let mut elems = self.parse_bind_target_list()?;
        if elems.len() == 1 {
            let only = elems.pop().expect("one element");
            if parenthesized && matches!(only, Pattern::Tuple(_)) {
                return Err(ParseError {
                    message: "an unpacking target takes no outer parentheses; write the names separated by commas".to_string(),
                    span,
                });
            }
            Ok(only)
        } else {
            Ok(Pattern::Tuple(elems))
        }
    }

    /// Speculatively read `<target> :=`. Commits (`Some`, the `:=` eaten) only for a name, `_` (each
    /// possibly grouped, `(x)`) or a top-level comma list; every other shape restores `pos`, `depth` and `fold_depth` and
    /// returns `None`, so the expression path reports the error (`bind_target_error`).
    fn try_parse_walrus_target(&mut self) -> PResult<Option<Pattern>> {
        if !matches!(self.peek(), Token::Ident(_) | Token::LParen) {
            return Ok(None);
        }
        let save = self.pos;
        let save_depth = self.depth;
        let save_fold = self.fold_depth;
        if let Ok(mut elems) = self.parse_bind_target_list()
            && self.check(&Token::Walrus)
            && (elems.len() > 1 || !matches!(elems[0], Pattern::Tuple(_)))
        {
            self.advance(); // ':='
            // A removed carrier name is no binder: the expression path rejected it as a primary.
            for (name, span) in elems.iter().flat_map(Pattern::binders) {
                self.reject_removed_name(name, span)?;
            }
            return Ok(Some(if elems.len() == 1 {
                elems.pop().expect("one element")
            } else {
                Pattern::Tuple(elems)
            }));
        }
        self.pos = save;
        self.depth = save_depth;
        self.fold_depth = save_fold;
        Ok(None)
    }

    /// The right side of `:=`, `=`, `return` and `yield`: one value, or a comma list that is a
    /// tuple written without parentheses. Only the first value is a value position (DEC-260).
    /// The bool says the right side was a list.
    fn parse_value_list(&mut self, site: ValueSite) -> PResult<(Expr, bool)> {
        let first = self.parse_value(site)?;
        if self.ended_at_block() || self.peek() != &Token::Comma {
            return Ok((first, false));
        }
        let span = first.span;
        let mut values = vec![first];
        while self.eat_in_line(&Token::Comma) {
            values.push(self.parse_expr()?);
        }
        Ok((
            Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Tuple(values),
                span,
            },
            true,
        ))
    }

    fn parse_return(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Return)?;
        // a bare `return` ends the line — no value
        if self.at_line_end() {
            Ok(StmtKind::Return(None))
        } else {
            let (value, _) = self.parse_value_list(ValueSite::Return)?;
            self.reject_else_guard()?;
            Ok(StmtKind::Return(Some(value)))
        }
    }

    /// `yield <expr>` — always carries a value (unlike `return`). Generator syntax (VM-only; shipped).
    fn parse_yield(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Yield)?;
        let (value, _) = self.parse_value_list(ValueSite::Yield)?;
        Ok(StmtKind::Yield(value))
    }

    /// `assert <cond>` or `assert <cond>, <msg>` — line-oriented (the caller requires a stmt end).
    fn parse_assert(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Assert)?;
        let cond = self.parse_expr()?;
        let msg = if self.eat_in_line(&Token::Comma) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(StmtKind::Assert { cond, msg })
    }

    /// `defer:` block (form 2) or `defer <call>` (form 1). Mirrors `parse_spawn`: form 1 is
    /// line-oriented (terminated here); form 2 is compound (its block ends at its own `Dedent`). The
    /// call-only restriction on form 1 is enforced by the checker (context-sensitive).
    fn parse_defer(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Defer)?;
        if self.check(&Token::Colon) {
            let body = self.parse_block()?;
            return Ok(StmtKind::Defer(DeferTarget::Block(body)));
        }
        let expr = self.parse_expr()?;
        self.expect_stmt_end()?;
        Ok(StmtKind::Defer(DeferTarget::Call(expr)))
    }

    /// `parallel:` — a nursery whose body is an indented (or inline) block. Compound: ends at its
    /// own `Dedent`, so the caller does not require a line terminator.
    fn parse_parallel(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Parallel)?;
        let body = self.parse_block()?;
        Ok(StmtKind::Parallel { body })
    }

    /// `spawn:` block. Compound: its block ends at its own `Dedent`.
    fn parse_spawn(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Spawn)?;
        let body = self.parse_block()?;
        Ok(StmtKind::Spawn(SpawnTarget::Block(body)))
    }

    /// `spawn <call>`. A simple statement, terminated here; a non-call is rejected with a clear
    /// message.
    fn parse_spawn_call(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Spawn)?;
        let expr = self.parse_expr()?;
        if !matches!(expr.kind, ExprKind::Call { .. }) {
            return Err(self.err(
                "spawn requires a function or method call (`spawn f(x)`) or a block (`spawn:`)"
                    .to_string(),
            ));
        }
        self.expect_stmt_end()?;
        Ok(StmtKind::Spawn(SpawnTarget::Call(expr)))
    }

    /// `wait:` — Chezzi's `select` (see [`StmtKind::Wait`]). Indented arms `<target> (:=|=)
    /// <chan>.recv(): <body>`, with an optional non-blocking `else:` that must be the last arm. The
    /// RHS must be a bare `.recv()` (no args); a non-`recv` RHS or `recv(args)` is a parse error.
    fn parse_wait(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Wait)?;
        self.open_block()?; // ':' Newline Indent
        let mut arms = Vec::new();
        let mut else_block = None;
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            if self.check(&Token::Else) {
                self.advance();
                else_block = Some(self.parse_block()?);
                self.skip_newlines();
                if !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
                    return Err(self.err("`else` must be the last arm of a `wait`".to_string()));
                }
                break;
            }
            let arm_span = self.cur_span();
            let mut assign_first: Option<Stmt> = None;
            // `a := ch.recv():`, `_ := …`, `a, (b, c) := …`: the one binding-target parse.
            let kind = if let Some(target) = self.try_parse_walrus_target()? {
                WaitArmKind::Recv {
                    target,
                    chan: self.parse_recv_chan()?,
                }
            } else {
                self.parse_wait_arm_head(arm_span, &mut assign_first)?
            };
            let mut body = self.parse_block()?;
            if let Some(stmt) = assign_first {
                body.insert(0, stmt);
            }
            arms.push(WaitArm {
                kind,
                body,
                span: arm_span,
            });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        if arms.is_empty() {
            return Err(self.err("`wait` needs at least one `recv` arm".to_string()));
        }
        Ok(StmtKind::Wait { arms, else_block })
    }

    /// A wait arm whose head is an expression: a `:=` with a target that is not a name list (the
    /// error path, `bind_target_error` reports it), a `=` recv arm (`y = ch.recv():`, `a, (b.f, c) =
    /// ch.recv():`), or a bare send arm (`ch.send(v):`).
    /// A `=` arm binds a fresh `$waitN` and sets `assign_first` to the assignment `lhs = $waitN`
    /// for the arm body's first statement (TICKET-227), so the assigned value has a node (it can
    /// wrap into a carrier target like any assignment). The parser stays lenient on a send arm: the
    /// CHECKER validates it is exactly `chan.send(value)`.
    fn parse_wait_arm_head(
        &mut self,
        arm_span: Span,
        assign_first: &mut Option<Stmt>,
    ) -> PResult<WaitArmKind> {
        let mut lhs = self.parse_expr()?;
        let mut listed = false;
        if self.peek() == &Token::Comma {
            let list_span = lhs.span;
            let mut places = vec![lhs];
            while self.eat_in_line(&Token::Comma) {
                places.push(self.parse_expr()?);
            }
            lhs = Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Tuple(places),
                span: list_span,
            };
            listed = true;
        }
        let lhs_span = lhs.span;
        if self.eat_in_line(&Token::Walrus) {
            // `try_parse_walrus_target` took every valid `:=` target, so this branch only reports.
            return Err(
                self.bind_target_error(lhs, "left side of ':=' in a wait arm must be a name")
            );
        }
        if !self.eat_in_line(&Token::Assign) {
            // Bare expr, no `:=`/`=` — a send arm.
            return Ok(WaitArmKind::Send { call: lhs });
        }
        let target = if matches!(lhs.kind, ExprKind::Ident(ref n) if n == "_") {
            Pattern::Wildcard
        } else if is_assign_target(&lhs, listed) {
            let id = crate::ast::NodeId::fresh();
            let tmp = format!("$wait{}", id.0);
            let value = Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Ident(tmp.clone()),
                span: lhs_span,
            };
            *assign_first = Some(Stmt {
                kind: StmtKind::Assign {
                    target: lhs,
                    op: AssignOp::Eq,
                    value,
                },
                span: arm_span,
            });
            Pattern::Ident(tmp, lhs_span, crate::ast::NodeId::fresh())
        } else {
            return Err(ParseError {
                message: "invalid assignment target".to_string(),
                span: lhs_span,
            });
        };
        Ok(WaitArmKind::Recv {
            target,
            chan: self.parse_recv_chan()?,
        })
    }

    /// The RHS of a recv `wait:` arm: a bare `<chan>.recv()` (no args). Returns the channel expr.
    fn parse_recv_chan(&mut self) -> PResult<Expr> {
        let rhs = self.parse_expr()?;
        let rhs_span = rhs.span;
        match rhs.kind {
            ExprKind::Call {
                callee,
                args,
                named,
                ..
            } if matches!(&callee.kind, ExprKind::Field { name, .. } if name == "recv") => {
                if !args.is_empty() || !named.is_empty() {
                    return Err(ParseError {
                        message: "`recv` in a wait arm takes no arguments".to_string(),
                        span: rhs_span,
                    });
                }
                match callee.kind {
                    ExprKind::Field { obj, .. } => Ok(*obj),
                    ExprKind::Int(_)
                    | ExprKind::Float(_)
                    | ExprKind::Str(_)
                    | ExprKind::Interp(_)
                    | ExprKind::Bytes(_)
                    | ExprKind::RawStr(_)
                    | ExprKind::Bool(_)
                    | ExprKind::NoneLit
                    | ExprKind::Pass
                    | ExprKind::Ident(_)
                    | ExprKind::List(..)
                    | ExprKind::Tuple(_)
                    | ExprKind::Map(_)
                    | ExprKind::Set(_)
                    | ExprKind::Comprehension { .. }
                    | ExprKind::Unary { .. }
                    | ExprKind::Binary { .. }
                    | ExprKind::Compare { .. }
                    | ExprKind::Range { .. }
                    | ExprKind::Call { .. }
                    | ExprKind::Index { .. }
                    | ExprKind::Try(_)
                    | ExprKind::OptChain { .. }
                    | ExprKind::NullCoalesce { .. }
                    | ExprKind::Closure { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::IfElse { .. }
                    | ExprKind::Recover(..)
                    | ExprKind::ElseGuard { .. } => unreachable!("guarded by the matches! above"),
                }
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::Ident(_)
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => Err(ParseError {
                message: "a wait arm must `recv` from a channel (`v := ch.recv():`)".to_string(),
                span: rhs_span,
            }),
        }
    }

    fn parse_import(&mut self) -> PResult<Import> {
        self.expect(&Token::Import)?;
        if self.rest_of_line_has(&Token::From) {
            // `import n1[, n2 as a], … from a.b.c`
            let mut names = Vec::new();
            // BOUND-NAME spans, parallel to `names`: the `as` alias token when present, else the
            // member token. Diagnostic-only (decl-site hover); equality-neutral on `Import`.
            let mut name_spans = Vec::new();
            loop {
                let member_span = self.cur_span();
                let name = if self.native_enums.is_empty() {
                    self.expect_ident()?
                } else {
                    self.expect_ident_or_none()?
                };
                self.reject_removed_name(&name, member_span)?;
                let (alias, bound_span) = if self.eat(&Token::As) {
                    let alias_span = self.cur_span();
                    (Some(self.expect_ident()?), alias_span)
                } else {
                    (None, member_span)
                };
                names.push((name, alias));
                name_spans.push(bound_span);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::From)?;
            let from_span = self.cur_span();
            let path = self.parse_dotted_path()?;
            if let Some(first) = path.first() {
                self.reject_removed_name(first, from_span)?;
            }
            Ok(Import::From {
                path,
                names,
                name_spans,
            })
        } else {
            // `import a.b.c [as alias]`
            let (path, last_seg_span) = self.parse_dotted_path_spanned()?;
            let (alias, name_span) = if self.eat(&Token::As) {
                let alias_span = self.cur_span();
                (Some(self.expect_ident()?), alias_span)
            } else {
                (None, last_seg_span)
            };
            Ok(Import::Module {
                path,
                alias,
                name_span,
            })
        }
    }

    fn parse_dotted_path(&mut self) -> PResult<Vec<String>> {
        let mut path = vec![self.expect_path_segment()?];
        while self.eat(&Token::Dot) {
            path.push(self.expect_path_segment()?);
        }
        Ok(path)
    }

    /// Like [`parse_dotted_path`] but also returns the source span of the LAST segment token — the
    /// bound name of a whole-module import when there is no `as` alias (`math` in `import std.math`).
    /// Diagnostic-only (the LSP records a decl-site hover there); the span is equality-neutral.
    fn parse_dotted_path_spanned(&mut self) -> PResult<(Vec<String>, Span)> {
        let mut last_span = self.cur_span();
        let mut path = vec![self.expect_path_segment()?];
        while self.eat(&Token::Dot) {
            last_span = self.cur_span();
            path.push(self.expect_path_segment()?);
        }
        Ok((path, last_span))
    }

    /// A module-path segment: an identifier.
    fn expect_path_segment(&mut self) -> PResult<String> {
        self.expect_ident()
    }

    // ----- blocks -----

    /// Open + parse a block after a `:`. Either an indented `Newline Indent … Dedent` group, or a
    /// single inline statement on the same line.
    fn parse_block(&mut self) -> PResult<Block> {
        self.expect(&Token::Colon)?;
        if self.check(&Token::Newline) {
            self.advance(); // the Newline
            self.expect(&Token::Indent)?;
            let mut stmts = Vec::new();
            self.skip_newlines();
            while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
                stmts.push(self.parse_stmt()?);
                self.skip_newlines();
            }
            self.expect(&Token::Dedent)?;
            Ok(stmts)
        } else {
            // inline single statement (e.g. a one-line `match` arm): one `<simpleStmt>`.
            Ok(vec![self.parse_stmt_in(true)?])
        }
    }

    /// The `:` at token `colon` ends a block expression's head. Inside brackets the lexer emitted no
    /// layout, so ask it to open the block now ([`crate::lexer::open_layout`]); at bracket depth 0 the
    /// next token is already `Newline`, and a same-line body is left for the caller to reject.
    fn request_layout(&mut self, colon: usize) -> PResult<()> {
        if self.toks[colon + 1].kind == Token::Newline {
            return Ok(());
        }
        crate::lexer::open_layout(&mut self.toks, colon).map_err(|e| ParseError {
            message: e.message,
            span: Span {
                line: e.line as u32,
                col: e.col as u32,
                file: self.toks[self.pos].span.file,
            },
        })?;
        Ok(())
    }

    /// Open an indented block for declarations whose bodies are not statements (`struct`, `enum`):
    /// consumes `: Newline Indent`. The caller parses members until `Dedent`.
    fn open_block(&mut self) -> PResult<()> {
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        Ok(())
    }

    // ----- types -----

    /// One parameter slot of a `fn(...)` TYPE, with an OPTIONAL Swift-style label: `name: T` yields
    /// `(Some("name"), T)`; a bare `T` yields `(None, T)`. A label is an `IDENT` IMMEDIATELY followed
    /// by `:` — disambiguated by lookahead so a bare qualified/generic type head (`m.Type` = `IDENT
    /// Dot`, `List[int]` = `IDENT LBracket`) is never mistaken for a label.
    fn parse_fn_type_param(&mut self) -> PResult<(Option<String>, Type)> {
        if matches!(self.peek(), Token::Ident(_)) && *self.peek_at(1) == Token::Colon {
            let label = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            let ty = self.parse_type()?;
            Ok((Some(label), ty))
        } else {
            Ok((None, self.parse_type()?))
        }
    }

    /// One type, with its own **fold scope**: `parse_type_postfix` is the parser's THIRD iterative
    /// fold loop (`T?!?…` wraps a `Type::Generic` around the tree it already built), and like the two
    /// expression loops it must be charged against [`MAX_AST_DEPTH`] — a recursion counter never sees
    /// it. Reset before the children so sibling type arguments cost the depth of the DEEPEST one
    /// rather than their sum, `max`-combined into the caller on the way out, so a postfix chain
    /// stacked on an already-deep type pays for both. Measured before this: `fn f(x: int` + `?!` ×
    /// 350 000 + `)` reached `chezzi check` and SIGABRTed with `fatal runtime error: stack overflow`
    /// (both `b1307258` and `dfdc7a1b`); it is now a clean `type nested too deeply`.
    fn parse_type(&mut self) -> PResult<Type> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("type nested too deeply".to_string()));
        }
        let outer_fold = self.fold_depth;
        self.fold_depth = 0;
        let ty = self.parse_type_body()?;
        self.fold_depth = outer_fold.max(self.fold_depth);
        self.depth -= 1;
        Ok(ty)
    }

    /// [`Self::parse_type`]'s body — everything but the depth/fold bookkeeping.
    fn parse_type_body(&mut self) -> PResult<Type> {
        // `fn(T1, …) -> R` — a function type in type position.
        if self.eat(&Token::Fn) {
            self.expect(&Token::LParen)?;
            let mut params = Vec::new();
            let mut labels: Vec<Option<String>> = Vec::new();
            if !self.check(&Token::RParen) {
                let (l, t) = self.parse_fn_type_param()?;
                labels.push(l);
                params.push(t);
                while self.eat(&Token::Comma) {
                    let (l, t) = self.parse_fn_type_param()?;
                    labels.push(l);
                    params.push(t);
                }
            }
            self.expect(&Token::RParen)?;
            self.expect(&Token::Arrow)?;
            let ret = self.parse_type()?;
            let mut ty = Type::Func {
                params,
                ret: Box::new(ret),
                labels,
            };
            ty = self.parse_type_postfix(ty)?;
            return Ok(ty);
        }
        // `(T)` unwraps to `T`; `(T1, T2, …)` is a tuple type. The `?`/`!` postfix still applies.
        if self.eat(&Token::LParen) {
            let mut types = vec![self.parse_type()?];
            while self.eat(&Token::Comma) {
                types.push(self.parse_type()?);
            }
            self.expect(&Token::RParen)?;
            let mut ty = if types.len() == 1 {
                types.pop().unwrap()
            } else {
                Type::Tuple(types)
            };
            ty = self.parse_type_postfix(ty)?;
            return Ok(ty);
        }
        // Prefix `!E` (and a bare `!`): `None!E`, the type of a fallible call with no value. The
        // `!` is left for `parse_type_postfix`, so both spellings are one production.
        if self.check(&Token::Bang) {
            let ty = Type::Nil(self.cur_span());
            return self.parse_type_postfix(ty);
        }
        if self.check(&Token::NoneKw) {
            let ty = Type::Nil(self.cur_span());
            self.advance();
            return self.parse_type_postfix(ty);
        }
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        self.reject_removed_name(&name, name_span)?;
        // A module-qualified type `module.Type` (mirrors how `module.func()` is reached): after the
        // first ident, a `.` introduces the type's name in the bound module. A longer path
        // (`pkg.deep.Point`) is the full path of an imported module (TICKET-175): the LAST segment is
        // the type and the rest, dot-joined, is the module. Any trailing `[args]` then belongs to the
        // qualified type (`geo.Box[int]`).
        if self.eat(&Token::Dot) {
            let mut segments = vec![name, self.expect_ident()?];
            while self.eat(&Token::Dot) {
                segments.push(self.expect_ident()?);
            }
            let member = segments.pop().unwrap_or_default();
            let name = segments.join(".");
            let args = if self.eat(&Token::LBracket) {
                let mut args = vec![self.parse_type()?];
                while self.eat(&Token::Comma) {
                    args.push(self.parse_type()?);
                }
                self.expect(&Token::RBracket)?;
                args
            } else {
                Vec::new()
            };
            let ty = Type::Qualified {
                module: name,
                name: member,
                args,
            };
            let ty = self.parse_type_postfix(ty)?;
            return Ok(ty);
        }
        let ty = if self.eat(&Token::LBracket) {
            let mut args = vec![self.parse_type()?];
            while self.eat(&Token::Comma) {
                args.push(self.parse_type()?);
            }
            self.expect(&Token::RBracket)?;
            // The head-name token's OWN span (captured before `expect_ident`) rides along for the
            // editor hover overlay; equality-neutral, never read by the engines.
            Type::Generic(name, args, name_span)
        } else {
            Type::Named {
                name,
                span: name_span,
            }
        };
        self.parse_type_postfix(ty)
    }

    /// Postfix shorthand on a fully-parsed base type: `T?` = Option[T], `T!` = Result[T, Error],
    /// `T!E` = Result[T, E]. Stacks left-to-right (`T?!` = Result[Option[T], Error]).
    ///
    /// Same accounting as the two expression fold loops: each iteration adds one level to a type
    /// tree the front-end walkers descend recursively, and this loop bumps neither `self.depth` nor
    /// anything else. The base type's own fold-depth is already in `self.fold_depth` (the caller
    /// [`Self::parse_type`] opened the scope), so the chain SUMS on top of it.
    fn parse_type_postfix(&mut self, mut ty: Type) -> PResult<Type> {
        let mut chain = 0usize;
        loop {
            if self.depth + self.fold_depth + chain > MAX_AST_DEPTH {
                return Err(self.err("type nested too deeply".to_string()));
            }
            if self.eat(&Token::Question) {
                chain += 1;
                ty = Type::Generic("Option".to_string(), vec![ty], Span::default());
            } else if self.eat(&Token::Bang) {
                chain += 1;
                // An explicit error type follows only if the next token can start one; otherwise
                // `T!` defaults the error type to `Error` (resolved later by the checker).
                if matches!(
                    self.peek(),
                    Token::Ident(_) | Token::LParen | Token::Fn | Token::NoneKw
                ) {
                    let err = self.parse_type()?;
                    ty = Type::Generic("Result".to_string(), vec![ty, err], Span::default());
                } else {
                    ty = Type::Generic("Result".to_string(), vec![ty], Span::default());
                }
            } else if self.check(&Token::NotEq) && self.bang_eq != Some(self.pos) {
                // Adjacent `!=` lexes as ONE token: `int!= 6` is `int! = 6`. The token stays; the
                // position test stops an outer postfix loop wrapping a second time.
                chain += 1;
                self.bang_eq = Some(self.pos);
                ty = Type::Generic("Result".to_string(), vec![ty], Span::default());
                break;
            } else if self.eat(&Token::QuestionQuestion) {
                // Adjacent `??` lexes as ONE token: `int??` is `Option[Option[int]]`.
                chain += 2;
                ty = Type::Generic("Option".to_string(), vec![ty], Span::default());
                ty = Type::Generic("Option".to_string(), vec![ty], Span::default());
            } else {
                break;
            }
        }
        self.fold_depth += chain;
        Ok(ty)
    }

    // ----- expressions (Pratt) -----

    /// Parse an expression that starts a whole value at `_site`: the one place a block expression may
    /// start ([`Parser::block_ok`]).
    fn parse_value(&mut self, site: ValueSite) -> PResult<Expr> {
        self.open_value(site, self.pos);
        self.parse_expr()
    }

    /// Mark the token at `at` as the start of a whole value at `site`: the one writer of
    /// `value_start`. `parse_fn` calls it for an inline body, whose first token is not `self.pos`.
    fn open_value(&mut self, _site: ValueSite, at: usize) {
        self.value_start = Some(at);
    }

    /// May the block expression whose keyword is the token at `kw` start here? Only the token where a
    /// whole value starts qualifies, which excludes every operand, prefix, argument and element.
    fn block_ok(&self, kw: usize) -> bool {
        self.value_start == Some(kw)
    }

    fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_bp(0)
    }

    /// Precedence-climbing core. `min_bp` is the minimum left binding power that may bind here.
    fn parse_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.too_deep());
        }
        // Each fold below adds one level to a left-leaning AST that the recursive front-end walkers
        // descend. Iterative, so the `self.depth` guard above never sees it — the bound is
        // `MAX_AST_DEPTH` over the synthesized `fold_depth` (see that constant). Reset before the
        // children so their fold-depths accumulate here, restore `max`-combined on the way out.
        let outer_fold = self.fold_depth;
        self.fold_depth = 0;
        // `not` sits between `and` and the comparisons (Python's grammar), not in the unary tier,
        // so `not x in xs` is `not (x in xs)` and `1 + not y` is a parse error. Gated on `min_bp`
        // rather than a token check alone, so `not` still parses in every loose position (an `if`
        // condition, an `and`/`or` operand, a call argument, the operand of another `not`).
        let mut lhs = if matches!(self.peek(), Token::Not) && min_bp <= NOT_BP {
            let not_span = self.cur_span();
            self.advance();
            let expr = self.parse_bp(NOT_BP)?;
            Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(expr),
                },
                span: not_span,
            }
        } else if matches!(self.peek(), Token::DotDot | Token::DotDotEq) && min_bp <= RANGE_BP {
            // `..b`, `..=b` and `..`: a range with no start.
            let span = self.cur_span();
            let inclusive = self.check(&Token::DotDotEq);
            self.advance();
            let end = self.parse_range_end(inclusive, span)?;
            Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Range {
                    start: None,
                    end,
                    inclusive,
                },
                span,
            }
        } else {
            self.parse_unary()?
        };
        let mut chain = 0usize;
        while let Some((op, l_bp, r_bp)) = infix_op(self.peek()) {
            if l_bp < min_bp || self.ended_at_block() {
                break;
            }
            chain += 1;
            if self.depth + self.fold_depth + chain > MAX_AST_DEPTH {
                return Err(self.too_deep());
            }
            let op_span = self.cur_span();
            let inclusive = self.check(&Token::DotDotEq);
            self.advance(); // the operator
            if let InfixOp::Range = op {
                if matches!(lhs.kind, ExprKind::Range { .. }) {
                    return Err(ParseError {
                        message: "a range has no third bound; write `Range(a, b, step)` for a step"
                            .to_string(),
                        span: op_span,
                    });
                }
                let end = self.parse_range_end(inclusive, op_span)?;
                let span = lhs.span;
                lhs = Expr {
                    id: crate::ast::NodeId::fresh(),
                    kind: ExprKind::Range {
                        start: Some(Box::new(lhs)),
                        end,
                        inclusive,
                    },
                    span,
                };
                continue;
            }
            let rhs = self.parse_bp(r_bp)?;
            let span = lhs.span;
            lhs = match op {
                // TICKET-077: a comparison operator CHAINS with any further comparison at the same
                // level, Python-style — consume every adjacent link here rather than letting the
                // outer `while` fold them pairwise (which would nest as `(a < b) == c`).
                InfixOp::Bin(op) if is_comparison(op) => {
                    let mut operands = vec![lhs, rhs];
                    let mut ops = vec![op];
                    while let Some((InfixOp::Bin(next_op), next_l, next_r)) = infix_op(self.peek())
                        .filter(|(o, ..)| matches!(o, InfixOp::Bin(nop) if is_comparison(*nop)))
                    {
                        if next_l < min_bp || self.ended_at_block() {
                            break;
                        }
                        chain += 1;
                        if self.depth + self.fold_depth + chain > MAX_AST_DEPTH {
                            return Err(self.too_deep());
                        }
                        self.advance();
                        let next_rhs = self.parse_bp(next_r)?;
                        operands.push(next_rhs);
                        ops.push(next_op);
                    }
                    if ops.len() == 1 {
                        Expr {
                            id: crate::ast::NodeId::fresh(),
                            kind: ExprKind::Binary {
                                op: ops[0],
                                lhs: Box::new(operands.remove(0)),
                                rhs: Box::new(operands.remove(0)),
                            },
                            span,
                        }
                    } else {
                        Expr {
                            id: crate::ast::NodeId::fresh(),
                            kind: ExprKind::Compare { operands, ops },
                            span,
                        }
                    }
                }
                InfixOp::Bin(op) => Expr {
                    id: crate::ast::NodeId::fresh(),
                    kind: ExprKind::Binary {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    span,
                },
                InfixOp::Range => unreachable!("a range is built before the generic operand parse"),
                InfixOp::Coalesce => Expr {
                    id: crate::ast::NodeId::fresh_block(crate::ast::CARRIER_IDS),
                    kind: ExprKind::NullCoalesce {
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                        op_span,
                    },
                    span,
                },
                // `lhs |> f(args)` desugars at parse time to `f(lhs, args)` — threading `lhs` as the
                // first argument. The RHS must be a call, so checker/interp/VM see a plain call and
                // need no pipe-specific code.
                InfixOp::Pipe => match rhs.kind {
                    ExprKind::Call {
                        callee,
                        args,
                        named,
                        type_args,
                        bracket,
                    } => {
                        if !named.is_empty() {
                            return Err(self.err(
                                "named arguments are not supported on the right side of '|>'"
                                    .to_string(),
                            ));
                        }
                        let mut new_args = Vec::with_capacity(args.len() + 1);
                        new_args.push(lhs);
                        new_args.extend(args);
                        Expr {
                            id: crate::ast::NodeId::fresh(),
                            kind: ExprKind::Call {
                                callee,
                                args: new_args,
                                named,
                                type_args,
                                bracket,
                            },
                            span,
                        }
                    }
                    ExprKind::Int(_)
                    | ExprKind::Float(_)
                    | ExprKind::Str(_)
                    | ExprKind::Interp(_)
                    | ExprKind::Bytes(_)
                    | ExprKind::RawStr(_)
                    | ExprKind::Bool(_)
                    | ExprKind::NoneLit
                    | ExprKind::Pass
                    | ExprKind::Ident(_)
                    | ExprKind::List(..)
                    | ExprKind::Tuple(_)
                    | ExprKind::Map(_)
                    | ExprKind::Set(_)
                    | ExprKind::Comprehension { .. }
                    | ExprKind::Unary { .. }
                    | ExprKind::Binary { .. }
                    | ExprKind::Compare { .. }
                    | ExprKind::Range { .. }
                    | ExprKind::Field { .. }
                    | ExprKind::Index { .. }
                    | ExprKind::Try(_)
                    | ExprKind::OptChain { .. }
                    | ExprKind::NullCoalesce { .. }
                    | ExprKind::Closure { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::IfElse { .. }
                    | ExprKind::Recover(..)
                    | ExprKind::ElseGuard { .. } => {
                        return Err(
                            self.err("right side of '|>' must be a function call".to_string())
                        );
                    }
                },
            };
        }
        self.fold_depth = outer_fold.max(self.fold_depth + chain);
        self.depth -= 1;
        Ok(lhs)
    }

    /// Prefix unary operators (`-`, `!` error value, `?` present/success value; `??` is two
    /// nested `?`), then a postfix chain. `not` is parsed one level up.
    fn parse_unary(&mut self) -> PResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.too_deep());
        }
        let span = self.cur_span();
        if self.check(&Token::QuestionQuestion) {
            // Adjacent `??` lexes as ONE token: prefix `??x` is `?(?x)`, two nodes.
            self.advance();
            let inner = self.parse_unary();
            self.depth -= 1;
            let wrap = |e: Expr| Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Unary {
                    op: UnaryOp::Wrap,
                    expr: Box::new(e),
                },
                span,
            };
            return inner.map(|e| wrap(wrap(e)));
        }
        let op = match self.peek() {
            Token::Minus => Some(UnaryOp::Neg),
            Token::Bang => Some(UnaryOp::ErrVal),
            Token::Question => Some(UnaryOp::Wrap),
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Assert
            | Token::Test
            | Token::Spawn
            | Token::Parallel
            | Token::Wait
            | Token::Yield
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => None,
        };
        let result = if let Some(op) = op {
            self.advance();
            // i64::MIN carve-out: `-9223372036854775808` folds straight to Int(i64::MIN). The lexer
            // emits the 2^63 magnitude as IntMinMagnitude (it does not fit i64 as a positive value);
            // runtime-negating it would overflow, so emit the literal directly here.
            if matches!(op, UnaryOp::Neg) && matches!(self.peek(), Token::IntMinMagnitude) {
                self.advance();
                Ok(Expr {
                    id: crate::ast::NodeId::fresh(),
                    kind: ExprKind::Int(i64::MIN),
                    span,
                })
            } else {
                let expr = self.parse_unary()?;
                Ok(Expr {
                    id: crate::ast::NodeId::fresh(),
                    kind: ExprKind::Unary {
                        op,
                        expr: Box::new(expr),
                    },
                    span,
                })
            }
        } else {
            self.parse_postfix()
        };
        self.depth -= 1;
        result
    }

    /// A primary expression followed by any chain of `(call)`, `.field`, `[index]`, `?`.
    fn parse_postfix(&mut self) -> PResult<Expr> {
        // Same accounting as the infix loop in `parse_bp`: `x.f.f…` / `a[0][0]…` / `f().g()…` build a
        // left-leaning AST the walkers descend, and this loop never bumps `self.depth`. Count every
        // iteration (incl. the turbofish `continue` and the final non-folding one — an overcount of
        // at most one per frame, itself bounded by `MAX_DEPTH`).
        let outer_fold = self.fold_depth;
        self.fold_depth = 0;
        let mut e = self.parse_primary()?;
        let mut chain = 0usize;
        loop {
            chain += 1;
            if self.depth + self.fold_depth + chain > MAX_AST_DEPTH {
                return Err(self.too_deep());
            }
            let span = e.span;
            if self.ended_at_block() {
                break;
            }
            e = match self.peek() {
                Token::LParen => {
                    self.advance();
                    let (args, named) = self.parse_call_args()?;
                    Expr {
                        id: crate::ast::NodeId::fresh(),
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                            named,
                            type_args: Vec::new(),
                            bracket: None,
                        },
                        span,
                    }
                }
                Token::Dot => {
                    self.advance();
                    // `obj.field` (struct/module) or `tuple.0` (element access). A numeric field name
                    // is the tuple-element index, stored as its decimal string (reusing `Field`).
                    // Capture the field-name token's TRUE span before consuming it (editor hover keys on it).
                    let name_span = self.cur_span();
                    let name = if let Token::Int(n) = self.peek() {
                        let n = *n;
                        self.advance();
                        n.to_string()
                    } else {
                        self.expect_ident()?
                    };
                    // TICKET-187: `.decode[T](s)` is no longer special here. It parses as an
                    // ordinary member call with a type argument (`try_parse_type_arg_call`), and
                    // the checker decides that only `decode` on the `std.json` module is JSON.
                    Expr {
                        id: crate::ast::NodeId::fresh(),
                        kind: ExprKind::Field {
                            obj: Box::new(e),
                            name,
                            name_span,
                        },
                        span,
                    }
                }
                Token::LBracket => {
                    let headed = matches!(e.kind, ExprKind::Ident(_) | ExprKind::Field { .. });
                    // `name[Type, …](args)` — explicit call-site type arguments. A bare name can be a
                    // generic fn / struct / variant; ANY member access (`recv.name`, a `Field` over an
                    // arbitrary receiver) can be a method turbofish (`recv.name[U](args)`) — the receiver
                    // need NOT be a bare ident (`W(1).cast[str]("a")`, `xs[0].cast[U](x)`, `h.w.cast[U](x)`
                    // all qualify). SPECULATIVE: `try_parse_type_arg_call` only commits when it sees the
                    // exact `[types](` shape, restoring pos+depth otherwise, so a genuine subscript that
                    // is NOT a type-then-call (`obj.items[0]`, `m.data[k]`, the numeric `arr[0].h[0](x)`)
                    // backtracks and keeps its plain index (then-call) meaning. A one-arg bracket keeps
                    // its expression reading too (`bracket`), and the checker chooses (TICKET-210): a
                    // head that denotes data (`fs[k](10)`, `h.fs[k](10)`) indexes then calls, with no
                    // parens. The parser reads no scope (DEC-204).
                    let type_call = if headed {
                        self.try_parse_type_arg_call(&e, span)?
                    } else {
                        None
                    };
                    match type_call {
                        Some(call) => call,
                        None => self.parse_bracket(e, span)?,
                    }
                }
                Token::Question => {
                    self.advance();
                    Expr {
                        id: crate::ast::NodeId::fresh(),
                        kind: ExprKind::Try(Box::new(e)),
                        span,
                    }
                }
                Token::QuestionDot => {
                    self.advance();
                    // `obj?.field` or `obj?.method(args)`. Field name mirrors `.` (ident or tuple
                    // index). A following `(` makes it an optional-chained method CALL — but only on
                    // a named method, never a tuple index (`t?.0()` is meaningless; the `(` there is
                    // left to a following postfix iteration, keeping the grammar and parser aligned).
                    // Capture the name token's TRUE span before consuming it — `span` above is the
                    // PRIMARY's span, shared by every link of `a?.b?.c`, so `name_span` is the only
                    // per-link key this node has (mirrors the `.` arm).
                    let name_span = self.cur_span();
                    let (name, is_ident) = if let Token::Int(n) = self.peek() {
                        let n = *n;
                        self.advance();
                        (n.to_string(), false)
                    } else {
                        (self.expect_ident()?, true)
                    };
                    let call = if is_ident && self.check(&Token::LParen) {
                        self.advance();
                        let (args, named) = self.parse_call_args()?;
                        Some(OptCall {
                            args,
                            named,
                            type_args: Vec::new(),
                        })
                    } else {
                        None
                    };
                    Expr {
                        id: crate::ast::NodeId::fresh_block(crate::ast::CARRIER_IDS),
                        kind: ExprKind::OptChain {
                            obj: Box::new(e),
                            name,
                            name_span,
                            call,
                        },
                        span,
                    }
                }
                Token::Int(_)
                | Token::IntMinMagnitude
                | Token::Float(_)
                | Token::Str(_)
                | Token::Bytes(_)
                | Token::RawStr(_)
                | Token::Ident(_)
                | Token::Fn
                | Token::Return
                | Token::If
                | Token::Else
                | Token::Elif
                | Token::For
                | Token::While
                | Token::In
                | Token::Break
                | Token::Continue
                | Token::Pass
                | Token::Struct
                | Token::Enum
                | Token::Protocol
                | Token::Type
                | Token::Match
                | Token::Recover
                | Token::Defer
                | Token::Assert
                | Token::Test
                | Token::Spawn
                | Token::Parallel
                | Token::Wait
                | Token::Yield
                | Token::Import
                | Token::Extern
                | Token::Native
                | Token::From
                | Token::As
                | Token::Const
                | Token::And
                | Token::Or
                | Token::Not
                | Token::True
                | Token::False
                | Token::NoneKw
                | Token::Where
                | Token::Plus
                | Token::Minus
                | Token::Star
                | Token::Slash
                | Token::Percent
                | Token::Assign
                | Token::Walrus
                | Token::EqEq
                | Token::NotEq
                | Token::Lt
                | Token::LtEq
                | Token::Gt
                | Token::GtEq
                | Token::PlusEq
                | Token::MinusEq
                | Token::StarEq
                | Token::SlashEq
                | Token::PercentEq
                | Token::AmpEq
                | Token::PipeEq
                | Token::CaretEq
                | Token::ShlEq
                | Token::ShrEq
                | Token::Arrow
                | Token::Pipe
                | Token::QuestionQuestion
                | Token::Bang
                | Token::Amp
                | Token::Caret
                | Token::BitOr
                | Token::Shl
                | Token::Shr
                | Token::RParen
                | Token::RBracket
                | Token::LBrace
                | Token::RBrace
                | Token::Comma
                | Token::Colon
                | Token::DotDot
                | Token::DotDotEq
                | Token::DotDotDot
                | Token::Newline
                | Token::Indent
                | Token::Dedent
                | Token::Eof => break,
            };
        }
        self.fold_depth = outer_fold.max(self.fold_depth + chain);
        Ok(e)
    }

    /// The end bound of a range whose operator was just consumed: an expression above `..`'s own
    /// level, or none when the next token cannot start one (`5..`, `..`). `a..=` needs an end.
    fn parse_range_end(&mut self, inclusive: bool, span: Span) -> PResult<Option<Box<Expr>>> {
        if token_ends_range(self.peek()) || self.ended_at_block() {
            if inclusive {
                return Err(ParseError {
                    message: "an inclusive range needs an end".to_string(),
                    span,
                });
            }
            return Ok(None);
        }
        Ok(Some(Box::new(self.parse_bp(RANGE_BP + 1)?)))
    }

    /// Parse the inside of a subscript `[ ... ]` (the `[` already consumed) and the closing `]`,
    /// producing an `Index` whose index is one expression: an int, a key, or a `Range` (`xs[a..b]`
    /// is a subscript by a range value; the checker decides whether it slices). A `:` inside the
    /// brackets is the removed colon slice and names its replacement (TICKET-253).
    fn parse_subscript(&mut self, obj: Expr, span: Span) -> PResult<Expr> {
        // A component is present iff the next token is not `:` or `]`.
        let component = |p: &mut Self| -> PResult<Option<Box<Expr>>> {
            if p.check(&Token::Colon) || p.check(&Token::RBracket) {
                Ok(None)
            } else {
                Ok(Some(Box::new(p.parse_expr()?)))
            }
        };
        let start = component(self)?;
        // No colon → plain index. `xs[]` (empty) is rejected as a missing index.
        if !self.check(&Token::Colon) {
            self.expect(&Token::RBracket)?;
            let index = start.ok_or_else(|| ParseError {
                message: "expected an index expression".to_string(),
                span,
            })?;
            return Ok(Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Index {
                    obj: Box::new(obj),
                    index: Some(index),
                    types: Vec::new(),
                },
                span,
            });
        }
        // TICKET-253: the colon slice is removed. Read the rest of the old form only to name its
        // replacement: `xs[a:b]` is `xs[a..b]`, `xs[a:b:c]` is `xs[Range(a, b, c)]` and `xs[::-1]` is
        // `xs.reversed()`.
        self.advance(); // first ':'
        let end = component(self)?;
        let step = if self.check(&Token::Colon) {
            self.advance(); // second ':'
            component(self)?
        } else {
            None
        };
        let is_minus_one = |st: &Expr| {
            if let ExprKind::Unary {
                op: UnaryOp::Neg,
                expr,
            } = &st.kind
            {
                return matches!(expr.kind, ExprKind::Int(1));
            }
            matches!(st.kind, ExprKind::Int(-1))
        };
        let is_reverse =
            start.is_none() && end.is_none() && step.as_deref().is_some_and(is_minus_one);
        let message = if is_reverse {
            "the slice `xs[::-1]` is removed; write `xs.reversed()`"
        } else if step.is_some() {
            "the slice `xs[a:b:c]` is removed; write `xs[Range(a, b, c)]` (use `None` for an omitted bound)"
        } else {
            "the slice `xs[a:b]` is removed; write `xs[a..b]` (also `xs[a..]`, `xs[..b]`, `xs[..]`)"
        };
        Err(ParseError {
            message: message.to_string(),
            span,
        })
    }

    /// Parse a comma-separated argument list and the closing `)`, assuming the opening `(` has just
    /// been consumed. A trailing comma is not allowed (the loop breaks on a non-comma). Returns the
    /// positional args and the named args (`name = expr`) separately. A named argument is recognised
    /// by a bare `IDENT` immediately followed by a single `=` (`Token::Assign`, distinct from `==`).
    /// Once a named argument appears, every later argument must also be named.
    fn parse_call_args(&mut self) -> PResult<CallArgs> {
        let mut args = Vec::new();
        let mut named: Vec<(String, Expr)> = Vec::new();
        if !self.check(&Token::RParen) {
            loop {
                // `name = expr` — only when an IDENT is directly followed by a single `=`.
                if let Token::Ident(name) = self.peek()
                    && self.peek_at(1) == &Token::Assign
                {
                    let name = name.clone();
                    self.advance(); // ident
                    self.advance(); // '='
                    let value = self.parse_expr()?;
                    named.push((name, value));
                } else {
                    if !named.is_empty() {
                        return Err(
                            self.err("positional argument after named argument".to_string())
                        );
                    }
                    args.push(self.parse_expr()?);
                }
                if !self.eat(&Token::Comma) {
                    break;
                }
                // optional trailing comma: a `)` right after a comma ends the argument list
                if self.check(&Token::RParen) {
                    break;
                }
            }
        }
        self.expect(&Token::RParen)?;
        Ok((args, named))
    }

    /// Parse a bracket `obj[…]` that is not a `[types](` call, the `[` not yet consumed, into one
    /// `Index` carrying every reading that parses (TICKET-222): `types` by the type grammar
    /// (`parse_type`, comma-separated; only on an `Ident` or non-tuple `Field` head) and `index` by
    /// `parse_subscript`'s expression grammar, kept only when it ends at the same `]`. A slice is
    /// returned as `parse_subscript` builds it. When no reading parses, `parse_subscript` runs
    /// again non-speculatively so its error surfaces. The parser never chooses; the checker does.
    /// On any other head the type reading is tried only to name the mistake: a type list that
    /// parses where the subscript does not (`1[T, U]`) reports [`TYPE_LIST_HEAD`].
    fn parse_bracket(&mut self, obj: Expr, span: Span) -> PResult<Expr> {
        let headed = obj.kind.is_named_head();
        let save = self.pos;
        // `parse_type` bumps `self.depth` and only unwinds it on success; restore on backtrack.
        // `fold_depth` rides along for the same reason (see `MAX_AST_DEPTH`).
        let save_depth = self.depth;
        let save_fold = self.fold_depth;
        let restore = |p: &mut Self, pos: usize| {
            p.pos = pos;
            p.depth = save_depth;
            p.fold_depth = save_fold;
        };
        let mut types = Vec::new();
        let mut end = None;
        self.advance(); // '['
        loop {
            match self.parse_type() {
                Ok(t) => types.push(t),
                Err(_) => {
                    types.clear();
                    break;
                }
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        if !types.is_empty() && self.eat(&Token::RBracket) {
            end = Some(self.pos);
        } else {
            types.clear();
        }
        restore(self, save);
        let Some(end) = end.filter(|_| headed) else {
            let bracket = self.cur_span();
            self.advance(); // '['
            return self.parse_subscript(obj, span).map_err(|e| {
                if end.is_some() {
                    ParseError {
                        message: TYPE_LIST_HEAD.to_string(),
                        span: bracket,
                    }
                } else {
                    e
                }
            });
        };
        self.advance(); // '['
        let index = match self.parse_subscript(obj.clone(), span) {
            Ok(Expr {
                kind: ExprKind::Index { index, .. },
                ..
            }) if self.pos == end => index,
            _ => None,
        };
        restore(self, end);
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Index {
                obj: Box::new(obj),
                index,
                types,
            },
            span,
        })
    }

    /// Speculatively parse `[Type, …](args)` as a call with explicit type arguments, assuming the
    /// `[` has NOT yet been consumed and `callee` is a bare name. Returns `Ok(None)` (position
    /// restored) when the `[types](` shape isn't present, so the caller falls back to indexing
    /// (`arr[i]`, `fns[0](x)`). Once `]` `(` is confirmed it commits, so a genuine error inside the
    /// argument list propagates instead of silently re-parsing as an index.
    fn try_parse_type_arg_call(&mut self, callee: &Expr, span: Span) -> PResult<Option<Expr>> {
        let save = self.pos;
        // `parse_type` bumps `self.depth` and only unwinds it on success; on a swallowed (backtrack)
        // failure we must restore depth too, else every plain `name[idx]` leaks a level and many
        // such indexes spuriously trip MAX_DEPTH. `fold_depth` rides along for the same reason.
        let save_depth = self.depth;
        let save_fold = self.fold_depth;
        self.advance(); // '['
        let mut type_args = Vec::new();
        loop {
            match self.parse_type() {
                Ok(t) => type_args.push(t),
                Err(_) => {
                    self.pos = save;
                    self.depth = save_depth;
                    self.fold_depth = save_fold;
                    return Ok(None);
                }
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        if !self.eat(&Token::RBracket) || !self.check(&Token::LParen) {
            self.pos = save;
            self.depth = save_depth;
            self.fold_depth = save_fold;
            return Ok(None);
        }
        // A one-arg bracket on a bare-name or member head keeps its expression reading too: the
        // checker, which knows the scope, chooses index-then-call when the head denotes data
        // (`fs[k](10)`). The parser reads no scope (DEC-204).
        let bracket = self.bracket_expression(callee, &type_args, save, save_depth, save_fold);
        self.advance(); // '(' — committed to a type-argument call now.
        let (args, named) = self.parse_call_args()?;
        Ok(Some(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Call {
                callee: Box::new(callee.clone()),
                args,
                named,
                type_args,
                bracket,
            },
            span,
        }))
    }

    /// The expression reading of the bracket `[X]` that `try_parse_type_arg_call` just read as one
    /// type, from `save` (the `[`) to the `(` at the current position. `Some` only for an `Ident`
    /// head or a non-tuple-index `Field` head, and only when `X` parses as an expression that ends
    /// at the `]`. Leaves the position at the `(`, with `depth` and `fold_depth` restored.
    fn bracket_expression(
        &mut self,
        callee: &Expr,
        type_args: &[Type],
        save: usize,
        save_depth: usize,
        save_fold: usize,
    ) -> Option<Box<Expr>> {
        let headed = callee.kind.is_named_head();
        if !headed || type_args.len() != 1 {
            return None;
        }
        let end = self.pos;
        self.pos = save + 1;
        self.depth = save_depth;
        self.fold_depth = save_fold;
        let parsed = self.parse_expr();
        let fits = self.pos + 1 == end;
        self.pos = end;
        self.depth = save_depth;
        self.fold_depth = save_fold;
        match parsed {
            Ok(e) if fits => Some(Box::new(e)),
            _ => None,
        }
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let tok = self.advance();
        let span = tok.span;
        let kind = match tok.kind {
            Token::Int(n) => ExprKind::Int(n),
            // Bare (un-negated) 2^63: legal ONLY as the operand of a unary minus (folded in
            // `parse_unary`). Reaching here means it stands alone → out of i64 range.
            Token::IntMinMagnitude => {
                return Err(ParseError {
                    message: "number too large to fit in target type".to_string(),
                    span,
                });
            }
            Token::Float(f) => ExprKind::Float(f),
            Token::Str(s) => match s.plain() {
                Some(text) => ExprKind::Str(text.to_string()),
                None => ExprKind::Interp(self.interp_chunks(s)?),
            },
            Token::Bytes(b) => ExprKind::Bytes(b),
            Token::RawStr(s) => ExprKind::RawStr(s),
            Token::True => ExprKind::Bool(true),
            Token::False => ExprKind::Bool(false),
            Token::NoneKw => ExprKind::NoneLit,
            Token::Ident(name) => {
                self.reject_removed_name(&name, span)?;
                if name == "range" && self.check(&Token::LParen) {
                    return Err(ParseError {
                        message: REMOVED_RANGE.to_string(),
                        span,
                    });
                }
                ExprKind::Ident(name)
            }
            Token::LParen => {
                // `()` stays unchanged (no inner expr → falls through to the error path below);
                // `(e)` is grouping; `(e1, e2, …)` is a tuple; `(e,)` is a parse error.
                if self.check(&Token::RParen) {
                    return Err(self.err("unexpected ')' in expression".to_string()));
                }
                let first = self.parse_expr()?;
                if self.eat(&Token::Comma) {
                    // A comma after the first element ⇒ a tuple. `(e,)` is a 1-element tuple
                    // (distinct from grouping `(e)`); `(e1, e2,)` allows an optional trailing comma.
                    let mut elems = vec![first];
                    // The just-eaten comma may be the trailing one (`(e,)` or `(…,)`): stop here.
                    while !self.check(&Token::RParen) {
                        elems.push(self.parse_expr()?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                    self.expect(&Token::RParen)?;
                    ExprKind::Tuple(elems)
                } else {
                    self.expect(&Token::RParen)?;
                    return Ok(first); // grouped — keep the inner expr (and its span)
                }
            }
            Token::LBracket => {
                // `[]` is the empty list. A first element followed by `for` is a list
                // comprehension (`[elem for x in xs if g]`); otherwise a plain list literal.
                if self.check(&Token::RBracket) {
                    self.advance();
                    ExprKind::List(Vec::new(), None)
                } else {
                    let first = self.parse_expr()?;
                    if self.check(&Token::For) {
                        let clauses = self.parse_comp_clauses()?;
                        self.expect(&Token::RBracket)?;
                        ExprKind::Comprehension {
                            kind: CompKind::List,
                            key: None,
                            elem: Box::new(first),
                            clauses,
                        }
                    } else {
                        let mut elems = vec![first];
                        while self.eat(&Token::Comma) {
                            // optional trailing comma: a `]` right after a comma ends the literal
                            if self.check(&Token::RBracket) {
                                break;
                            }
                            elems.push(self.parse_expr()?);
                        }
                        self.expect(&Token::RBracket)?;
                        ExprKind::List(elems, None)
                    }
                }
            }
            Token::LBrace => {
                // `{}` is the empty map. Otherwise the first element decides: a `key: value` pair
                // makes it a map (or, before `for`, a map comprehension); a bare expression makes
                // it a set (or a set comprehension before `for`). The empty set is `Set()`.
                if self.check(&Token::RBrace) {
                    self.advance();
                    ExprKind::Map(Vec::new())
                } else {
                    let first = self.parse_expr()?;
                    if self.eat(&Token::Colon) {
                        let value = self.parse_expr()?;
                        if self.check(&Token::For) {
                            // Map comprehension: `{k: v for k, v in m}`.
                            let clauses = self.parse_comp_clauses()?;
                            self.expect(&Token::RBrace)?;
                            ExprKind::Comprehension {
                                kind: CompKind::Map,
                                key: Some(Box::new(first)),
                                elem: Box::new(value),
                                clauses,
                            }
                        } else {
                            // Map literal: finish the first pair, then the rest.
                            let mut entries = vec![(first, value)];
                            while self.eat(&Token::Comma) {
                                // optional trailing comma: a `}` right after a comma ends the map
                                if self.check(&Token::RBrace) {
                                    break;
                                }
                                let key = self.parse_expr()?;
                                self.expect(&Token::Colon)?;
                                let value = self.parse_expr()?;
                                entries.push((key, value));
                            }
                            self.expect(&Token::RBrace)?;
                            ExprKind::Map(entries)
                        }
                    } else if self.check(&Token::For) {
                        // Set comprehension: `{x for x in xs}`.
                        let clauses = self.parse_comp_clauses()?;
                        self.expect(&Token::RBrace)?;
                        ExprKind::Comprehension {
                            kind: CompKind::Set,
                            key: None,
                            elem: Box::new(first),
                            clauses,
                        }
                    } else {
                        // Set literal: a comma-separated list of elements.
                        let mut elems = vec![first];
                        while self.eat(&Token::Comma) {
                            // optional trailing comma: a `}` right after a comma ends the set
                            if self.check(&Token::RBrace) {
                                break;
                            }
                            elems.push(self.parse_expr()?);
                        }
                        self.expect(&Token::RBrace)?;
                        ExprKind::Set(elems)
                    }
                }
            }
            Token::Fn => return self.parse_closure(span),
            // Expression-position `match`/`if` (the keyword was already consumed by `advance`).
            // Statement-position `if`/`match` never reach here — `parse_stmt` dispatches them first.
            Token::Match => return self.parse_match_expr(span),
            Token::If => return self.parse_if_expr(span),
            Token::Recover => return self.parse_recover_expr(span),
            other @ Token::Return
            | other @ Token::Else
            | other @ Token::Elif
            | other @ Token::For
            | other @ Token::While
            | other @ Token::In
            | other @ Token::Break
            | other @ Token::Continue
            | other @ Token::Pass
            | other @ Token::Struct
            | other @ Token::Enum
            | other @ Token::Protocol
            | other @ Token::Type
            | other @ Token::Defer
            | other @ Token::Assert
            | other @ Token::Test
            | other @ Token::Spawn
            | other @ Token::Parallel
            | other @ Token::Wait
            | other @ Token::Yield
            | other @ Token::Import
            | other @ Token::Extern
            | other @ Token::Native
            | other @ Token::From
            | other @ Token::As
            | other @ Token::Const
            | other @ Token::And
            | other @ Token::Or
            | other @ Token::Not
            | other @ Token::Where
            | other @ Token::Plus
            | other @ Token::Minus
            | other @ Token::Star
            | other @ Token::Slash
            | other @ Token::Percent
            | other @ Token::Assign
            | other @ Token::Walrus
            | other @ Token::EqEq
            | other @ Token::NotEq
            | other @ Token::Lt
            | other @ Token::LtEq
            | other @ Token::Gt
            | other @ Token::GtEq
            | other @ Token::PlusEq
            | other @ Token::MinusEq
            | other @ Token::StarEq
            | other @ Token::SlashEq
            | other @ Token::PercentEq
            | other @ Token::AmpEq
            | other @ Token::PipeEq
            | other @ Token::CaretEq
            | other @ Token::ShlEq
            | other @ Token::ShrEq
            | other @ Token::Arrow
            | other @ Token::Pipe
            | other @ Token::Question
            | other @ Token::QuestionDot
            | other @ Token::QuestionQuestion
            | other @ Token::Bang
            | other @ Token::Amp
            | other @ Token::Caret
            | other @ Token::BitOr
            | other @ Token::Shl
            | other @ Token::Shr
            | other @ Token::RParen
            | other @ Token::RBracket
            | other @ Token::RBrace
            | other @ Token::Comma
            | other @ Token::Colon
            | other @ Token::Dot
            | other @ Token::DotDot
            | other @ Token::DotDotEq
            | other @ Token::DotDotDot
            | other @ Token::Newline
            | other @ Token::Indent
            | other @ Token::Dedent
            | other @ Token::Eof => {
                return Err(ParseError {
                    message: format!("unexpected {} in expression", describe_unexpected(&other)),
                    span,
                });
            }
        };
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind,
            span,
        })
    }

    /// Build the chunks of an interpolated literal from what the lexer found: its text parts and
    /// its holes. The lexer owns where a hole starts and ends (`Lexer::quoted`); this only parses
    /// the tokens it handed over, so nothing here reads a brace, a quote or a colon.
    fn interp_chunks(&mut self, s: StrLit) -> PResult<Vec<Chunk>> {
        let mut chunks = Vec::with_capacity(s.parts.len());
        for part in s.parts {
            match part {
                StrPart::Text(text) => chunks.push(Chunk::Lit(text)),
                StrPart::Hole(hole) => {
                    let expr = self.parse_hole(hole.tokens).map_err(|mut e| {
                        if hole.spec.is_some() {
                            e.message.push_str(
                                "; the first ':' outside brackets starts the format spec, so wrap an expression that contains ':' in parentheses",
                            );
                        }
                        e
                    })?;
                    let (spec, fields) = match hole.spec {
                        None => (None, Vec::new()),
                        Some(parts) => {
                            let mut items = Vec::new();
                            let mut fields = Vec::new();
                            for part in parts {
                                match part {
                                    SpecPart::Text(text) => {
                                        items.extend(text.chars().map(SpecItem::Ch));
                                    }
                                    SpecPart::Field(tokens) => {
                                        if let [only] = &tokens[..] {
                                            return Err(ParseError {
                                                message: "format spec: a nested field needs an expression, e.g. '{w}'".to_string(),
                                                span: only.span,
                                            });
                                        }
                                        items.push(SpecItem::Field);
                                        fields.push(self.parse_hole(tokens)?);
                                    }
                                }
                            }
                            let spec = crate::fmtspec::parse_nested(&items).map_err(|message| {
                                ParseError {
                                    message,
                                    span: hole.spec_at,
                                }
                            })?;
                            (Some(spec), fields)
                        }
                    };
                    chunks.push(Chunk::Expr(expr, spec, fields));
                }
            }
        }
        Ok(chunks)
    }

    /// Parse the tokens of one hole (or one nested spec field) as exactly one expression, on this
    /// parser: `depth` and `fold_depth` carry on, so a hole counts against the same bounds as the
    /// code around it. The hole's tokens stand in for the module's while it parses; token-indexed
    /// state (`if_line`, `bang_eq`) belongs to the module's tokens and is set aside.
    fn parse_hole(&mut self, tokens: Vec<Tok>) -> PResult<Expr> {
        self.hole_parsed = true;
        let outer_toks = std::mem::replace(&mut self.toks, tokens);
        let outer_pos = std::mem::replace(&mut self.pos, 0);
        let outer_if_line = self.if_line.take();
        let outer_value = self.value_start.take();
        let outer_inline = std::mem::replace(&mut self.stmt_inline, false);
        let outer_bang = self.bang_eq.take();
        let result = self.hole_expr();
        self.toks = outer_toks;
        self.pos = outer_pos;
        self.if_line = outer_if_line;
        self.value_start = outer_value;
        self.stmt_inline = outer_inline;
        self.bang_eq = outer_bang;
        result
    }

    fn hole_expr(&mut self) -> PResult<Expr> {
        self.skip_newlines();
        let expr = self.parse_expr()?;
        self.skip_newlines();
        if !self.check(&Token::Eof) {
            return Err(self.err(format!(
                "unexpected {} after expression",
                describe_unexpected(self.peek())
            )));
        }
        Ok(expr)
    }

    /// `fn(params) [-> ret]: expr` — the `fn` keyword has already been consumed.
    fn parse_closure(&mut self, span: Span) -> PResult<Expr> {
        self.expect(&Token::LParen)?;
        let params = self.parse_params(false)?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&Token::Colon)?;
        let body = if self.check(&Token::Pass) {
            let pass_span = self.cur_span();
            self.advance();
            Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Pass,
                span: pass_span,
            }
        } else {
            self.parse_value(ValueSite::Lambda)?
        };
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Closure {
                params,
                ret,
                body: Box::new(body),
            },
            span,
        })
    }
}

/// Does this function body contain a `yield` that belongs to *this* function? Recurses through
/// compound statements' sub-blocks but deliberately does NOT descend into a nested `fn`
/// definition (its yields are its own) nor into closure expressions (a closure is an expression,
/// so it is never reached by this statement-only walk — a `yield` inside one stays invisible here
/// and is later flagged by the checker as "yield outside a generator").
fn body_contains_yield(block: &Block) -> bool {
    block.iter().any(stmt_contains_yield)
}

fn stmt_contains_yield(s: &Stmt) -> bool {
    // A `yield` can also live in a `recover:` block in expression position (`x := recover: … yield …`),
    // which the statement structure does not reach — scan those too. Stops at closures (a closure's
    // yields are its own). See `ast::expr_recover_blocks`.
    let mut recover_blocks = Vec::new();
    stmt_expr_recover_blocks(s, &mut recover_blocks);
    if recover_blocks.iter().any(|b| body_contains_yield(b)) {
        return true;
    }
    match &s.kind {
        StmtKind::Yield(_) => true,
        StmtKind::If {
            branches,
            else_block,
        } => {
            branches.iter().any(|(_, b)| body_contains_yield(b))
                || else_block.as_ref().is_some_and(body_contains_yield)
        }
        StmtKind::For { body, .. } | StmtKind::While { body, .. } | StmtKind::Parallel { body } => {
            body_contains_yield(body)
        }
        StmtKind::Match { arms, .. } => arms.iter().any(|a| body_contains_yield(&a.body)),
        StmtKind::Defer(DeferTarget::Block(b)) | StmtKind::Spawn(SpawnTarget::Block(b)) => {
            body_contains_yield(b)
        }
        StmtKind::Wait { arms, else_block } => {
            arms.iter().any(|a| body_contains_yield(&a.body))
                || else_block.as_ref().is_some_and(body_contains_yield)
        }
        // A nested `fn` owns its own yields — do not descend.
        StmtKind::Let { .. }
        | StmtKind::Assign { .. }
        | StmtKind::Fn(_)
        | StmtKind::Struct { .. }
        | StmtKind::Protocol { .. }
        | StmtKind::Enum { .. }
        | StmtKind::TypeAlias { .. }
        | StmtKind::Return(_)
        | StmtKind::Defer(_)
        | StmtKind::Spawn(_)
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Pass
        | StmtKind::Import(_)
        | StmtKind::Extern { .. }
        | StmtKind::Native(_)
        | StmtKind::NativeStruct { .. }
        | StmtKind::NativeEnum { .. }
        | StmtKind::NativeType { .. }
        | StmtKind::Assert { .. }
        | StmtKind::Expr(_) => false,
    }
}

/// Render a token as the user would recognize it, for error messages — `':='` not `Walrus`.
fn describe(tok: &Token) -> String {
    use Token::*;
    let s = match tok {
        Plus => "'+'",
        Minus => "'-'",
        Star => "'*'",
        Slash => "'/'",
        Percent => "'%'",
        Assign => "'='",
        Walrus => "':='",
        EqEq => "'=='",
        NotEq => "'!='",
        Lt => "'<'",
        LtEq => "'<='",
        Gt => "'>'",
        GtEq => "'>='",
        PlusEq => "'+='",
        MinusEq => "'-='",
        Arrow => "'->'",
        Pipe => "'|>'",
        Question => "'?'",
        Bang => "'!'",
        LParen => "'('",
        RParen => "')'",
        LBracket => "'['",
        RBracket => "']'",
        LBrace => "'{'",
        RBrace => "'}'",
        Comma => "','",
        Colon => "':'",
        Dot => "'.'",
        DotDot => "'..'",
        DotDotEq => "'..='",
        Newline => "end of line",
        Indent => "an indented block",
        Dedent => "a dedent",
        Eof => "end of input",
        Ident(name) => return format!("identifier '{name}'"),
        Int(n) => return format!("integer {n}"),
        IntMinMagnitude => return "integer 9223372036854775808".to_string(),
        Float(f) => return format!("float {f}"),
        Str(_) => "a string literal",
        Bytes(_) => "a byte-string literal",
        RawStr(_) => "a raw-string literal",
        // operators and keywords print their source spelling
        other @ Fn
        | other @ Return
        | other @ If
        | other @ Else
        | other @ Elif
        | other @ For
        | other @ While
        | other @ In
        | other @ Break
        | other @ Continue
        | other @ Pass
        | other @ Struct
        | other @ Enum
        | other @ Protocol
        | other @ Type
        | other @ Match
        | other @ Recover
        | other @ Defer
        | other @ Assert
        | other @ Test
        | other @ Spawn
        | other @ Parallel
        | other @ Wait
        | other @ Yield
        | other @ Import
        | other @ Extern
        | other @ Native
        | other @ From
        | other @ As
        | other @ Const
        | other @ And
        | other @ Or
        | other @ Not
        | other @ True
        | other @ False
        | other @ NoneKw
        | other @ Where
        | other @ StarEq
        | other @ SlashEq
        | other @ PercentEq
        | other @ AmpEq
        | other @ PipeEq
        | other @ CaretEq
        | other @ ShlEq
        | other @ ShrEq
        | other @ QuestionDot
        | other @ QuestionQuestion
        | other @ Amp
        | other @ Caret
        | other @ BitOr
        | other @ Shl
        | other @ Shr
        | other @ DotDotDot => match other.lexeme() {
            Some(l) => return format!("'{l}'"),
            None => return format!("'{}'", format!("{other:?}").to_lowercase()),
        },
    };
    s.to_string()
}

/// [`describe`] without its leading article, for the two messages that put `unexpected` in front
/// (`unexpected indented block in expression`, not `unexpected an indented block ...`). CPython 3.14
/// on a stray indent prints `IndentationError: unexpected indent` — no article. The other callers
/// read `found {}` / `expected {}` / `{} was never closed`, where the article is correct English.
fn describe_unexpected(tok: &Token) -> String {
    let d = describe(tok);
    match d.strip_prefix("an ").or_else(|| d.strip_prefix("a ")) {
        Some(rest) => rest.to_string(),
        None => d,
    }
}

/// An infix operator: a normal binary op, the range marker `..`, or the pipe `|>` (M6).
enum InfixOp {
    Bin(BinaryOp),
    Range,
    Pipe,
    /// Null-coalescing `??` — right-associative carrier. It SURVIVES desugar (W7-43); the checker
    /// lowers it to a `match` via `desugar::lower_carrier_option`, and rejects a non-`Option` lhs.
    Coalesce,
}

/// The one leaf of a binding target: `_` skips a slot (`Pattern::Wildcard`), any other name binds
/// (`Pattern::Ident`). Every target the parser builds (`:=`, typed let, `for`, comprehension, wait
/// arm) goes through it, so a blank is encoded one way.
fn bind_leaf(name: String, span: Span) -> Pattern {
    if name == "_" {
        Pattern::Wildcard
    } else {
        Pattern::Ident(name, span, crate::ast::NodeId::fresh())
    }
}

/// Map a token to its infix operator and (left, right) binding powers, per `docs/syntax.md` §4.
/// All operators are left-associative (`right = left + 1`). `None` means "not an infix operator".
/// True for the compound-assignment tokens (`+= -= *= /= %= &= |= ^= <<= >>=`). Used to give a
/// clear error when one appears with a multi-target list (`a, b += 1`).
/// True iff `e` is a place `=` can store into: a name, a field, an index, or a parenthesized
/// tuple of places (`a, (b.f, c[0]) = t`). Every other expression kind is not a place. `listed` is
/// true when `e` is a comma list the statement wrote without outer parentheses (U9: no outer
/// parentheses): a tuple at the top takes it, a parenthesized top-level tuple `(a, b) = t` does not.
/// A nested tuple always takes parentheses.
fn is_assign_target(e: &Expr, listed: bool) -> bool {
    match &e.kind {
        ExprKind::Ident(_) | ExprKind::Field { .. } | ExprKind::Index { index: Some(_), .. } => {
            true
        }
        ExprKind::Tuple(elems) => listed && elems.iter().all(|el| is_assign_target(el, true)),
        ExprKind::Index { index: None, .. }
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::List(..)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Compare { .. }
        | ExprKind::Range { .. }
        | ExprKind::Call { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => false,
    }
}

fn is_compound_assign(tok: &Token) -> bool {
    matches!(
        tok,
        Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
    )
}

/// Binding power of prefix `not`, gating `parse_bp`'s `min_bp <= NOT_BP` check. It sits between
/// `and` (5) and the comparisons (7), matching Python: `not x in xs` is `not (x in xs)`.
const NOT_BP: u8 = 6;

/// Binding power of `..` / `..=` (TICKET-253): below arithmetic, shifts and the bitwise operators,
/// above comparison, `in`, `not`, `and` and `or`. `0..1 << 2` is `0..(1 << 2)`; `x in 0..3` and
/// `0..3 == 0..3` need no parentheses. A range does not chain: `a..b..c` is an error.
const RANGE_BP: u8 = 9;

/// A range with no end bound (`5..`, `xs[2..]`, `..`) is a range whose next token cannot start an
/// expression. The list names every token that closes or continues a surrounding construct.
fn token_ends_range(t: &Token) -> bool {
    matches!(
        t,
        Token::Colon
            | Token::RBracket
            | Token::RParen
            | Token::RBrace
            | Token::Comma
            | Token::Newline
            | Token::Eof
            | Token::Dedent
            | Token::And
            | Token::Or
            | Token::Pipe
            | Token::Assign
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::In
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::If
            | Token::BitOr
            | Token::Caret
            | Token::Amp
            | Token::Shl
            | Token::Shr
            | Token::Plus
            | Token::Star
            | Token::Slash
            | Token::Percent
    )
}

fn infix_op(tok: &Token) -> Option<(InfixOp, u8, u8)> {
    use BinaryOp::*;
    use InfixOp::*;
    // Precedence ladder (loosest → tightest). Bitwise ops follow Python's relative order: comparison
    // is looser than `|` < `^` < `&` < shifts, and shifts are looser than additive (gap #13).
    let (op, l) = match tok {
        // Pipe `|>` is the lowest-precedence infix op (level 1): the whole expression to its left
        // is threaded into the call on its right. Left-associative (`a |> f |> g` = `(a|>f)|>g`).
        Token::Pipe => (Pipe, 1),
        Token::Or => (Bin(Or), 3),
        Token::And => (Bin(And), 5),
        Token::EqEq => (Bin(Eq), 7),
        Token::NotEq => (Bin(NotEq), 7),
        // `in` membership — comparison-level precedence (same as `==`). `for x in xs:` never
        // reaches here: `parse_for`/`parse_comp_clause` consume `in` explicitly via `expect`.
        Token::In => (Bin(In), 7),
        // TICKET-077: all seven comparisons (`==`/`!=`/`in`/`<`/`<=`/`>`/`>=`) share ONE level and
        // CHAIN like Python (`a < b == c` means `a < b and b == c`) — see `parse_bp`'s comparison
        // fold and `ExprKind::Compare`.
        Token::Lt => (Bin(Lt), 7),
        Token::LtEq => (Bin(LtEq), 7),
        Token::Gt => (Bin(Gt), 7),
        Token::GtEq => (Bin(GtEq), 7),
        Token::BitOr => (Bin(BitOr), 11),
        Token::Caret => (Bin(BitXor), 13),
        Token::Amp => (Bin(BitAnd), 15),
        Token::Shl => (Bin(Shl), 17),
        Token::Shr => (Bin(Shr), 17),
        Token::DotDot | Token::DotDotEq => (Range, 9),
        Token::Plus => (Bin(Add), 21),
        Token::Minus => (Bin(Sub), 21),
        Token::Star => (Bin(Mul), 23),
        Token::Slash => (Bin(Div), 23),
        Token::Percent => (Bin(Mod), 23),
        // Null-coalescing `??`: binds tighter than every binary operator and looser than unary
        // `-` and the postfix chain, so `a ?? b + 1` is `(a ?? b) + 1`. RIGHT-associative
        // (`a ?? b ?? c` = `a ?? (b ?? c)`) — the one exception to the `l+1` left-assoc rule
        // above, so it returns early with equal left/right binding powers.
        Token::QuestionQuestion => return Some((Coalesce, 25, 25)),
        Token::Int(_)
        | Token::IntMinMagnitude
        | Token::Float(_)
        | Token::Str(_)
        | Token::Bytes(_)
        | Token::RawStr(_)
        | Token::Ident(_)
        | Token::Fn
        | Token::Return
        | Token::If
        | Token::Else
        | Token::Elif
        | Token::For
        | Token::While
        | Token::Break
        | Token::Continue
        | Token::Pass
        | Token::Struct
        | Token::Enum
        | Token::Protocol
        | Token::Type
        | Token::Match
        | Token::Recover
        | Token::Defer
        | Token::Assert
        | Token::Test
        | Token::Spawn
        | Token::Parallel
        | Token::Wait
        | Token::Yield
        | Token::Import
        | Token::Extern
        | Token::Native
        | Token::From
        | Token::As
        | Token::Const
        | Token::Not
        | Token::True
        | Token::False
        | Token::NoneKw
        | Token::Where
        | Token::Assign
        | Token::Walrus
        | Token::PlusEq
        | Token::MinusEq
        | Token::StarEq
        | Token::SlashEq
        | Token::PercentEq
        | Token::AmpEq
        | Token::PipeEq
        | Token::CaretEq
        | Token::ShlEq
        | Token::ShrEq
        | Token::Arrow
        | Token::Question
        | Token::QuestionDot
        | Token::Bang
        | Token::LParen
        | Token::RParen
        | Token::LBracket
        | Token::RBracket
        | Token::LBrace
        | Token::RBrace
        | Token::Comma
        | Token::Colon
        | Token::Dot
        | Token::DotDotDot
        | Token::Newline
        | Token::Indent
        | Token::Dedent
        | Token::Eof => return None,
    };
    Some((op, l, l + 1))
}

/// True for the seven comparison operators, which all share one precedence level and CHAIN
/// (TICKET-077) — see `infix_op`'s comment and `parse_bp`'s comparison fold.
fn is_comparison(op: BinaryOp) -> bool {
    use BinaryOp::*;
    matches!(op, Eq | NotEq | In | Lt | LtEq | Gt | GtEq)
}

#[cfg(test)]
mod tests;
