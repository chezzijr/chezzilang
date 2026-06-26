//! Seeded, type-directed program generator.
//!
//! Everything is correct by construction: every `Var`/`Index`/`Call` is in scope and
//! well-typed, every divisor is non-zero, every container index is in range, and — most
//! importantly — every integer value is provably within a safe window so it can never hit
//! Chezzi's i64-overflow fault. That last property is what makes a divergence meaningful: if
//! the program can't legitimately overflow, a Chezzi fault is a real bug, not a generator
//! artifact.
//!
//! Integer-bound discipline:
//!   * leaf literals are tiny (`LEAF_INT_MAX`),
//!   * every int expression carries a conservative absolute bound; a node whose bound would
//!     exceed `MAX_BOUND` is rejected and re-rolled toward a leaf,
//!   * loops carry a known trip cap; an in-loop `+=`/`-=` widens the target's bound by
//!     `loop_mult * delta_bound` immediately (pessimistic but safe). No `*=` or self-
//!     referential reassignment inside loops.

use super::ast::*;
use super::rng::Rng;

const LEAF_INT_MAX: i64 = 100;
const PARAM_BOUND: i128 = 1000;
const MAX_BOUND: i128 = 1_000_000_000_000; // 10^12, far under i64::MAX (~9.2e18)
const LOOP_CAP: i64 = 20;
const MAX_EXPR_DEPTH: usize = 3;

#[derive(Clone, Copy)]
pub struct Features {
    pub div_mod: bool,
    pub control_flow: bool,
    pub while_loops: bool,
    pub collections: bool,
    pub functions: bool,
    pub floats: bool,
}

impl Features {
    /// Everything we trust today (floats stay off by default — see the float-format risk).
    pub fn full() -> Self {
        Features {
            div_mod: true,
            control_flow: true,
            while_loops: true,
            collections: true,
            functions: true,
            floats: false,
        }
    }
    pub fn straight_line() -> Self {
        Features {
            div_mod: false,
            control_flow: false,
            while_loops: false,
            collections: false,
            functions: false,
            floats: false,
        }
    }
}

struct VarInfo {
    name: String,
    ty: Ty,
    bound: i128,        // conservative |value| bound (Int only)
    len: Option<usize>, // known length (List/Map literals)
    keys: Vec<Expr>,    // known keys for Map indexing
    reserved: bool,     // loop counter — body must not reassign
}

struct FuncSig {
    name: String,
    params: Vec<Ty>,
    ret: Ty,
    ret_bound: i128,
}

pub struct Gen {
    rng: Rng,
    feat: Features,
    scope: Vec<VarInfo>,
    sigs: Vec<FuncSig>,
    next_var: usize,
    next_fn: usize,
    loop_mult: i128,   // product of enclosing loop trip caps (1 = straight-line)
    in_loop_rhs: bool, // generating the RHS of an in-loop `+=`/`-=`
}

pub fn gen_program(seed: u64, feat: Features) -> Program {
    let mut g = Gen {
        rng: Rng::seed(seed),
        feat,
        scope: Vec::new(),
        sigs: Vec::new(),
        next_var: 0,
        next_fn: 0,
        loop_mult: 1,
        in_loop_rhs: false,
    };
    let mut funcs = Vec::new();
    if feat.functions {
        let n = g.rng.below(3); // 0..2 functions
        for _ in 0..n {
            let f = g.gen_func();
            funcs.push(f);
        }
    }
    let main = g.gen_main_block();
    Program { funcs, main }
}

impl Gen {
    fn fresh_var(&mut self) -> String {
        let n = self.next_var;
        self.next_var += 1;
        format!("v{n}")
    }

    fn scope_mark(&self) -> usize {
        self.scope.len()
    }
    fn scope_reset(&mut self, mark: usize) {
        self.scope.truncate(mark);
    }

    fn vars_of(&self, ty: &Ty) -> Vec<usize> {
        self.scope
            .iter()
            .enumerate()
            .filter(|(_, v)| &v.ty == ty)
            .map(|(i, _)| i)
            .collect()
    }

