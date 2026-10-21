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
    pub(crate) impl_modules: Vec<String>,
    pub fns: HashMap<String, FnDef>,
    pub consts: HashMap<String, ConstDef>,
    pub aliases: HashMap<String, TypeAlias>,
    /// Modules occupy the same type namespace as aliases.
    modules: std::collections::HashSet<String>,
    pub(crate) resolved_aliases: HashMap<String, std::rc::Rc<crate::type_alias::ResolvedAlias>>,
    /// `use` declarations, applied after the whole table is collected.
    pub pending_uses: Vec<crate::ast::UsePath>,
    pub(crate) pending_scoped_uses: Vec<(String, crate::ast::UsePath)>,
    pub(crate) scoped_use_aliases: HashMap<String, HashMap<String, Vec<String>>>,
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
        // Qualified paths must name their actual declaration.
        if !path.is_single() { return None; }
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
        for (module, u) in std::mem::take(&mut self.pending_scoped_uses) {
            let glob = u.segments.last().is_some_and(|part| part == "*");
            let parts = if glob { &u.segments[..u.segments.len()-1] } else { &u.segments[..] };
            let written = parts.join("::");
            let exists = |name: &str| self.by_path.contains_key(name) || self.by_path.keys().any(|key| key.starts_with(&format!("{name}::")));
            let mut full = written.clone();
            if !exists(&full) {
                let mut scope = module.as_str();
                while !scope.is_empty() {
                    let candidate = format!("{scope}::{written}");
                    if exists(&candidate) { full = candidate; break; }
                    scope = scope.rsplit_once("::").map_or("", |(parent,_)|parent);
                }
            }
            let members: Vec<(String,String)> = if glob {
                let prefix = format!("{full}::");
                self.by_path.keys().filter_map(|key|key.strip_prefix(&prefix).filter(|tail|!tail.contains("::")).map(|tail|(tail.to_string(),key.clone()))).collect()
            } else { vec![(parts.last().cloned().unwrap_or_default(),full)] };
            for (short, full) in members {
                let targets = self.scoped_use_aliases.entry(module.clone()).or_default().entry(short).or_default();
                if !targets.contains(&full) { targets.push(full); targets.sort(); }
            }
        }
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
    synthesize_trait_objects(&mut globals);
    globals.apply_uses();
    globals.resolved_aliases = crate::type_alias::resolve_aliases(&globals).map_err(|e| ResolveError { msg: e.msg, span: e.span })?;
    crate::type_alias::qualify_types(&mut globals)?;
    validate_types(&globals)?;
    let trait_order = validate_supertrait_hierarchy(&globals)?;
    validate_impl_parameters(&globals)?;
    validate_methods(&globals)?;
    validate_trait_impls(&globals)?;
    crate::trait_solver::validate_impl_obligations(&globals, &trait_order)
        .map_err(|error| ResolveError { msg: error.msg, span: error.span })?;
    crate::trait_solver::validate_alias_obligations(&globals)
        .map_err(|error| ResolveError { msg: error.msg, span: error.span })?;
    validate_visibility(&module.items, &mut Vec::new(), &globals)?;
    Ok(Resolved { module, globals })
}

