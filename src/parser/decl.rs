//! Parser: declarations: fns, type params, `where` bounds, protocols, extern and native decls,
//! params, structs, aliases, enums.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::stmt::body_contains_yield;
use super::*;
use crate::lexer::Token;

impl Parser {
    /// `test fn …` — consume the `test` modifier, parse the following `fn`, and tag it `is_test`.
    /// Shared by the free-fn site (`parse_stmt`) and the struct-method site (`parse_struct`).
    pub(super) fn parse_test_fn(&mut self, allow_defaults: bool) -> PResult<FnDecl> {
        self.expect(&Token::Test)?;
        if !self.check(&Token::Fn) {
            return Err(self.err("`test` must be followed by `fn`".to_string()));
        }
        let mut decl = self.parse_fn(allow_defaults)?;
        decl.is_test = true;
        Ok(decl)
    }

    pub(super) fn parse_fn(&mut self, allow_defaults: bool) -> PResult<FnDecl> {
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

    pub(super) fn parse_protocol(&mut self) -> PResult<StmtKind> {
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
    pub(super) fn parse_native_decl(&mut self) -> PResult<StmtKind> {
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
        let first_is_self = params.first().is_some_and(Param::is_self);
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
        if let Some(p) = params.iter().skip(1).find(|p| p.is_self()) {
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
    pub(super) fn parse_extern(&mut self) -> PResult<StmtKind> {
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
    pub(super) fn parse_params(&mut self, allow_defaults: bool) -> PResult<Vec<Param>> {
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
                // A name is bound once per list (Go: `duplicate argument`; Rust: E0415). A second
                // `self` is never first, so the callers' `self` rule reports it.
                if !is_blank_name(&name)
                    && !crate::ast::is_self_name(&name)
                    && params.iter().any(|p: &Param| p.name == name)
                {
                    return Err(ParseError {
                        message: format!(
                            "identifier '{name}' is bound more than once in this parameter list"
                        ),
                        span: name_span,
                    });
                }
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

    pub(super) fn parse_struct(&mut self) -> PResult<StmtKind> {
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
    pub(super) fn parse_type_alias(&mut self) -> PResult<StmtKind> {
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

    pub(super) fn parse_enum(&mut self) -> PResult<StmtKind> {
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
}
