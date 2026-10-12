//! Name resolution: build the global item table and resolve paths.
//!
//! This is a light pass between parsing and type checking. It does NOT do type
//! inference. It:
//!   - collects every top-level item (functions, types, traits, impls, consts)
//!     into a flat, name-indexed table, handling nested `mod`s by path,
//!   - records the kind of each name (so the type checker / lowerer knows a
//!     `Path` expr is a fn vs a unit-variant vs a const vs a local),
//!   - validates that referenced type names exist,
//!   - leaves trait-method and generic-instantiation resolution to Phase 2
//!     (those need types).
//!
//! Local-variable resolution (let bindings, params, match binds, closure
//! params) is done here too, producing a `DefId`/`LocalId` for every binding so
//! later phases don't re-walk scopes.

use crate::ast::*;
use crate::lexer::Span;
use std::collections::HashMap;

#[derive(Debug)]
pub struct ResolveError {
    pub msg: String,
    pub span: Span,
}

/// A resolved program: the original module plus a global symbol table.
pub struct Resolved {
    pub module: Module,
    pub globals: GlobalTable,
}

/// What a top-level name refers to.
#[derive(Clone, Debug)]
pub enum SymKind {
    Fn,
    Struct,
    Enum,
    Trait,
    Const,
    TypeAlias,
    /// An enum variant: (enum name, variant index, has-payload).
    Variant { enum_name: String, index: u32, has_payload: bool },
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub kind: SymKind,
    pub span: Span,
    /// Was the item declared `pub`? Items at the crate root are always
    /// accessible; for items inside a `mod`, this gates cross-module access.
    pub vis: bool,
    /// The module path the item lives in (`""` for the crate root,
    /// `"geometry::shapes"` for a nested module). Used for visibility checks.
    pub module: String,
}

#[derive(Default)]
pub struct GlobalTable {
    /// Fully-qualified dotted name (`geometry::area`) → symbol. Also indexed by
    /// last segment for unqualified lookups where unambiguous.
    pub by_path: HashMap<String, Symbol>,
    /// last-segment → list of full paths (for unqualified resolution).
    pub by_last: HashMap<String, Vec<String>>,
    /// Struct/enum/trait definitions kept for layout + method resolution.
    pub structs: HashMap<String, StructDef>,
    pub enums: HashMap<String, EnumDef>,
    pub traits: HashMap<String, TraitDef>,
    pub impls: Vec<ImplBlock>,
    pub fns: HashMap<String, FnDef>,
    pub consts: HashMap<String, ConstDef>,
    pub aliases: HashMap<String, TypeAlias>,
    /// `use` declarations, applied after the whole table is collected.
    pub pending_uses: Vec<crate::ast::UsePath>,
    /// `use` aliases: a short name → the fully-qualified path it refers to.
    pub use_aliases: HashMap<String, String>,
}

impl GlobalTable {
    fn insert(&mut self, path: String, sym: Symbol) {
        let last = path.rsplit("::").next().unwrap().to_string();
        self.by_last.entry(last).or_default().push(path.clone());
        self.by_path.insert(path, sym);
    }

    /// Resolve a (possibly unqualified) path to a symbol.
    pub fn resolve(&self, path: &Path) -> Option<&Symbol> {
        let joined = path.segments.join("::");
        if let Some(s) = self.by_path.get(&joined) {
            return Some(s);
        }
        // A `use` alias for the unqualified name maps to a full path.
        if path.is_single() {
            if let Some(full) = self.use_aliases.get(path.last()) {
                if let Some(s) = self.by_path.get(full) {
                    return Some(s);
                }
            }
        }
        // Try last-segment if unambiguous.
        if let Some(paths) = self.by_last.get(path.last()) {
            if paths.len() == 1 {
                return self.by_path.get(&paths[0]);
            }
        }
        None
    }