/// The type a `dyn Trait` names: a hidden struct `dyn#Trait` beside the trait,
/// holding one closure per object-safe method (the trait's and its
/// supertraits'). Coercing a value to `dyn Trait` builds the closures over it.
/// A method is object-safe when it takes `self`, has no type parameters of
/// its own, and mentions `Self` only through associated types.
///
/// The trait's associated types (`Item` of `dyn Iterator<Item = i64>`) are
/// extra type parameters of the object type, in name order after the trait's
/// own. When every required method of the trait and its supertraits is
/// object-safe, the object type also gets an ordinary `impl` of each of them
/// whose methods call the closures, so default methods (`map`, `collect`,
/// `to_string`, ...) work on trait objects too.
fn synthesize_trait_objects(g: &mut GlobalTable) {
    fn find_trait<'a>(g: &'a GlobalTable, module: &str, path: &Path) -> Option<(String, &'a TraitDef)> {
        let written = path.segments.join("::");
        let candidates = [if module.is_empty() { written.clone() } else { format!("{module}::{written}") }, written.clone()];
        for c in candidates {
            if let Some(t) = g.traits.get(&c) { return Some((c, t)); }
        }
        let mut found = g.traits.iter().filter(|(k, _)| k.rsplit("::").next() == Some(path.last()));
        let first = found.next()?;
        found.next().is_none().then(|| (first.0.clone(), first.1))
    }
    /// `t` with `Self::X` replaced by the object's parameter for `X`; `None`
    /// if `t` mentions `Self` any other way.
    fn object_type(t: &Type, assocs: &[String]) -> Option<Type> {
        let span = t.span;
        let kind = match &t.kind {
            TypeKind::SelfType => return None,
            TypeKind::Path(p, args) if p.segments.len() == 2 && p.segments[0] == "Self" && args.is_empty() => {
                if !assocs.contains(&p.segments[1]) { return None; }
                TypeKind::Path(Path::single(format!("assoc#{}", p.segments[1]), span), vec![])
            }
            TypeKind::Path(p, _) if p.is_single() && p.last() == "Self" => return None,
            TypeKind::Path(p, args) => TypeKind::Path(p.clone(), args.iter().map(|a| object_type(a, assocs)).collect::<Option<_>>()?),
            TypeKind::Tuple(ts) => TypeKind::Tuple(ts.iter().map(|a| object_type(a, assocs)).collect::<Option<_>>()?),
            TypeKind::Array(e, n) => TypeKind::Array(Box::new(object_type(e, assocs)?), n.clone()),
            TypeKind::Fn(ps, r) => TypeKind::Fn(ps.iter().map(|a| object_type(a, assocs)).collect::<Option<_>>()?, match r { Some(r) => Some(Box::new(object_type(r, assocs)?)), None => None }),
            TypeKind::ExternFn(ps, r) => TypeKind::ExternFn(ps.iter().map(|a| object_type(a, assocs)).collect::<Option<_>>()?, match r { Some(r) => Some(Box::new(object_type(r, assocs)?)), None => None }),
        };
        Some(Type { kind, span })
    }
    struct Method { name: String, params: Vec<Param>, ret: Option<Type>, self_is_mut: bool, span: Span }
    let mut structs = Vec::new();
    let mut impls = Vec::new();
    for (path, def) in &g.traits {
        let module = path.rsplit_once("::").map(|(m, _)| m.to_string()).unwrap_or_default();
        // The trait and its supertraits, with the arguments each is used at.
        let mut hierarchy: Vec<(String, &TraitDef, Vec<Type>)> = Vec::new();
        let own_args: Vec<Type> = def.generics.params.iter()
            .map(|p| Type { kind: TypeKind::Path(Path::single(p.name.clone(), p.span), vec![]), span: p.span }).collect();
        let mut pending = vec![(path.clone(), def, own_args)];
        while let Some((tpath, t, args)) = pending.pop() {
            if hierarchy.iter().any(|(p, _, _)| *p == tpath) { continue; }
            let tmodule = tpath.rsplit_once("::").map(|(m, _)| m.to_string()).unwrap_or_default();
            for sup in &t.supertraits {
                if let Some((sp, sd)) = find_trait(g, &tmodule, &sup.path) { pending.push((sp, sd, sup.args.clone())); }
            }
            hierarchy.push((tpath, t, args));
        }
        let mut assocs: Vec<String> = hierarchy.iter()
            .flat_map(|(_, t, _)| t.items.iter().filter_map(|i| match i { TraitItem::AssocType(n) => Some(n.clone()), _ => None }))
            .collect();
        assocs.sort();
        assocs.dedup();
        let mut fields: Vec<FieldDef> = Vec::new();
        let mut implementable = true;
        let mut per_trait: Vec<Vec<Method>> = Vec::new();
        for (_, t, _) in &hierarchy {
            let mut methods = Vec::new();
            for item in &t.items {
                let (name, generics, params, has_self, self_is_mut, ret, span, required) = match item {
                    TraitItem::Required(f) => (&f.name, &f.generics, &f.params, f.has_self, f.self_is_mut, &f.ret, f.span, true),
                    TraitItem::Provided(f) => (&f.name, &f.generics, &f.params, f.has_self, f.self_is_mut, &f.ret, f.span, false),
                    // A trait object has no value for a required constant.
                    TraitItem::Const(c) => {
                        if matches!(&*c.value.kind, ExprKind::Path(p) if p.last() == "const#required") { implementable = false; }
                        continue;
                    }
                    TraitItem::AssocType(_) => continue,
                };
                let converted = (has_self && generics.params.is_empty()).then(|| {
                    let ps: Option<Vec<Param>> = params.iter().map(|p| Some(Param { ty: object_type(&p.ty, &assocs)?, ..p.clone() })).collect();
                    let r = match ret { Some(r) => Some(Some(object_type(r, &assocs)?)), None => Some(None) };
                    Some((ps?, r?))
                }).flatten();
                let Some((ps, r)) = converted else {
                    if required { implementable = false; }
                    continue;
                };
                // A name another trait in the hierarchy already uses has no
                // closure of its own.
                if fields.iter().any(|f| &f.name == name) {
                    if required { implementable = false; }
                    continue;
                }
                let ty = Type { kind: TypeKind::Fn(ps.iter().map(|p| p.ty.clone()).collect(), r.clone().map(Box::new)), span };
                fields.push(FieldDef { vis: true, name: name.clone(), ty, span });
                methods.push(Method { name: name.clone(), params: ps, ret: r, self_is_mut, span });
            }
            per_trait.push(methods);
        }
        let name = format!("dyn#{}", def.name);
        let struct_path = if module.is_empty() { name.clone() } else { format!("{module}::{name}") };
        let mut params = def.generics.params.clone();
        params.extend(assocs.iter().map(|a| TypeParam { name: format!("assoc#{a}"), bounds: vec![], default: None, span: def.span }));
        let self_ty = Type {
            kind: TypeKind::Path(Path::single(name.clone(), def.span), params.iter()
                .map(|p| Type { kind: TypeKind::Path(Path::single(p.name.clone(), def.span), vec![]), span: def.span }).collect()),
            span: def.span,
        };
        let has_methods = per_trait.iter().any(|m| !m.is_empty());
        if implementable && has_methods {
            for ((tpath, t, args), methods) in hierarchy.iter().zip(per_trait) {
                let span = def.span;
                let ex = |kind: ExprKind| Expr { kind: std::sync::Arc::new(kind), span };
                let items = methods.into_iter().map(|m| {
                    let callee = ex(ExprKind::Field { base: ex(ExprKind::Path(Path::single("self".into(), span))), field: FieldAccess::Named(m.name.clone()) });
                    let args = m.params.iter().map(|p| ex(ExprKind::Path(Path::single(p.name.clone(), span)))).collect();
                    FnDef {
                        vis: false, name: m.name, generics: Generics::default(), params: m.params,
                        has_self: true, self_is_mut: m.self_is_mut, ret: m.ret,
                        body: Block { stmts: vec![], tail: Some(ex(ExprKind::Call(callee, args))), span },
                        is_extern: false, span: m.span,
                    }
                }).collect();
                let assoc_types = t.items.iter().filter_map(|i| match i {
                    TraitItem::AssocType(n) => Some((n.clone(), Type { kind: TypeKind::Path(Path::single(format!("assoc#{n}"), span), vec![]), span })),
                    _ => None,
                }).collect();
                impls.push((module.clone(), ImplBlock {
                    generics: Generics { params: params.clone(), where_clauses: vec![] },
                    trait_ref: Some(TraitRef { path: Path { segments: tpath.split("::").map(str::to_string).collect(), span }, args: args.clone(), bindings: vec![], span }),
                    self_ty: self_ty.clone(),
                    items,
                    assoc_types,
                    consts: vec![],
                    span,
                }));
            }
        }
        structs.push((struct_path, module, StructDef {
            vis: def.vis,
            is_value: false,
            name,
            generics: Generics { params, where_clauses: vec![] },
            body: StructBody::Named(fields),
            span: def.span,
        }));
    }
    for (path, module, def) in structs {
        g.insert(path.clone(), Symbol { kind: SymKind::Struct, span: def.span, vis: def.vis, module });
        g.structs.insert(path, def);
    }
    for (module, imp) in impls {
        g.impls.push(imp);
        g.impl_modules.push(module);
    }
}