    // ---- top-level structure --------------------------------------------------

    fn gen_main_block(&mut self) -> Block {
        let mark = self.scope_mark();
        let mut b = Vec::new();
        let n = 3 + self.rng.below(6) as usize; // 3..8 statements
        for _ in 0..n {
            if let Some(s) = self.gen_stmt(0) {
                b.push(s);
            }
        }
        // Always end with prints of the in-scope values so output is non-trivial.
        let snapshot: Vec<(String, Ty)> = self
            .scope
            .iter()
            .filter(|v| !v.reserved)
            .map(|v| (v.name.clone(), v.ty.clone()))
            .collect();
        for (name, _ty) in snapshot {
            b.push(Stmt::Print(vec![Expr::Var(name)]));
        }
        if b.is_empty() {
            b.push(Stmt::Print(vec![Expr::IntLit(0)]));
        }
        self.scope_reset(mark);
        b
    }

    fn gen_func(&mut self) -> Func {
        let name = {
            let n = self.next_fn;
            self.next_fn += 1;
            format!("f{n}")
        };
        let ret = self.rand_scalar_ty();
        let nparams = self.rng.below(3) as usize; // 0..2
        let mark = self.scope_mark();
        let mut params = Vec::new();
        for _ in 0..nparams {
            let ty = self.rand_scalar_ty();
            let pname = self.fresh_var();
            params.push((pname.clone(), ty.clone()));
            self.scope.push(VarInfo {
                name: pname,
                ty,
                bound: PARAM_BOUND,
                len: None,
                keys: Vec::new(),
                reserved: false,
            });
        }
        // small body of straight-line lets (functions stay simple & non-recursive)
        let mut body = Vec::new();
        let nstmt = self.rng.below(3) as usize;
        for _ in 0..nstmt {
            if let Some(s) = self.gen_simple_let() {
                body.push(s);
            }
        }
        let (ret_expr, ret_bound) = self.gen_expr(&ret, 0);
        body.push(Stmt::Return(Some(ret_expr)));
        self.scope_reset(mark);

        self.sigs.push(FuncSig {
            name: name.clone(),
            params: params.iter().map(|(_, t)| t.clone()).collect(),
            ret: ret.clone(),
            ret_bound: ret_bound.min(MAX_BOUND),
        });
        Func {
            name,
            params,
            ret,
            body,
        }
    }

    fn gen_simple_let(&mut self) -> Option<Stmt> {
        let ty = self.rand_scalar_ty();
        let (init, bound) = self.gen_expr(&ty, 1);
        let name = self.fresh_var();
        self.scope.push(VarInfo {
            name: name.clone(),
            ty: ty.clone(),
            bound,
            len: None,
            keys: Vec::new(),
            reserved: false,
        });
        Some(Stmt::Let { name, ty, init })
    }

    // ---- statements -----------------------------------------------------------

    fn gen_stmt(&mut self, depth: usize) -> Option<Stmt> {
        let mut choices: Vec<u8> = vec![0, 0, 1]; // bias toward Let + Print
        if self.feat.control_flow && depth < 2 {
            choices.push(2); // if
            choices.push(3); // for-range
        }
        if self.feat.while_loops && depth < 2 {
            choices.push(4); // while
        }
        if !self.scope.iter().any(|v| !v.reserved) {
            // nothing to assign/print yet — force a Let
            return self.gen_let();
        }
        match *self.rng.choice(&choices) {
            0 => self.gen_let(),
            1 => self.gen_print_or_assign(),
            2 => self.gen_if(depth),
            3 => self.gen_for(depth),
            4 => self.gen_while(depth),
            _ => self.gen_let(),
        }
    }

    fn gen_let(&mut self) -> Option<Stmt> {
        let ty = self.rand_ty();
        let (init, bound, len, keys) = self.gen_init(&ty);
        let name = self.fresh_var();
        self.scope.push(VarInfo {
            name: name.clone(),
            ty: ty.clone(),
            bound,
            len,
            keys,
            reserved: false,
        });
        Some(Stmt::Let { name, ty, init })
    }