    /// Apply collected `use` declarations, populating `use_aliases`. A
    /// `use a::b::c;` maps `c` → `a::b::c`; a `use a::b::*;` maps every item
    /// directly in module `a::b` by its last segment.
    fn apply_uses(&mut self) {
        let uses = std::mem::take(&mut self.pending_uses);
        for u in &uses {
            if u.segments.last().map(|s| s.as_str()) == Some("*") {
                // Glob: prefix is everything before the `*`.
                let prefix = u.segments[..u.segments.len() - 1].join("::");
                let pref_colon = format!("{}::", prefix);
                let members: Vec<(String, String)> = self
                    .by_path
                    .keys()
                    .filter(|p| p.starts_with(&pref_colon))
                    .filter_map(|p| {
                        let rest = &p[pref_colon.len()..];
                        // Only items DIRECTLY in the module (no further `::`).
                        if rest.contains("::") { None } else { Some((rest.to_string(), p.clone())) }
                    })
                    .collect();
                for (short, full) in members {
                    self.use_aliases.entry(short).or_insert(full);
                }
            } else {
                let full = u.segments.join("::");
                if let Some(last) = u.segments.last() {
                    self.use_aliases.entry(last.clone()).or_insert(full);
                }
            }
        }
    }
}

pub fn resolve_module(module: Module) -> Result<Resolved, ResolveError> {
    let mut globals = GlobalTable::default();
    collect_items(&module.items, &mut Vec::new(), &mut globals)?;
    globals.apply_uses();
    validate_types(&globals)?;
    validate_methods(&globals)?;
    validate_trait_impls(&globals)?;
    validate_visibility(&module.items, &mut Vec::new(), &globals)?;
    Ok(Resolved { module, globals })
}

/// Reject two methods with the same name on the same receiver type. Methods are
/// indexed by (receiver base name, method name), so a second definition would
/// otherwise silently replace the first.
fn validate_methods(g: &GlobalTable) -> Result<(), ResolveError> {
    let mut seen = std::collections::HashSet::new();
    for imp in &g.impls {
        let base = crate::types::type_base_name(&imp.self_ty);
        for m in &imp.items {
            if !seen.insert((base.clone(), m.name.clone())) {
                return Err(ResolveError {
                    msg: format!("duplicate method `{}` on `{}`: another impl block already defines it", m.name, base),
                    span: m.span,
                });
            }
        }
    }
    Ok(())
}

/// Check each `impl Trait for T` against the trait's declaration: the trait
/// must exist and take the given type arguments, every required method must
/// be implemented, no method may be outside the trait, and each method's
/// receiver, parameters, and return type must match the declaration with
/// `Self` and the trait's type parameters substituted.
fn validate_trait_impls(g: &GlobalTable) -> Result<(), ResolveError> {
    for imp in &g.impls {
        let Some(tr) = &imp.trait_ref else { continue };
        let tname = tr.path.last();
        let def = g.traits.iter().find(|(k, _)| k.rsplit("::").next() == Some(tname)).map(|(_, d)| d)
            .ok_or_else(|| ResolveError { msg: format!("unknown trait `{}`", tname), span: tr.span })?;
        if tr.args.len() != def.generics.params.len() {
            return Err(ResolveError {
                msg: format!("trait `{}` takes {} type argument(s), the impl gives {}", tname, def.generics.params.len(), tr.args.len()),
                span: tr.span,
            });
        }
        let mut subst: HashMap<String, Type> = def.generics.params.iter().map(|p| p.name.clone()).zip(tr.args.iter().cloned()).collect();
        subst.insert("Self".to_string(), imp.self_ty.clone());
        let self_name = crate::types::type_base_name(&imp.self_ty);
        for item in &def.items {
            let (sig, provided) = match item {
                TraitItem::Required(sig) => (sig.clone(), false),
                TraitItem::Provided(f) => (FnSig {
                    name: f.name.clone(),
                    generics: f.generics.clone(),
                    params: f.params.clone(),
                    has_self: f.has_self,
                    self_is_mut: f.self_is_mut,
                    ret: f.ret.clone(),
                    span: f.span,
                }, true),
                TraitItem::AssocType(name) => {
                    return Err(ResolveError { msg: format!("associated type `{}` in trait `{}` is not supported", name, tname), span: def.span });
                }
            };
            match imp.items.iter().find(|m| m.name == sig.name) {
                Some(m) => check_impl_signature(&sig, m, &subst, tname)?,
                None if provided => {}
                None => {
                    return Err(ResolveError {
                        msg: format!("`impl {} for {}` is missing method `{}`", tname, self_name, sig.name),
                        span: imp.span,
                    });
                }
            }
        }
        for m in &imp.items {
            let declared = def.items.iter().any(|item| match item {
                TraitItem::Required(sig) => sig.name == m.name,
                TraitItem::Provided(f) => f.name == m.name,
                TraitItem::AssocType(_) => false,
            });
            if !declared {
                return Err(ResolveError { msg: format!("method `{}` is not a member of trait `{}`", m.name, tname), span: m.span });
            }
        }
    }
    Ok(())
}