/// Supertrait inheritance must be acyclic even when no impl or call uses it.
/// Work on canonical trait identities and walk iteratively for long chains.
fn validate_supertrait_hierarchy(g: &GlobalTable) -> Result<Vec<String>, ResolveError> {
    use std::collections::{BTreeSet, HashMap};
    let mut dependencies = HashMap::new();
    let mut users: HashMap<String, Vec<String>> = HashMap::new();
    let mut remaining = HashMap::new();
    let mut ready = BTreeSet::new();
    for (name, definition) in &g.traits {
        let parents: BTreeSet<String> = definition.supertraits.iter()
            .map(|parent| parent.path.segments.join("::")).collect();
        remaining.insert(name.clone(), parents.len());
        if parents.is_empty() { ready.insert(name.clone()); }
        for parent in &parents { users.entry(parent.clone()).or_default().push(name.clone()); }
        dependencies.insert(name.clone(), parents);
    }
    let mut order = Vec::new();
    while let Some(name) = ready.pop_first() {
        order.push(name.clone());
        if let Some(children) = users.get(&name) {
            for child in children {
                let count = remaining.get_mut(child).expect("collected trait");
                *count -= 1;
                if *count == 0 { ready.insert(child.clone()); }
            }
        }
    }
    let Some(mut current) = remaining.iter().filter(|(_, count)| **count > 0)
        .map(|(name, _)| name.clone()).min() else { return Ok(order) };
    let mut positions = HashMap::new();
    let mut path = Vec::new();
    loop {
        if let Some(&position) = positions.get(&current) {
            let mut cycle = path[position..].to_vec();
            cycle.push(current.clone());
            return Err(ResolveError {
                msg: format!("cyclic supertraits: {}", cycle.join(" -> ")),
                span: g.traits[&current].span,
            });
        }
        positions.insert(current.clone(), path.len());
        path.push(current.clone());
        current = dependencies[&current].iter().find(|parent| remaining[*parent] > 0)
            .expect("an unfinished trait depends on an unfinished parent").clone();
    }
}