    fn gen_print_or_assign(&mut self) -> Option<Stmt> {
        // 50/50 between printing an in-scope value and mutating an int var.
        if self.rng.chance(0.5) {
            let candidates: Vec<usize> = self
                .scope
                .iter()
                .enumerate()
                .filter(|(_, v)| !v.reserved)
                .map(|(i, _)| i)
                .collect();
            if candidates.is_empty() {
                return None;
            }
            let idx = *self.rng.choice(&candidates);
            return Some(Stmt::Print(vec![Expr::Var(self.scope[idx].name.clone())]));
        }
        self.gen_assign()
    }

    fn gen_assign(&mut self) -> Option<Stmt> {
        let ints = self.int_assign_targets();
        if ints.is_empty() {
            return None;
        }
        let idx = *self.rng.choice(&ints);
        let in_loop = self.loop_mult > 1;
        // In a loop, only additive mutation, and widen the target's bound pessimistically.
        let op = if in_loop {
            if self.rng.chance(0.5) {
                AssignOp::Add
            } else {
                AssignOp::Sub
            }
        } else {
            *self
                .rng
                .choice(&[AssignOp::Set, AssignOp::Add, AssignOp::Sub])
        };
        // In a loop, the RHS must NOT read any mutable int var (the accumulator or another
        // in-loop-mutated var). Otherwise the delta compounds across iterations and the value
        // grows geometrically — overflowing i64 in Chezzi (a false finding) while Python
        // bignums absorb it. Restricting to loop-stable leaves keeps the widen bound exact.
        let prev = self.in_loop_rhs;
        self.in_loop_rhs = in_loop;
        let (value, vbound) = self.gen_expr(&Ty::Int, 1);
        self.in_loop_rhs = prev;
        match op {
            AssignOp::Set => {
                self.scope[idx].bound = vbound.min(MAX_BOUND);
            }
            AssignOp::Add | AssignOp::Sub => {
                let widen = self.loop_mult.saturating_mul(vbound);
                let new = self.scope[idx].bound.saturating_add(widen);
                if new > MAX_BOUND {
                    // would risk overflow — fall back to a print instead
                    return Some(Stmt::Print(vec![Expr::Var(self.scope[idx].name.clone())]));
                }
                self.scope[idx].bound = new;
            }
            AssignOp::Mul => unreachable!(),
        }
        Some(Stmt::Assign {
            name: self.scope[idx].name.clone(),
            op,
            value,
        })
    }

    fn int_assign_targets(&self) -> Vec<usize> {
        self.scope
            .iter()
            .enumerate()
            .filter(|(_, v)| v.ty == Ty::Int && !v.reserved)
            .map(|(i, _)| i)
            .collect()
    }

    fn gen_if(&mut self, depth: usize) -> Option<Stmt> {
        let (cond, _) = self.gen_expr(&Ty::Bool, 1);
        let then = self.gen_sub_block(depth + 1);
        let els = if self.rng.chance(0.5) {
            Some(self.gen_sub_block(depth + 1))
        } else {
            None
        };
        Some(Stmt::If { cond, then, els })
    }

    fn gen_for(&mut self, depth: usize) -> Option<Stmt> {
        let start = self.rng.range_i64(0, 5);
        let span = self.rng.range_i64(0, LOOP_CAP);
        let end = start + span;
        let var = self.fresh_var();
        let mark = self.scope_mark();
        self.scope.push(VarInfo {
            name: var.clone(),
            ty: Ty::Int,
            bound: end as i128,
            len: None,
            keys: Vec::new(),
            reserved: true, // loop var: don't reassign
        });
        let prev_mult = self.loop_mult;
        self.loop_mult = self.loop_mult.saturating_mul((span.max(1)) as i128);
        let body = self.gen_block_inner(depth + 1);
        self.loop_mult = prev_mult;
        self.scope_reset(mark);
        Some(Stmt::ForRange {
            var,
            start: Expr::IntLit(start),
            end: Expr::IntLit(end),
            body,
        })
    }