/// An impl method must have the trait method's receiver, parameter types, and
/// return type, after `subst` (Self and the trait's type parameters).
fn check_impl_signature(sig: &FnSig, m: &FnDef, subst: &HashMap<String, Type>, tname: &str) -> Result<(), ResolveError> {
    let fail = |what: String| Err(ResolveError {
        msg: format!("method `{}` does not match trait `{}`: {}", m.name, tname, what),
        span: m.span,
    });
    if sig.has_self != m.has_self {
        return fail(if sig.has_self { "the trait method takes `self`".into() } else { "the trait method takes no `self`".into() });
    }
    if sig.self_is_mut != m.self_is_mut {
        return fail(if sig.self_is_mut { "the trait method takes `mut self`".into() } else { "the trait method takes `self`, not `mut self`".into() });
    }
    if sig.params.len() != m.params.len() {
        return fail(format!("the trait method takes {} parameter(s), this one takes {}", sig.params.len(), m.params.len()));
    }
    if sig.generics.params.len() != m.generics.params.len() {
        return fail(format!("the trait method has {} type parameter(s), this one has {}", sig.generics.params.len(), m.generics.params.len()));
    }
    // The method's own type parameters correspond by position.
    let mut subst = subst.clone();
    for (tp, mp) in sig.generics.params.iter().zip(&m.generics.params) {
        subst.insert(tp.name.clone(), Type { kind: TypeKind::Path(Path::single(mp.name.clone(), mp.span), vec![]), span: mp.span });
    }
    let self_ty = subst.get("Self").cloned().expect("Self is substituted");
    for (i, (tp, mp)) in sig.params.iter().zip(&m.params).enumerate() {
        if !same_type(&tp.ty, &subst, &mp.ty, &self_ty) {
            return fail(format!("parameter {} should be `{}`, found `{}`", i + 1, type_text(&tp.ty, &subst), type_text(&mp.ty, &HashMap::new())));
        }
    }
    let unit = Type { kind: TypeKind::Tuple(vec![]), span: m.span };
    let tret = sig.ret.clone().unwrap_or_else(|| unit.clone());
    let mret = m.ret.clone().unwrap_or(unit);
    if !same_type(&tret, &subst, &mret, &self_ty) {
        return fail(format!("it should return `{}`, found `{}`", type_text(&tret, &subst), type_text(&mret, &HashMap::new())));
    }
    Ok(())
}