/// An impl parameter must be recoverable from its normalized receiver or
/// trait arguments. A where condition alone does not bind an unknown type.
fn validate_impl_parameters(g: &GlobalTable) -> Result<(), ResolveError> {
    use crate::types::{Ty, lower_type_with};
    let env = crate::type_alias::AliasView { globals: g, resolved: &g.resolved_aliases, module: "" };
    for imp in &g.impls {
        let params = imp.generics.params.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        let mut roots = vec![lower_type_with(&imp.self_ty, &params, &env)
            .map_err(|e| ResolveError { msg: e.msg, span: e.span })?];
        if let Some(tr) = &imp.trait_ref {
            for arg in &tr.args {
                roots.push(lower_type_with(arg, &params, &env).map_err(|e| ResolveError { msg: e.msg, span: e.span })?);
            }
        }
        let mut used = std::collections::HashSet::new();
        let mut pending: Vec<&Ty> = roots.iter().collect();
        while let Some(ty) = pending.pop() {
            match ty {
                Ty::Var(name) => { used.insert(name.as_str()); }
                Ty::Named { args, .. } | Ty::Tuple(args) => pending.extend(args),
                Ty::Array(elem, _) => pending.push(elem),
                Ty::Fn { params, ret } | Ty::ExternFn { params, ret } => { pending.extend(params); pending.push(ret); }
                Ty::Prim(_) | Ty::Infer(_) => {}
            }
        }
        for parameter in &imp.generics.params {
            if !used.contains(parameter.name.as_str()) {
                return Err(ResolveError { msg: format!("unconstrained impl type parameter `{}`: it must occur in the receiver or trait arguments after alias expansion", parameter.name), span: parameter.span });
            }
        }
    }
    Ok(())
}