    fn gen_while(&mut self, depth: usize) -> Option<Stmt> {
        // Counter-bounded while so it always terminates. The counter + loop are wrapped in an
        // `if true:` block so the counter is scoped exactly to the loop in BOTH the generated
        // scope and the emitted source (no leak to later statements).
        let mark = self.scope_mark();
        let counter = self.fresh_var();
        let cap = self.rng.range_i64(0, LOOP_CAP);
        self.scope.push(VarInfo {
            name: counter.clone(),
            ty: Ty::Int,
            bound: cap as i128,
            len: None,
            keys: Vec::new(),
            reserved: true,
        });
        let cond = Expr::Bin {
            op: BinOp::Lt,
            ty: Ty::Bool,
            l: Box::new(Expr::Var(counter.clone())),
            r: Box::new(Expr::IntLit(cap)),
        };
        let prev_mult = self.loop_mult;
        self.loop_mult = self.loop_mult.saturating_mul((cap.max(1)) as i128);
        let mut body = self.gen_block_inner(depth + 1);
        // guarantee progress
        body.push(Stmt::Assign {
            name: counter.clone(),
            op: AssignOp::Add,
            value: Expr::IntLit(1),
        });
        self.loop_mult = prev_mult;
        self.scope_reset(mark); // drop counter + body-local vars

        Some(Stmt::If {
            cond: Expr::BoolLit(true),
            then: vec![
                Stmt::Let {
                    name: counter,
                    ty: Ty::Int,
                    init: Expr::IntLit(0),
                },
                Stmt::While { cond, body },
            ],
            els: None,
        })
    }

    fn gen_sub_block(&mut self, depth: usize) -> Block {
        let mark = self.scope_mark();
        let b = self.gen_block_inner(depth);
        self.scope_reset(mark);
        b
    }

    fn gen_block_inner(&mut self, depth: usize) -> Block {
        let mut b = Vec::new();
        let n = 1 + self.rng.below(3) as usize; // 1..3 statements
        for _ in 0..n {
            if let Some(s) = self.gen_stmt(depth) {
                b.push(s);
            }
        }
        if b.is_empty() {
            b.push(Stmt::Eval(Expr::IntLit(0)));
        }
        b
    }

    // ---- initializers & expressions ------------------------------------------

    fn gen_init(&mut self, ty: &Ty) -> (Expr, i128, Option<usize>, Vec<Expr>) {
        match ty {
            Ty::List(elem) => {
                let n = 1 + self.rng.below(3) as usize; // 1..3 (empty `[]` defeats `:=` inference)
                let mut items = Vec::new();
                for _ in 0..n {
                    let (e, _) = self.gen_expr(elem, MAX_EXPR_DEPTH); // leaves only
                    items.push(e);
                }
                (
                    Expr::ListLit {
                        elem: (**elem).clone(),
                        items,
                    },
                    0,
                    Some(n),
                    Vec::new(),
                )
            }
            Ty::Map(k, v) => {
                let n = 1 + self.rng.below(3) as usize; // 1..3 (empty `{}` defeats `:=` inference)
                let mut entries = Vec::new();
                let mut keys = Vec::new();
                for i in 0..n {
                    // deterministic distinct keys
                    let key = match **k {
                        Ty::Int => Expr::IntLit(i as i64),
                        Ty::Str => Expr::StrLit(format!("k{i}")),
                        _ => Expr::IntLit(i as i64),
                    };
                    let (val, _) = self.gen_expr(v, MAX_EXPR_DEPTH);
                    keys.push(key.clone());
                    entries.push((key, val));
                }
                (
                    Expr::MapLit {
                        k: (**k).clone(),
                        v: (**v).clone(),
                        entries,
                    },
                    0,
                    Some(n),
                    keys,
                )
            }
            _ => {
                let (e, b) = self.gen_expr(ty, 1);
                (e, b, None, Vec::new())
            }
        }
    }