/// Is the trait-side type `t` (under `subst`) the impl-side type `u` (whose
/// `Self` is `self_ty`)? Named types compare by their last path segment.
fn same_type(t: &Type, subst: &HashMap<String, Type>, u: &Type, self_ty: &Type) -> bool {
    let empty = HashMap::new();
    match (&t.kind, &u.kind) {
        (TypeKind::SelfType, _) => match subst.get("Self") {
            Some(s) => same_type(s, &empty, u, self_ty),
            None => false,
        },
        (_, TypeKind::SelfType) => same_type(t, subst, self_ty, self_ty),
        (TypeKind::Path(p, args), _) if args.is_empty() && p.is_single() && subst.contains_key(p.last()) => {
            same_type(&subst[p.last()], &empty, u, self_ty)
        }
        (TypeKind::Path(p, pa), TypeKind::Path(q, qa)) => {
            p.last() == q.last() && pa.len() == qa.len() && pa.iter().zip(qa).all(|(a, b)| same_type(a, subst, b, self_ty))
        }
        (TypeKind::Tuple(a), TypeKind::Tuple(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_type(x, subst, y, self_ty)),
        (TypeKind::Array(a, _), TypeKind::Array(b, _)) => same_type(a, subst, b, self_ty),
        (TypeKind::Fn(ap, ar), TypeKind::Fn(bp, br)) | (TypeKind::ExternFn(ap, ar), TypeKind::ExternFn(bp, br)) => {
            let unit = Type { kind: TypeKind::Tuple(vec![]), span: t.span };
            ap.len() == bp.len()
                && ap.iter().zip(bp).all(|(x, y)| same_type(x, subst, y, self_ty))
                && same_type(ar.as_deref().unwrap_or(&unit), subst, br.as_deref().unwrap_or(&unit), self_ty)
        }
        _ => false,
    }
}

/// Render a surface type for a diagnostic, applying `subst`.
fn type_text(t: &Type, subst: &HashMap<String, Type>) -> String {
    let list = |ts: &[Type]| ts.iter().map(|x| type_text(x, subst)).collect::<Vec<_>>().join(", ");
    match &t.kind {
        TypeKind::SelfType => subst.get("Self").map(|s| type_text(s, &HashMap::new())).unwrap_or_else(|| "Self".into()),
        TypeKind::Path(p, args) if args.is_empty() && p.is_single() && subst.contains_key(p.last()) => type_text(&subst[p.last()], &HashMap::new()),
        TypeKind::Path(p, args) if args.is_empty() => p.last().to_string(),
        TypeKind::Path(p, args) => format!("{}<{}>", p.last(), list(args)),
        TypeKind::Tuple(ts) => format!("({})", list(ts)),
        TypeKind::Array(e, _) => format!("[{}; _]", type_text(e, subst)),
        TypeKind::Fn(ps, r) => format!("fn({}){}", list(ps), r.as_ref().map(|r| format!(" -> {}", type_text(r, subst))).unwrap_or_default()),
        TypeKind::ExternFn(ps, r) => format!("extern fn({}){}", list(ps), r.as_ref().map(|r| format!(" -> {}", type_text(r, subst))).unwrap_or_default()),
    }
}

/// Can code in module `from_mod` reference the symbol at fully-qualified path
/// `target`? Allowed when the target is `pub`, or lives in `from_mod` itself or
/// an ancestor module of `from_mod` (a child can see its parents' privates, and
/// every module can see the crate root). This mirrors Rust's rule closely
/// enough for v1.
fn visible_from(g: &GlobalTable, target: &str, from_mod: &str) -> bool {
    let Some(sym) = g.by_path.get(target) else { return true }; // unknown → let later passes report
    if sym.vis {
        return true;
    }
    let item_mod = &sym.module;
    // Same module, or item_mod is an ancestor of (a prefix of) from_mod.
    item_mod.is_empty()
        || from_mod == item_mod
        || from_mod.starts_with(&format!("{}::", item_mod))
}

/// Walk items, tracking the enclosing module, and reject explicitly
/// module-qualified references (`a::b::c`, ≥2 segments) to a private item that
/// isn't visible from the referencing module.
fn validate_visibility(
    items: &[Item],
    prefix: &mut Vec<String>,
    g: &GlobalTable,
) -> Result<(), ResolveError> {
    let cur_mod = prefix.join("::");
    for item in items {
        match &item.kind {
            ItemKind::Fn(f) => {
                let mut paths = Vec::new();
                collect_paths_block(&f.body, &mut paths);
                check_paths_vis(&paths, &cur_mod, g)?;
            }
            ItemKind::Mod(m) => {
                prefix.push(m.name.clone());
                validate_visibility(&m.items, prefix, g)?;
                prefix.pop();
            }
            ItemKind::Impl(b) => {
                for m in &b.items {
                    let mut paths = Vec::new();
                    collect_paths_block(&m.body, &mut paths);
                    check_paths_vis(&paths, &cur_mod, g)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reject any qualified (≥2 segment) reference targeting a private item not
/// visible from `cur_mod`.
fn check_paths_vis(paths: &[(Vec<String>, Span)], cur_mod: &str, g: &GlobalTable) -> Result<(), ResolveError> {
    for (segs, span) in paths {
        if segs.len() < 2 {
            continue;
        }
        let joined = segs.join("::");
        if g.by_path.contains_key(&joined) && !visible_from(g, &joined, cur_mod) {
            return Err(ResolveError {
                msg: format!("`{}` is private and not accessible from here (mark it `pub`)", joined),
                span: *span,
            });
        }
    }
    Ok(())
}

fn collect_paths_block(b: &Block, out: &mut Vec<(Vec<String>, Span)>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { init: Some(e), .. } => collect_paths_expr(e, out),
            Stmt::Let { .. } => {}
            Stmt::Expr(e) => collect_paths_expr(e, out),
            Stmt::Item(_) => {}
        }
    }
    if let Some(t) = &b.tail {
        collect_paths_expr(t, out);
    }
}

fn collect_paths_expr(e: &Expr, out: &mut Vec<(Vec<String>, Span)>) {
    use ExprKind::*;
    match &*e.kind {
        Path(p) => out.push((p.segments.clone(), e.span)),
        StructLit { path, fields, .. } => {
            out.push((path.segments.clone(), e.span));
            for f in fields { if let Some(v) = &f.value { collect_paths_expr(v, out); } }
        }
        Call(callee, args) => {
            collect_paths_expr(callee, out);
            for a in args { collect_paths_expr(a, out); }
        }
        MethodCall { recv, args, .. } => {
            collect_paths_expr(recv, out);
            for a in args { collect_paths_expr(a, out); }
        }
        Field { base, .. } => collect_paths_expr(base, out),
        Index { base, index } => { collect_paths_expr(base, out); collect_paths_expr(index, out); }
        Unary(_, x) => collect_paths_expr(x, out),
        Binary(_, l, r) => { collect_paths_expr(l, out); collect_paths_expr(r, out); }
        Assign { target, value, .. } => { collect_paths_expr(target, out); collect_paths_expr(value, out); }
        Cast(x, _) => collect_paths_expr(x, out),
        Tuple(xs) => for x in xs { collect_paths_expr(x, out); },
        Array(crate::ast::ArrayLit::Elems(xs)) => for x in xs { collect_paths_expr(x, out); },
        Array(crate::ast::ArrayLit::Repeat(v, n)) => { collect_paths_expr(v, out); collect_paths_expr(n, out); }
        Block(b) => collect_paths_block(b, out),
        If { cond, then_branch, else_branch } => {
            collect_paths_expr(cond, out);
            collect_paths_block(then_branch, out);
            if let Some(e) = else_branch { collect_paths_expr(e, out); }
        }
        Match { scrutinee, arms } => {
            collect_paths_expr(scrutinee, out);
            for arm in arms {
                if let Some(g) = &arm.guard { collect_paths_expr(g, out); }
                collect_paths_expr(&arm.body, out);
            }
        }
        While { cond, body } => { collect_paths_expr(cond, out); collect_paths_block(body, out); }
        Loop { body } => collect_paths_block(body, out),
        For { iter, body, .. } => { collect_paths_expr(iter, out); collect_paths_block(body, out); }
        Closure { body, .. } => collect_paths_expr(body, out),
        Return(Some(x)) | Break(Some(x)) | Try(x) => collect_paths_expr(x, out),
        Range { lo, hi, .. } => {
            if let Some(x) = lo { collect_paths_expr(x, out); }
            if let Some(x) = hi { collect_paths_expr(x, out); }
        }
        _ => {}
    }
}

fn collect_items(
    items: &[Item],
    prefix: &mut Vec<String>,
    g: &mut GlobalTable,
) -> Result<(), ResolveError> {
    let module = prefix.join("::");
    for item in items {
        match &item.kind {
            ItemKind::Fn(f) => {
                let path = qualify(prefix, &f.name);
                g.insert(path.clone(), Symbol { kind: SymKind::Fn, span: f.span, vis: f.vis, module: module.clone() });
                g.fns.insert(path, f.clone());
            }
            ItemKind::Struct(s) => {
                let path = qualify(prefix, &s.name);
                g.insert(path.clone(), Symbol { kind: SymKind::Struct, span: s.span, vis: s.vis, module: module.clone() });
                g.structs.insert(path, s.clone());
            }
            ItemKind::Enum(e) => {
                let path = qualify(prefix, &e.name);
                g.insert(path.clone(), Symbol { kind: SymKind::Enum, span: e.span, vis: e.vis, module: module.clone() });
                g.enums.insert(path.clone(), e.clone());
                // Register variants as `Enum::Variant`. A variant is as visible
                // as its enum.
                for (i, v) in e.variants.iter().enumerate() {
                    let vpath = format!("{}::{}", path, v.name);
                    let has_payload = !matches!(v.payload, VariantPayload::None);
                    g.insert(
                        vpath,
                        Symbol {
                            kind: SymKind::Variant {
                                enum_name: path.clone(),
                                index: i as u32,
                                has_payload,
                            },
                            span: v.span,
                            vis: e.vis,
                            module: module.clone(),
                        },
                    );
                }
            }
            ItemKind::Trait(t) => {
                let path = qualify(prefix, &t.name);
                g.insert(path.clone(), Symbol { kind: SymKind::Trait, span: t.span, vis: t.vis, module: module.clone() });
                g.traits.insert(path, t.clone());
            }
            ItemKind::Impl(b) => {
                g.impls.push(b.clone());
            }
            ItemKind::Const(c) => {
                let path = qualify(prefix, &c.name);
                g.insert(path.clone(), Symbol { kind: SymKind::Const, span: c.span, vis: c.vis, module: module.clone() });
                g.consts.insert(path, c.clone());
            }
            ItemKind::TypeAlias(a) => {
                let path = qualify(prefix, &a.name);
                g.insert(path.clone(), Symbol { kind: SymKind::TypeAlias, span: a.span, vis: a.vis, module: module.clone() });
                g.aliases.insert(path, a.clone());
            }
            ItemKind::Mod(m) => {
                prefix.push(m.name.clone());
                collect_items(&m.items, prefix, g)?;
                prefix.pop();
            }
            ItemKind::Use(u) => {
                // `use a::b::c;` brings `c` into scope as an alias for the full
                // path `a::b::c`. `use a::b::*;` is a glob: every item directly
                // in module `a::b` becomes accessible by its last segment.
                // We record aliases now and apply them after the full table is
                // built (so forward references to not-yet-collected items work).
                g.pending_uses.push(u.clone());
            }
        }
    }
    Ok(())
}

fn qualify(prefix: &[String], name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", prefix.join("::"), name)
    }
}

/// Shallow check that every named type referenced by a signature exists (as a
/// builtin, a declared struct/enum/alias, or a generic parameter in scope).
/// Deep checking is the type checker's job; this catches obvious typos early.
fn validate_types(g: &GlobalTable) -> Result<(), ResolveError> {
    // Build a set of all known type names.
    let mut known: Vec<String> = Vec::new();
    known.extend(g.structs.keys().cloned());
    known.extend(g.enums.keys().cloned());
    known.extend(g.aliases.keys().cloned());
    let is_known = |name: &str, generics: &[String]| -> bool {
        is_builtin_type(name)
            || generics.iter().any(|gp| gp == name)
            || name == "Self"
            || g.by_last.contains_key(name)
    };

    for f in g.fns.values() {
        let gp: Vec<String> = f.generics.params.iter().map(|p| p.name.clone()).collect();
        for p in &f.params {
            check_type(&p.ty, &gp, &is_known)?;
        }
        if let Some(r) = &f.ret {
            check_type(r, &gp, &is_known)?;
        }
    }
    for s in g.structs.values() {
        let gp: Vec<String> = s.generics.params.iter().map(|p| p.name.clone()).collect();
        if let StructBody::Named(fields) = &s.body {
            for f in fields {
                check_type(&f.ty, &gp, &is_known)?;
            }
        }
        if let StructBody::Tuple(tys) = &s.body {
            for t in tys {
                check_type(t, &gp, &is_known)?;
            }
        }
    }
    for e in g.enums.values() {
        let gp: Vec<String> = e.generics.params.iter().map(|p| p.name.clone()).collect();
        for v in &e.variants {
            match &v.payload {
                VariantPayload::Tuple(tys) => {
                    for t in tys { check_type(t, &gp, &is_known)?; }
                }
                VariantPayload::Named(fields) => {
                    for f in fields { check_type(&f.ty, &gp, &is_known)?; }
                }
                VariantPayload::None => {}
            }
        }
    }
    let _ = known;
    Ok(())
}

fn check_type(
    t: &Type,
    generics: &[String],
    is_known: &impl Fn(&str, &[String]) -> bool,
) -> Result<(), ResolveError> {
    match &t.kind {
        TypeKind::Path(path, args) => {
            let name = path.last();
            if !is_known(name, generics) {
                return Err(ResolveError {
                    msg: format!("unknown type `{}`", name),
                    span: t.span,
                });
            }
            for a in args {
                check_type(a, generics, is_known)?;
            }
        }
        TypeKind::Tuple(tys) => {
            for t in tys { check_type(t, generics, is_known)?; }
        }
        TypeKind::Array(elem, _) => check_type(elem, generics, is_known)?,
        TypeKind::Fn(params, ret) | TypeKind::ExternFn(params, ret) => {
            for p in params { check_type(p, generics, is_known)?; }
            if let Some(r) = ret { check_type(r, generics, is_known)?; }
        }
        TypeKind::SelfType => {}
    }
    Ok(())
}

pub fn is_builtin_type(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16" | "i32" | "i64"
            | "u8" | "u16" | "u32" | "u64"
            | "f32" | "f64"
            | "bool" | "char" | "String" | "Bytes" | "RawPtr"
            // generic builtins resolved structurally later
            | "Vec" | "Array" | "Option" | "Result"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse_module;

    fn resolve(src: &str) -> Result<Resolved, ResolveError> {
        let toks = lex(src).unwrap();
        let m = parse_module(&toks).unwrap();
        resolve_module(m)
    }

    #[test]
    fn collects_fns_and_types() {
        let r = resolve(
            "struct Point { x: i64, y: i64 } fn area(p: Point) -> i64 { p.x }",
        )
        .unwrap();
        assert!(r.globals.by_path.contains_key("Point"));
        assert!(matches!(
            r.globals.resolve(&path("area")).unwrap().kind,
            SymKind::Fn
        ));
    }

    #[test]
    fn enum_variants_registered() {
        let r = resolve("enum Color { Red, Green, Blue }").unwrap();
        let sym = r.globals.by_path.get("Color::Green").unwrap();
        assert!(matches!(sym.kind, SymKind::Variant { index: 1, .. }));
    }

    #[test]
    fn unknown_type_errors() {
        let e = resolve("fn f(x: Nope) -> i64 { 0 }");
        assert!(e.is_err());
    }

    #[test]
    fn generic_param_is_known() {
        let r = resolve("fn id<T>(x: T) -> T { x }");
        assert!(r.is_ok());
    }

    #[test]
    fn nested_mod_qualifies() {
        let r = resolve("mod geo { pub fn area() -> i64 { 0 } }").unwrap();
        assert!(r.globals.by_path.contains_key("geo::area"));
        // unqualified resolution works since it's unambiguous
        assert!(r.globals.resolve(&path("area")).is_some());
    }

    fn path(name: &str) -> Path {
        Path::single(name.to_string(), Span::new(0, 0))
    }
}