/// Reject two methods with the same name on the same receiver type, unless
/// they implement different traits (`Display::fmt` and `Debug::fmt`). Methods
/// are indexed by (receiver base name, [trait,] method name), so a second
/// definition would otherwise silently replace the first.
fn validate_methods(g: &GlobalTable) -> Result<(), ResolveError> {
    let mut seen = std::collections::HashSet::new();
    for imp in &g.impls {
        let base = crate::types::impl_receiver_base(imp, g).map_err(|error| ResolveError { msg: error.msg, span: error.span })?;
        // One impl per trait *instantiation*: `Add<f64>` and `Add<V>` differ.
        let tr = imp.trait_ref.as_ref().map(|t| format!("{}<{}>", t.path.segments.join("::"),
            t.args.iter().map(|a| type_text(a, &HashMap::new())).collect::<Vec<_>>().join(", ")));
        for m in &imp.items {
            if !seen.insert((base.clone(), tr.clone(), m.name.clone())) {
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
        let tname = tr.path.segments.join("::");
        let def = g.traits.get(&tname)
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
                // A required constant must be given; a defaulted one may be.
                TraitItem::Const(c) => {
                    let required = matches!(&*c.value.kind, ExprKind::Path(p) if p.last() == "const#required");
                    if required && !imp.consts.iter().any(|ic| ic.name == c.name) {
                        return Err(ResolveError { msg: format!("`impl {} for {}` is missing constant `{}`", tname, self_name, c.name), span: imp.span });
                    }
                    continue;
                }
                TraitItem::AssocType(name) => {
                    if !imp.assoc_types.iter().any(|(n, _)| n == name) {
                        return Err(ResolveError { msg: format!("`impl {} for {}` is missing associated type `{}`", tname, self_name, name), span: imp.span });
                    }
                    continue;
                }
            };
            match imp.items.iter().find(|m| m.name == sig.name) {
                Some(m) => check_impl_signature(&sig, m, &subst, &tname, g, imp, def)?,
                None if provided => {}
                None => {
                    return Err(ResolveError {
                        msg: format!("`impl {} for {}` is missing method `{}`", tname, self_name, sig.name),
                        span: imp.span,
                    });
                }
            }
        }
        for (name, _) in &imp.assoc_types {
            if !def.items.iter().any(|item| matches!(item, TraitItem::AssocType(n) if n == name)) {
                return Err(ResolveError { msg: format!("associated type `{}` is not a member of trait `{}`", name, tname), span: imp.span });
            }
        }
        for m in &imp.items {
            let declared = def.items.iter().any(|item| match item {
                TraitItem::Required(sig) => sig.name == m.name,
                TraitItem::Provided(f) => f.name == m.name,
                TraitItem::AssocType(_) | TraitItem::Const(_) => false,
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
fn check_impl_signature(sig: &FnSig, m: &FnDef, subst: &HashMap<String, Type>, tname: &str, g: &GlobalTable, imp: &ImplBlock, def: &TraitDef) -> Result<(), ResolveError> {
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
    // Normalize each side in its own binder scope, then substitute semantic
    // types once. Internal binder names cannot capture user generic names.
    let env = crate::type_alias::AliasView { globals: g, resolved: &g.resolved_aliases, module: "" };
    let lower = |ty: &Type, scope: &[String]| crate::types::lower_type_with(ty, scope, &env)
        .map_err(|error| ResolveError { msg: error.msg, span: error.span });
    let impl_scope: Vec<String> = imp.generics.params.iter().map(|p| p.name.clone()).collect();
    let mut actual_subst: HashMap<String, crate::types::Ty> = impl_scope.iter().enumerate()
        .map(|(i, name)| (name.clone(), crate::types::Ty::Var(format!("$impl{i}")))).collect();
    let self_ty = crate::types::subst_ty(&lower(&imp.self_ty, &impl_scope)?, &actual_subst);
    actual_subst.insert("Self".into(), self_ty.clone());
    let mut expected_subst = HashMap::new();
    let tr = imp.trait_ref.as_ref().expect("trait implementation");
    for (param, argument) in def.generics.params.iter().zip(&tr.args) {
        expected_subst.insert(param.name.clone(), crate::types::subst_ty(&lower(argument, &impl_scope)?, &actual_subst));
    }
    expected_subst.insert("Self".into(), self_ty);
    let mut expected_scope: Vec<String> = def.generics.params.iter().map(|p| p.name.clone()).collect();
    let mut actual_scope = impl_scope;
    for (i, (tp, mp)) in sig.generics.params.iter().zip(&m.generics.params).enumerate() {
        let binder = crate::types::Ty::Var(format!("$method{i}"));
        expected_scope.push(tp.name.clone());
        actual_scope.push(mp.name.clone());
        expected_subst.insert(tp.name.clone(), binder.clone());
        actual_subst.insert(mp.name.clone(), binder);
    }
    // `Self::Item` on either side is the impl's `type Item = ..`.
    let mut assoc = HashMap::new();
    for (name, ty) in &imp.assoc_types {
        assoc.insert(name.clone(), crate::types::subst_ty(&lower(ty, &actual_scope)?, &actual_subst));
    }
    let self_ty = expected_subst["Self"].clone();
    fn resolve_self(ty: crate::types::Ty, self_ty: &crate::types::Ty, assoc: &HashMap<String, crate::types::Ty>) -> crate::types::Ty {
        use crate::types::Ty;
        match ty {
            Ty::Named { name, args } => {
                if let (Some(n), Some(base)) = (name.strip_prefix("proj#"), args.first()) {
                    if base == self_ty { if let Some(t) = assoc.get(n) { return t.clone(); } }
                }
                Ty::Named { name, args: args.into_iter().map(|a| resolve_self(a, self_ty, assoc)).collect() }
            }
            Ty::Tuple(es) => Ty::Tuple(es.into_iter().map(|a| resolve_self(a, self_ty, assoc)).collect()),
            Ty::Array(e, n) => Ty::Array(Box::new(resolve_self(*e, self_ty, assoc)), n),
            Ty::Fn { params, ret } => Ty::Fn { params: params.into_iter().map(|a| resolve_self(a, self_ty, assoc)).collect(), ret: Box::new(resolve_self(*ret, self_ty, assoc)) },
            other => other,
        }
    }
    let same_type = |expected: &Type, actual: &Type| -> Result<bool, ResolveError> {
        Ok(resolve_self(crate::types::subst_ty(&lower(expected, &expected_scope)?, &expected_subst), &self_ty, &assoc)
            == resolve_self(crate::types::subst_ty(&lower(actual, &actual_scope)?, &actual_subst), &self_ty, &assoc))
    };
    for (i, (tp, mp)) in sig.params.iter().zip(&m.params).enumerate() {
        if !same_type(&tp.ty, &mp.ty)? {
            return fail(format!("parameter {} should be `{}`, found `{}`", i + 1, type_text(&tp.ty, &subst), type_text(&mp.ty, &HashMap::new())));
        }
    }
    let unit = Type { kind: TypeKind::Tuple(vec![]), span: m.span };
    let tret = sig.ret.clone().unwrap_or_else(|| unit.clone());
    let mret = m.ret.clone().unwrap_or(unit);
    if !same_type(&tret, &mret)? {
        return fail(format!("it should return `{}`, found `{}`", type_text(&tret, &subst), type_text(&mret, &HashMap::new())));
    }
    Ok(())
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
pub(crate) fn visible_from(g: &GlobalTable, target: &str, from_mod: &str) -> bool {
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
        Block(b) | Unsafe(b) => collect_paths_block(b, out),
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

/// Declaration member names must be unique before layout or construction can
/// address them by name. Report the second declaration at its own source span.
fn validate_field_names(fields: &[FieldDef]) -> Result<(), ResolveError> {
    let mut names = std::collections::HashSet::new();
    for field in fields {
        if !names.insert(field.name.as_str()) {
            return Err(ResolveError { msg: format!("duplicate field `{}`", field.name), span: field.span });
        }
    }
    Ok(())
}

/// Repeated wildcard parameters are permitted; named parameter bindings must
/// be unique within each callable.
pub(crate) fn validate_parameter_names<'a>(parameters: impl IntoIterator<Item = (&'a str, Span)>) -> Result<(), ResolveError> {
    let mut names = std::collections::HashSet::new();
    for (name, span) in parameters {
        if name != "_" && !names.insert(name) {
            return Err(ResolveError { msg: format!("duplicate parameter `{name}`"), span });
        }
    }
    Ok(())
}

fn collect_items(
    items: &[Item],
    prefix: &mut Vec<String>,
    g: &mut GlobalTable,
) -> Result<(), ResolveError> {
    let module = prefix.join("::");
    for item in items {
        let declared = match &item.kind {
            ItemKind::Fn(d) => Some((&d.name, d.span, false)),
            ItemKind::Struct(d) => Some((&d.name, d.span, false)),
            ItemKind::Enum(d) => Some((&d.name, d.span, false)),
            ItemKind::Trait(d) => Some((&d.name, d.span, false)),
            ItemKind::Const(d) => Some((&d.name, d.span, false)),
            ItemKind::TypeAlias(d) => Some((&d.name, d.span, true)),
            ItemKind::Mod(d) => Some((&d.name, d.span, false)),
            _ => None,
        };
        if let Some((name, span, alias)) = declared {
            let path = qualify(prefix,name);
            if (alias && (g.by_path.contains_key(&path) || g.modules.contains(&path))) || g.aliases.contains_key(&path) {
                return Err(ResolveError { msg: format!("duplicate declaration `{path}` conflicts with a type alias"), span });
            }
        }
        match &item.kind {
            ItemKind::Fn(f) => {
                validate_parameter_names(f.params.iter().map(|p| (p.name.as_str(), p.span)))?;
                let path = qualify(prefix, &f.name);
                g.insert(path.clone(), Symbol { kind: SymKind::Fn, span: f.span, vis: f.vis, module: module.clone() });
                g.fns.insert(path, f.clone());
            }
            ItemKind::Struct(s) => {
                if let StructBody::Named(fields) = &s.body { validate_field_names(fields)?; }
                let path = qualify(prefix, &s.name);
                if path == "Array" { return Err(ResolveError { msg: "cannot redefine builtin type `Array`".into(), span: s.span }); }
                g.insert(path.clone(), Symbol { kind: SymKind::Struct, span: s.span, vis: s.vis, module: module.clone() });
                g.structs.insert(path, s.clone());
            }
            ItemKind::Enum(e) => {
                let mut variants = std::collections::HashSet::new();
                for variant in &e.variants {
                    if !variants.insert(variant.name.as_str()) {
                        return Err(ResolveError { msg: format!("duplicate variant `{}`", variant.name), span: variant.span });
                    }
                    if let VariantPayload::Named(fields) = &variant.payload { validate_field_names(fields)?; }
                }
                let path = qualify(prefix, &e.name);
                if path == "Array" { return Err(ResolveError { msg: "cannot redefine builtin type `Array`".into(), span: e.span }); }
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
                for item in &t.items {
                    let parameters = match item {
                        TraitItem::Required(method) => &method.params,
                        TraitItem::Provided(method) => &method.params,
                        TraitItem::AssocType(_) | TraitItem::Const(_) => continue,
                    };
                    validate_parameter_names(parameters.iter().map(|p| (p.name.as_str(), p.span)))?;
                }
                let path = qualify(prefix, &t.name);
                g.insert(path.clone(), Symbol { kind: SymKind::Trait, span: t.span, vis: t.vis, module: module.clone() });
                g.traits.insert(path, t.clone());
            }
            ItemKind::Impl(b) => {
                for method in &b.items {
                    validate_parameter_names(method.params.iter().map(|p| (p.name.as_str(), p.span)))?;
                }
                g.impls.push(b.clone());
                g.impl_modules.push(module.clone());
            }
            ItemKind::Const(c) => {
                let path = qualify(prefix, &c.name);
                g.insert(path.clone(), Symbol { kind: SymKind::Const, span: c.span, vis: c.vis, module: module.clone() });
                g.consts.insert(path, c.clone());
            }
            ItemKind::TypeAlias(a) => {
                let path = qualify(prefix, &a.name);
                if path == "Array" { return Err(ResolveError { msg: "cannot redefine builtin type `Array`".into(), span: a.span }); }
                g.insert(path.clone(), Symbol { kind: SymKind::TypeAlias, span: a.span, vis: a.vis, module: module.clone() });
                g.aliases.insert(path, a.clone());
            }
            ItemKind::Mod(m) => {
                g.modules.insert(qualify(prefix, &m.name));
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
                g.pending_scoped_uses.push((module.clone(),u.clone()));
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
        // `impl Trait` results.
        TypeKind::Path(path, args) if path.last().starts_with("impl#") => {
            for a in args { check_type(a, generics, is_known)?; }
        }
        // `Self::Item` / `I::Item`: an associated type of a type parameter.
        TypeKind::Path(path, args) if path.segments.len() == 2 && args.is_empty()
            && (path.segments[0] == "Self" || generics.iter().any(|g| *g == path.segments[0])) => {}
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
        "i8" | "i16" | "i32" | "i64" | "usize" | "isize"
            | "u8" | "u16" | "u32" | "u64"
            | "f32" | "f64"
            | "bool" | "char" | "String" | "Bytes" | "RawPtr"
            // generic builtins resolved structurally later
            | "Vec" | "Array" | "Option" | "Result"
            // owning pointers: the type they point to (see `lower_type_with`)
            | "Box" | "Rc" | "Arc"
            // a hole in `collect::<Vec<_>>()`
            | "_"
            // explicit type arguments (parser::turbofish_marker)
            | "turbofish#" | "typeargs#"
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