    /// Generate an expression of the given type. Returns (expr, int-bound). The bound is only
    /// meaningful for `Ty::Int`; it is `0` otherwise.
    fn gen_expr(&mut self, ty: &Ty, depth: usize) -> (Expr, i128) {
        match ty {
            Ty::Int => self.gen_int(depth),
            Ty::Bool => (self.gen_bool(depth), 0),
            Ty::Str => (self.gen_str(depth), 0),
            Ty::Float => (self.gen_float(depth), 0),
            Ty::List(_) | Ty::Map(_, _) => {
                // only reference an existing collection var of this exact type
                let vs = self.vars_of(ty);
                if vs.is_empty() {
                    let (e, _, _, _) = self.gen_init(ty);
                    (e, 0)
                } else {
                    let i = *self.rng.choice(&vs);
                    (Expr::Var(self.scope[i].name.clone()), 0)
                }
            }
        }
    }

    fn gen_int(&mut self, depth: usize) -> (Expr, i128) {
        let at_leaf = depth >= MAX_EXPR_DEPTH;
        // When building an in-loop accumulator delta, only loop-stable (reserved) int vars are
        // allowed as leaves — see the note in `gen_assign`.
        let int_vars: Vec<usize> = self
            .scope
            .iter()
            .enumerate()
            .filter(|(_, v)| v.ty == Ty::Int && (!self.in_loop_rhs || v.reserved))
            .map(|(i, _)| i)
            .collect();
        // leaf choices
        if at_leaf || self.rng.chance(0.45) {
            if !int_vars.is_empty() && self.rng.chance(0.5) {
                let i = *self.rng.choice(&int_vars);
                return (Expr::Var(self.scope[i].name.clone()), self.scope[i].bound);
            }
            let n = self.rng.range_i64(-LEAF_INT_MAX, LEAF_INT_MAX);
            return (Expr::IntLit(n), n.unsigned_abs() as i128);
        }

        // composite
        let mut ops: Vec<BinOp> = vec![BinOp::Add, BinOp::Sub, BinOp::Mul];
        if self.feat.div_mod {
            ops.push(BinOp::Div);
            ops.push(BinOp::Mod);
        }
        let op = *self.rng.choice(&ops);

        // length / call / index leaves
        if self.feat.collections
            && self.rng.chance(0.15)
            && let Some(e) = self.try_len()
        {
            return (e, LEAF_INT_MAX as i128); // len of small containers
        }
        if self.feat.functions
            && self.rng.chance(0.2)
            && let Some((e, b)) = self.try_call(&Ty::Int)
        {
            return (e, b);
        }
        if self.feat.collections
            && self.rng.chance(0.15)
            && let Some((e, b)) = self.try_index(&Ty::Int)
        {
            return (e, b);
        }

        let (l, bl) = self.gen_int(depth + 1);
        match op {
            BinOp::Add | BinOp::Sub => {
                let (r, br) = self.gen_int(depth + 1);
                let bound = bl.saturating_add(br);
                if bound > MAX_BOUND {
                    let n = self.rng.range_i64(-LEAF_INT_MAX, LEAF_INT_MAX);
                    return (Expr::IntLit(n), n.unsigned_abs() as i128);
                }
                (
                    Expr::Bin {
                        op,
                        ty: Ty::Int,
                        l: Box::new(l),
                        r: Box::new(r),
                    },
                    bound,
                )
            }
            BinOp::Mul => {
                let (r, br) = self.gen_int(depth + 1);
                let bound = bl.saturating_mul(br);
                if bound > MAX_BOUND {
                    let n = self.rng.range_i64(-LEAF_INT_MAX, LEAF_INT_MAX);
                    return (Expr::IntLit(n), n.unsigned_abs() as i128);
                }
                (
                    Expr::Bin {
                        op,
                        ty: Ty::Int,
                        l: Box::new(l),
                        r: Box::new(r),
                    },
                    bound,
                )
            }
            BinOp::Div | BinOp::Mod => {
                // divisor must be non-zero: use a literal in [1, 1000] (sign randomized)
                let mag = self.rng.range_i64(1, 1000);
                let d = if self.rng.chance(0.5) { mag } else { -mag };
                let bound = if op == BinOp::Div {
                    bl // |a/d| <= |a|
                } else {
                    (mag as i128 - 1).max(0) // |a % d| < |d|
                };
                (
                    Expr::Bin {
                        op,
                        ty: Ty::Int,
                        l: Box::new(l),
                        r: Box::new(Expr::IntLit(d)),
                    },
                    bound,
                )
            }
            _ => unreachable!(),
        }
    }

    fn gen_bool(&mut self, depth: usize) -> Expr {
        let at_leaf = depth >= MAX_EXPR_DEPTH;
        let bool_vars = self.vars_of(&Ty::Bool);
        if at_leaf || self.rng.chance(0.4) {
            if !bool_vars.is_empty() && self.rng.chance(0.4) {
                let i = *self.rng.choice(&bool_vars);
                return Expr::Var(self.scope[i].name.clone());
            }
            // comparison of two ints — the common, interesting case
            if self.rng.chance(0.6) {
                let (l, _) = self.gen_int(depth + 1);
                let (r, _) = self.gen_int(depth + 1);
                let op = *self.rng.choice(&[
                    BinOp::Lt,
                    BinOp::Le,
                    BinOp::Gt,
                    BinOp::Ge,
                    BinOp::Eq,
                    BinOp::Ne,
                ]);
                return Expr::Bin {
                    op,
                    ty: Ty::Bool,
                    l: Box::new(l),
                    r: Box::new(r),
                };
            }
            return Expr::BoolLit(self.rng.chance(0.5));
        }
        // composite bool
        if self.rng.chance(0.3) {
            let e = self.gen_bool(depth + 1);
            return Expr::Unary {
                op: UnOp::Not,
                ty: Ty::Bool,
                e: Box::new(e),
            };
        }
        let l = self.gen_bool(depth + 1);
        let r = self.gen_bool(depth + 1);
        let op = if self.rng.chance(0.5) {
            BinOp::And
        } else {
            BinOp::Or
        };
        Expr::Bin {
            op,
            ty: Ty::Bool,
            l: Box::new(l),
            r: Box::new(r),
        }
    }

    fn gen_str(&mut self, depth: usize) -> Expr {
        let at_leaf = depth >= MAX_EXPR_DEPTH;
        let str_vars = self.vars_of(&Ty::Str);
        if at_leaf || self.rng.chance(0.6) {
            if !str_vars.is_empty() && self.rng.chance(0.4) {
                let i = *self.rng.choice(&str_vars);
                return Expr::Var(self.scope[i].name.clone());
            }
            return Expr::StrLit(self.rand_str());
        }
        // concat
        let l = self.gen_str(depth + 1);
        let r = self.gen_str(depth + 1);
        Expr::Bin {
            op: BinOp::Concat,
            ty: Ty::Str,
            l: Box::new(l),
            r: Box::new(r),
        }
    }

    fn gen_float(&mut self, _depth: usize) -> Expr {
        // restricted to short exact-ish decimals (n/8) to dodge the formatting crossover
        let n = self.rng.range_i64(-80, 80);
        Expr::FloatLit(n as f64 / 8.0)
    }

    // ---- expression sub-builders ---------------------------------------------

    fn try_len(&mut self) -> Option<Expr> {
        let cands: Vec<usize> = self
            .scope
            .iter()
            .enumerate()
            .filter(|(_, v)| matches!(v.ty, Ty::List(_) | Ty::Map(_, _)))
            .map(|(i, _)| i)
            .collect();
        if cands.is_empty() {
            return None;
        }
        let i = *self.rng.choice(&cands);
        Some(Expr::Len(Box::new(Expr::Var(self.scope[i].name.clone()))))
    }

    fn try_index(&mut self, want: &Ty) -> Option<(Expr, i128)> {
        // Index into a List[want] (by in-range integer) or a Map[_, want] (by a known key).
        let list_cands: Vec<usize> = self
            .scope
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                matches!(&v.ty, Ty::List(e) if e.as_ref() == want)
                    && v.len.map(|l| l > 0).unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect();
        let map_cands: Vec<usize> = self
            .scope
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                matches!(&v.ty, Ty::Map(_, val) if val.as_ref() == want) && !v.keys.is_empty()
            })
            .map(|(i, _)| i)
            .collect();

        let bound = if *want == Ty::Int { MAX_BOUND } else { 0 };
        let use_map = !map_cands.is_empty() && (list_cands.is_empty() || self.rng.chance(0.5));

        if use_map {
            let i = *self.rng.choice(&map_cands);
            let kidx = self.rng.pick(self.scope[i].keys.len());
            let key = self.scope[i].keys[kidx].clone();
            return Some((
                Expr::Index {
                    ret: want.clone(),
                    base: Box::new(Expr::Var(self.scope[i].name.clone())),
                    idx: Box::new(key),
                },
                bound,
            ));
        }
        if list_cands.is_empty() {
            return None;
        }
        let i = *self.rng.choice(&list_cands);
        let len = self.scope[i].len.unwrap();
        let idx = self.rng.below(len as u64) as i64;
        Some((
            Expr::Index {
                ret: want.clone(),
                base: Box::new(Expr::Var(self.scope[i].name.clone())),
                idx: Box::new(Expr::IntLit(idx)),
            },
            bound,
        ))
    }

    fn try_call(&mut self, want: &Ty) -> Option<(Expr, i128)> {
        let cands: Vec<usize> = self
            .sigs
            .iter()
            .enumerate()
            .filter(|(_, s)| &s.ret == want)
            .map(|(i, _)| i)
            .collect();
        if cands.is_empty() {
            return None;
        }
        let si = *self.rng.choice(&cands);
        let params = self.sigs[si].params.clone();
        let mut args = Vec::new();
        for pty in &params {
            // The callee's body + `ret_bound` were generated assuming `|int param| <=
            // PARAM_BOUND`. An int arg must therefore honor that bound — a large-bounded var
            // here would overflow inside the body (e.g. `p*p`), faulting in Chezzi but not in
            // Python = a false finding. Pass int args as small literals (<= LEAF_INT_MAX <=
            // PARAM_BOUND); other scalar args can't overflow.
            let a = if *pty == Ty::Int {
                Expr::IntLit(self.rng.range_i64(-LEAF_INT_MAX, LEAF_INT_MAX))
            } else {
                self.gen_expr(pty, MAX_EXPR_DEPTH).0
            };
            args.push(a);
        }
        let name = self.sigs[si].name.clone();
        let ret_bound = self.sigs[si].ret_bound;
        Some((
            Expr::Call {
                name,
                ret: want.clone(),
                args,
            },
            ret_bound,
        ))
    }

    // ---- helpers --------------------------------------------------------------

    fn rand_scalar_ty(&mut self) -> Ty {
        let mut tys = vec![Ty::Int, Ty::Bool, Ty::Str];
        if self.feat.floats {
            tys.push(Ty::Float);
        }
        self.rng.choice(&tys).clone()
    }

    fn rand_ty(&mut self) -> Ty {
        if self.feat.collections && self.rng.chance(0.25) {
            let elem = self.rand_scalar_ty();
            if self.rng.chance(0.5) {
                Ty::List(Box::new(elem))
            } else {
                let v = self.rand_scalar_ty();
                let k = if self.rng.chance(0.5) {
                    Ty::Int
                } else {
                    Ty::Str
                };
                Ty::Map(Box::new(k), Box::new(v))
            }
        } else {
            self.rand_scalar_ty()
        }
    }

    fn rand_str(&mut self) -> String {
        // safe alphabet: no quotes, backslash, or braces (braces would trigger Chezzi interpolation)
        const ALPHA: &[u8] =
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,:;-_";
        let n = self.rng.below(8) as usize;
        let mut s = String::new();
        for _ in 0..n {
            let c = ALPHA[self.rng.pick(ALPHA.len())] as char;
            s.push(c);
        }
        s
    }
}
