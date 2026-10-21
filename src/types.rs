//! Semantic types and the type checker.
//!
//! gc-rust uses straightforward bidirectional checking with light unification
//! (enough for `let` inference, literal defaulting, and generic instantiation).
//! This module holds the semantic `Ty` representation and the `TyCtx` lookup
//! tables. The actual checking + monomorphization + lowering to core IR all
//! happen together in `src/lower.rs` (there is no separate `mono.rs`): each
//! reachable instantiation is specialized on demand as it is lowered.

use crate::ast::*;
use crate::lexer::{NumSuffix, Span};
use std::collections::HashMap;
use std::rc::Rc;

/// A semantic type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    /// A primitive scalar / unit / never.
    Prim(Prim),
    /// A user struct or enum applied to type args: `Vec<i64>`, `Point`,
    /// `Option<T>`. `name` is the fully-qualified def name.
    Named { name: String, args: Vec<Ty> },
    /// A generic parameter (de-name'd to its declared name in scope).
    Var(String),
    /// `fn(A, B) -> R` (also the type of closures, structurally).
    Fn { params: Vec<Ty>, ret: Box<Ty> },
    /// `extern fn(A, B) -> R` — a C callback function-pointer type. Represented
    /// at the machine level as a `RawPtr`; carries the signature so the call site
    /// can synthesize a correctly-typed trampoline. See `docs/ffi.md`.
    ExternFn { params: Vec<Ty>, ret: Box<Ty> },
    /// Tuple. Empty = unit (but unit is `Prim::Unit`).
    Tuple(Vec<Ty>),
    /// A fixed array `[T; N]`.
    Array(Box<Ty>, u64),
    /// An inference hole (only during checking).
    Infer(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Prim {
    I8, I16, I32, I64, U8, U16, U32, U64, F32, F64,
    Bool, Char, Str, Unit,
    /// An opaque, non-GC, pointer-sized value. Only meaningful at the FFI
    /// boundary: an `extern "C"` function may name `RawPtr` as a parameter or
    /// return type, and the `as_c_bytes` intrinsic produces one. It is not a
    /// managed reference — the GC never traces it. See `docs/ffi.md`.
    RawPtr,
}

impl Ty {
    pub fn unit() -> Ty { Ty::Prim(Prim::Unit) }
    pub fn bool() -> Ty { Ty::Prim(Prim::Bool) }
    pub fn i64() -> Ty { Ty::Prim(Prim::I64) }

    pub fn is_unit(&self) -> bool { matches!(self, Ty::Prim(Prim::Unit)) }
}

#[derive(Debug)]
pub struct TypeError {
    pub msg: String,
    pub span: Span,
}

type TResult<T> = Result<T, TypeError>;

/// Definitions the checker needs, indexed by fully-qualified name.
pub struct TyCtx {
    pub structs: HashMap<String, Rc<StructDef>>,
    pub enums: HashMap<String, Rc<EnumDef>>,
    pub fns: HashMap<String, Rc<FnDef>>,
    pub traits: HashMap<String, Rc<TraitDef>>,
    /// Trait impls, grouped for method lookup.
    pub impls: Vec<Rc<ImplBlock>>,
    pub(crate) trait_impls: HashMap<String, HashMap<String, Vec<usize>>>,
    /// Variant name → (enum fq-name, index, payload types as written).
    pub variants: HashMap<String, (String, u32)>,
    /// Method index: (receiver base type name, method name) → the impl method.
    /// Used to resolve `recv.method(..)` to a concrete function. Inherent and
    /// trait impls are both indexed here (trait method resolution is by name).
    pub methods: HashMap<(String, String), MethodEntry>,
    /// Trait methods by (receiver base, trait, method), including inherited
    /// defaults. `Display::fmt(x, f)` and operator desugaring look here, so
    /// two traits may define the same method name for one type.
    pub trait_methods: HashMap<(String, String, String), MethodEntry>,
    /// (receiver base, method) pairs defined by more than one trait (and no
    /// inherent impl): a plain `x.m()` call is ambiguous.
    pub ambiguous_methods: std::collections::HashSet<(String, String)>,
    /// `use` aliases: short name → fully-qualified path. Lets an unqualified call
    /// resolve to the specific item a `use` brought into scope (disambiguating
    /// same-named items across modules).
    pub use_aliases: HashMap<String, String>,
    pub(crate) aliases: HashMap<String, Rc<crate::type_alias::ResolvedAlias>>,
}

#[derive(Clone)]
pub struct MethodEntry {
    /// The impl's `Self` type as written (e.g. `Option<T>`), for binding `Self`.
    pub self_ty: Type,
    /// The impl block's own generic parameter names (`impl<T> ... for Foo<T>`).
    pub impl_generics: Vec<String>,
    /// Bounds and where predicates that make this impl applicable.
    pub impl_constraints: crate::ast::Generics,
    /// The trait this method implements (`impl Add for T` → `Some("Add")`),
    /// or `None` for an inherent impl. Operator and indexing desugaring only
    /// accept a method from the matching trait impl.
    pub trait_name: Option<String>,
    pub method: Rc<FnDef>,
}

/// Match generic binders consistently while preserving nominal identity and
/// every structural constraint. A repeated binder denotes the same type.
pub(crate) fn type_pattern_bindings(pattern: &Ty, actual: &Ty) -> Option<HashMap<String, Ty>> {
    fn matches(pattern: &Ty, actual: &Ty, bindings: &mut HashMap<String, Ty>) -> bool {
        match (pattern, actual) {
            (Ty::Var(name), _) => match bindings.get(name) {
                Some(bound) => bound == actual,
                None => { bindings.insert(name.clone(), actual.clone()); true }
            },
            (Ty::Named { name: pn, args: pa }, Ty::Named { name: an, args: aa }) =>
                pn == an && pa.len() == aa.len() && pa.iter().zip(aa).all(|(p, a)| matches(p, a, bindings)),
            (Ty::Tuple(pa), Ty::Tuple(aa)) => pa.len() == aa.len() && pa.iter().zip(aa).all(|(p, a)| matches(p, a, bindings)),
            (Ty::Array(p, pn), Ty::Array(a, an)) => pn == an && matches(p, a, bindings),
            (Ty::Fn { params: pp, ret: pr }, Ty::Fn { params: ap, ret: ar })
            | (Ty::ExternFn { params: pp, ret: pr }, Ty::ExternFn { params: ap, ret: ar }) =>
                pp.len() == ap.len() && pp.iter().zip(ap).all(|(p, a)| matches(p, a, bindings)) && matches(pr, ar, bindings),
            _ => pattern == actual,
        }
    }
    let mut bindings = HashMap::new();
    matches(pattern, actual, &mut bindings).then_some(bindings)
}

/// The canonical receiver identity used by both duplicate checking and lookup.
pub(crate) fn receiver_base(ty: &Ty) -> Option<String> {
    match ty {
        Ty::Named { name, .. } => Some(name.clone()),
        Ty::Prim(p) => Some(prim_name(*p).to_string()),
        _ => None,
    }
}

pub(crate) fn impl_receiver_base(imp: &crate::ast::ImplBlock, g: &crate::resolve::GlobalTable) -> TResult<String> {
    let env = crate::type_alias::AliasView { globals: g, resolved: &g.resolved_aliases, module: "" };
    let params = imp.generics.params.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
    let ty = lower_type_with(&imp.self_ty, &params, &env)?;
    receiver_base(&ty).ok_or_else(|| TypeError { msg: "impl receiver must be a nominal or primitive type".into(), span: imp.self_ty.span })
}

impl TyCtx {
    pub fn from_globals(g: &crate::resolve::GlobalTable) -> Self {
        let mut variants = HashMap::new();
        for (name, e) in &g.enums {
            for (i, v) in e.variants.iter().enumerate() {
                variants.insert(format!("{}::{}", name, v.name), (name.clone(), i as u32));
                // also bare variant name for unqualified `Some`/`None`/`Ok`/`Err`
                variants
                    .entry(v.name.clone())
                    .or_insert((name.clone(), i as u32));
            }
        }
        // Build the method index from every impl block. Inherent methods take
        // precedence over trait methods of the same name.
        let mut methods: HashMap<(String, String), MethodEntry> = HashMap::new();
        let mut trait_methods = HashMap::new();
        let mut inherent = std::collections::HashSet::new();
        let mut ambiguous_methods = std::collections::HashSet::new();
        let mut defaults = std::collections::HashSet::new();
        let mut add = |methods: &mut HashMap<(String, String), MethodEntry>, base: &str, entry: MethodEntry, is_default: bool| {
            let key = (base.to_string(), entry.method.name.clone());
            match &entry.trait_name {
                None => {
                    inherent.insert(key.clone());
                    ambiguous_methods.remove(&key);
                    methods.insert(key, entry);
                }
                Some(tr) => {
                    let tkey = (base.to_string(), tr.clone(), entry.method.name.clone());
                    if is_default && trait_methods.contains_key(&tkey) { return; }
                    trait_methods.insert(tkey, entry.clone());
                    // An explicitly implemented trait method beats another
                    // trait's default of the same name (so a type's
                    // `ToString::to_string` wins over `Display`'s default).
                    match methods.get(&key) {
                        None => {
                            if is_default { defaults.insert(key.clone()); }
                            methods.insert(key, entry);
                        }
                        Some(existing) => {
                            let explicit_vs_default = is_default != defaults.contains(&key);
                            if !inherent.contains(&key) && existing.trait_name.as_ref() != Some(tr) && !explicit_vs_default {
                                ambiguous_methods.insert(key);
                            }
                        }
                    }
                }
            }
        };
        for imp in g.impls.iter().filter(|imp| imp.trait_ref.is_none()) {
            let base = impl_receiver_base(imp, g).expect("validated impl receiver");
            let impl_generics: Vec<String> = imp.generics.params.iter().map(|p| p.name.clone()).collect();
            for m in &imp.items {
                add(&mut methods, &base, MethodEntry {
                    self_ty: imp.self_ty.clone(),
                    impl_generics: impl_generics.clone(),
                    impl_constraints: imp.generics.clone(),
                    trait_name: None,
                    method: Rc::new(m.clone()),
                }, false);
            }
        }
        for imp in g.impls.iter().filter(|imp| imp.trait_ref.is_some()) {
            let base = impl_receiver_base(imp, g).expect("validated impl receiver");
            let impl_generics: Vec<String> = imp.generics.params.iter().map(|p| p.name.clone()).collect();
            let tr = imp.trait_ref.as_ref().unwrap().path.segments.join("::");
            for m in &imp.items {
                add(&mut methods, &base, MethodEntry {
                    self_ty: imp.self_ty.clone(),
                    impl_generics: impl_generics.clone(),
                    impl_constraints: imp.generics.clone(),
                    trait_name: Some(tr.clone()),
                    method: Rc::new(m.clone()),
                }, false);
            }
        }
        // A trait's default methods belong to every implementing type that
        // does not define them, after all impl methods (so inherent and
        // overriding methods take precedence). Defaults inherited through a
        // supertrait chain are not copied: each trait contributes its own.
        for imp in &g.impls {
            let Some(tr) = &imp.trait_ref else { continue };
            let tname = tr.path.segments.join("::");
            let Some(def) = g.traits.get(&tname) else { continue };
            let base = impl_receiver_base(imp, g).expect("validated impl receiver");
            let impl_generics: Vec<String> = imp.generics.params.iter().map(|p| p.name.clone()).collect();
            for item in &def.items {
                let crate::ast::TraitItem::Provided(f) = item else { continue };
                add(&mut methods, &base, MethodEntry {
                    self_ty: imp.self_ty.clone(),
                    impl_generics: impl_generics.clone(),
                    impl_constraints: imp.generics.clone(),
                    trait_name: Some(tname.clone()),
                    method: Rc::new(f.clone()),
                }, true);
            }
        }
        let mut trait_impls: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        for (index, imp) in g.impls.iter().enumerate() {
            if let Some(tr) = &imp.trait_ref {
                let receiver = impl_receiver_base(imp, g).expect("validated impl receiver");
                trait_impls.entry(tr.path.segments.join("::")).or_default()
                    .entry(receiver).or_default().push(index);
            }
        }
        TyCtx {
            trait_impls,
            structs: g.structs.iter().map(|(k, v)| (k.clone(), Rc::new(v.clone()))).collect(),
            enums: g.enums.iter().map(|(k, v)| (k.clone(), Rc::new(v.clone()))).collect(),
            fns: g.fns.iter().map(|(k, v)| (k.clone(), Rc::new(v.clone()))).collect(),
            traits: g.traits.iter().map(|(k, v)| (k.clone(), Rc::new(v.clone()))).collect(),
            impls: g.impls.iter().map(|i| Rc::new(i.clone())).collect(),
            variants,
            methods,
            trait_methods,
            ambiguous_methods,
            use_aliases: g.use_aliases.clone(),
            aliases: g.resolved_aliases.clone(),
        }
    }

    /// Structural sharing includes explicit conditional Sync impls.
    pub fn is_sync(&self, ty: &Ty) -> bool {
        self.try_is_sync(ty).unwrap_or(false)
    }
    pub fn try_is_sync(&self, ty: &Ty) -> TResult<bool> {
        crate::trait_solver::prove(self, crate::trait_solver::Goal::Share(ty.clone()))
    }
    pub fn type_implements_trait(&self, ty: &Ty, name: &str) -> bool {
        self.type_implements_trait_args(ty, name, &[])
    }
    pub fn type_implements_trait_args(&self, ty: &Ty, name: &str, args: &[Ty]) -> bool {
        self.try_type_implements_trait_args(ty, name, args).unwrap_or(false)
    }
    pub fn try_type_implements_trait_args(&self, ty: &Ty, name: &str, args: &[Ty]) -> TResult<bool> {
        crate::trait_solver::prove(self, crate::trait_solver::Goal::Trait(ty.clone(), name.into(), args.to_vec()))
    }

    /// Resolve a variant through its enum's canonical path. A relative enum
    /// name may expand to its module, but an unknown qualifier is never dropped.
    pub fn variant_by_path(&self, path: &Path) -> Option<(String, u32)> {
        let written = path.segments.join("::");
        if let Some(found) = self.variants.get(&written) { return Some(found.clone()); }
        if path.segments.len() < 2 { return None; }
        let enum_path = path.segments[..path.segments.len() - 1].join("::");
        let canonical = self.canon(&enum_path)?;
        let canonical = match self.aliases.get(&canonical) {
            Some(alias) => match &alias.target {
                Ty::Named { name, .. } => name,
                _ => return None,
            },
            None => &canonical,
        };
        self.variants.get(&format!("{canonical}::{}", path.last())).cloned()
    }

    /// Resolve a fully-qualified or unqualified type/enum/struct name to its
    /// canonical fq-name.
    pub fn canon(&self, name: &str) -> Option<String> {
        if intrinsic_type_arity(name).is_some() { return None; }
        if self.structs.contains_key(name) || self.enums.contains_key(name) || self.aliases.contains_key(name) {
            return Some(name.to_string());
        }
        // A qualified path names one declaration; a matching final segment
        // elsewhere cannot repair an unknown module or qualifier.
        if name.contains("::") { return None; }
        if let Some(full) = self.use_aliases.get(name) {
            if self.structs.contains_key(full) || self.enums.contains_key(full) || self.aliases.contains_key(full) {
                return Some(full.clone());
            }
        }
        // The resolver permits an unqualified lookup only when unambiguous.
        // Hash-map iteration order must never choose the runtime layout.
        let mut matches = self.structs.keys().chain(self.enums.keys()).chain(self.aliases.keys())
            .filter(|k| k.rsplit("::").next() == Some(name));
        let first = matches.next()?;
        if matches.next().is_none() { return Some(first.clone()); }

        None
    }
}

/// The canonical source name of a primitive type (`i64`, `f64`, …).
pub fn prim_name(p: Prim) -> &'static str {
    match p {
        Prim::I8 => "i8", Prim::I16 => "i16", Prim::I32 => "i32", Prim::I64 => "i64",
        Prim::U8 => "u8", Prim::U16 => "u16", Prim::U32 => "u32", Prim::U64 => "u64",
        Prim::F32 => "f32", Prim::F64 => "f64",
        Prim::Bool => "bool", Prim::Char => "char", Prim::Str => "String", Prim::Unit => "()",
        Prim::RawPtr => "RawPtr",
    }
}

/// The base (head) type name of a surface type: `Vec<i64>` → `"Vec"`,
/// `i64` → `"i64"`. Used to key the method index by receiver type.
pub fn type_base_name(t: &Type) -> String {
    match &t.kind {
        TypeKind::Path(p, _) => p.last().to_string(),
        TypeKind::SelfType => "Self".to_string(),
        TypeKind::Tuple(_) => "(tuple)".to_string(),
        TypeKind::Array(..) => "(array)".to_string(),
        TypeKind::Fn(..) => "(fn)".to_string(),
        TypeKind::ExternFn(..) => "(extern fn)".to_string(),
    }
}

/// Convert a surface `Type` (with generic params in scope) to a semantic `Ty`.
/// Substitute type variables in `ty` per `subst` (used by `is_sync` to ground a
/// generic field type at a concrete instantiation).
pub(crate) fn subst_ty(ty: &Ty, subst: &std::collections::HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Var(v) => subst.get(v).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Named { name, args } => {
            let t = Ty::Named { name: name.clone(), args: args.iter().map(|a| subst_ty(a, subst)).collect() };
            normalize_projection(&t).unwrap_or(t)
        }
        Ty::Tuple(es) => Ty::Tuple(es.iter().map(|e| subst_ty(e, subst)).collect()),
        Ty::Array(e, n) => Ty::Array(Box::new(subst_ty(e, subst)), *n),
        Ty::Fn { params, ret } => Ty::Fn {
            params: params.iter().map(|p| subst_ty(p, subst)).collect(),
            ret: Box::new(subst_ty(ret, subst)),
        },
        Ty::ExternFn { params, ret } => Ty::ExternFn {
            params: params.iter().map(|p| subst_ty(p, subst)).collect(),
            ret: Box::new(subst_ty(ret, subst)),
        },
        Ty::Prim(_) | Ty::Infer(_) => ty.clone(),
    }
}

pub fn lower_type(t: &Type, generics: &[String], ctx: &TyCtx) -> TResult<Ty> {
    lower_type_with(t, generics, ctx)
}

pub(crate) trait TypeEnvironment {
    fn canonical(&self, name: &str) -> Option<String>;
    fn alias(&self, name: &str) -> Option<&crate::type_alias::ResolvedAlias>;
    fn declared_nominal(&self, name: &str) -> bool;
}
impl TypeEnvironment for TyCtx {
    fn canonical(&self, name: &str) -> Option<String> { self.canon(name) }
    fn alias(&self, name: &str) -> Option<&crate::type_alias::ResolvedAlias> { self.aliases.get(name).map(Rc::as_ref) }
    fn declared_nominal(&self, name: &str) -> bool { self.structs.contains_key(name) || self.enums.contains_key(name) }
}

pub(crate) fn lower_type_with(t: &Type, generics: &[String], ctx: &impl TypeEnvironment) -> TResult<Ty> {
    match &t.kind {
        // `Self::Item` / `I::Item`: an associated type of a type parameter,
        // resolved once the parameter is known (see `normalize_projection`).
        TypeKind::Path(path, args) if path.segments.len() == 2 && args.is_empty()
            && (path.segments[0] == "Self" || generics.iter().any(|g| *g == path.segments[0])) =>
        {
            Ok(projection(&path.segments[1], Ty::Var(path.segments[0].clone())))
        }
        TypeKind::Path(path, args) => {
            let name = path.last();
            if path.is_single() && generics.iter().any(|g| g == name) {
                if !args.is_empty() { return Err(TypeError { msg: format!("type parameter `{name}` takes no type arguments"), span: t.span }); }
                return Ok(Ty::Var(name.to_string()));
            }
            if path.is_single() && name == "_" && args.is_empty() {
                return Ok(Ty::Var("_".into()));
            }
            if let Some(p) = path.is_single().then(|| prim_of(name)).flatten() {
                if !args.is_empty() { return Err(TypeError { msg: format!("primitive `{name}` takes no type arguments"), span: t.span }); }
                return Ok(Ty::Prim(p));
            }
            let targs: Vec<Ty> = args.iter().map(|a| lower_type_with(a, generics, ctx)).collect::<Result<_, _>>()?;
            if path.is_single() && name == "Array" {
                if targs.len() != 1 { return Err(TypeError { msg: format!("type `Array` takes 1 type argument, got {}", targs.len()), span: t.span }); }
                return Ok(Ty::Named { name: "Array".into(), args: targs });
            }
            let qualified = path.segments.join("::");
            // Every heap value is already a shared GC reference, so the owning
            // pointers are the type they point to (unless the program declares
            // its own type of that name).
            if path.is_single() && matches!(name, "Box" | "Rc" | "Arc") && targs.len() == 1 && ctx.canonical(&qualified).is_none() {
                return Ok(targs.into_iter().next().unwrap());
            }
            let canon = ctx.canonical(&qualified).unwrap_or(qualified);
            if let Some(alias) = ctx.alias(&canon) {
                if targs.len() != alias.params.len() {
                    return Err(TypeError { msg: format!("type alias `{canon}` takes {} type argument(s), got {}", alias.params.len(), targs.len()), span: t.span });
                }
                let subst = alias.params.iter().cloned().zip(targs).collect();
                return Ok(subst_ty(&alias.target, &subst));
            }
            // Bare generic impl namespaces (e.g. impl Thread with static
            // methods) are distinct from instantiated runtime container types.
            // The structural fallback has no declaration to check elsewhere.
            let arity = if ctx.declared_nominal(&canon) { None }
                else { match canon.as_str() { "Vec" | "Array" | "Option" => Some(1), "Result" => Some(2), _ => None } };
            if let Some(arity) = arity {
                if args.len() != arity { return Err(TypeError { msg: format!("type `{canon}` takes {arity} type argument(s), got {}", args.len()), span: t.span }); }
            }
            Ok(Ty::Named { name: canon, args: targs })
        }
        TypeKind::Tuple(tys) => {
            if tys.is_empty() {
                Ok(Ty::Prim(Prim::Unit))
            } else {
                Ok(Ty::Tuple(tys.iter().map(|t| lower_type_with(t, generics, ctx)).collect::<Result<_, _>>()?))
            }
        }
        TypeKind::Array(elem, count) => {
            let e = lower_type_with(elem, generics, ctx)?;
            let n = const_usize(count)?;
            Ok(Ty::Array(Box::new(e), n))
        }
        TypeKind::Fn(params, ret) => {
            let ps = params.iter().map(|p| lower_type_with(p, generics, ctx)).collect::<Result<_, _>>()?;
            let r = match ret {
                Some(r) => lower_type_with(r, generics, ctx)?,
                None => Ty::Prim(Prim::Unit),
            };
            Ok(Ty::Fn { params: ps, ret: Box::new(r) })
        }
        TypeKind::ExternFn(params, ret) => {
            let ps = params.iter().map(|p| lower_type_with(p, generics, ctx)).collect::<Result<_, _>>()?;
            let r = match ret {
                Some(r) => lower_type_with(r, generics, ctx)?,
                None => Ty::Prim(Prim::Unit),
            };
            Ok(Ty::ExternFn { params: ps, ret: Box::new(r) })
        }
        TypeKind::SelfType => Ok(Ty::Var("Self".to_string())),
    }
}

/// The associated type `name` of `base` (`<base as Trait>::name`), as a
/// placeholder named type until `base` is known.
pub(crate) fn projection(name: &str, base: Ty) -> Ty {
    Ty::Named { name: format!("proj#{}", name), args: vec![base] }
}

thread_local! {
    /// The definitions used to resolve projections while a program is being
    /// checked and lowered. Substitution resolves `proj#Item<Counter>` to the
    /// `type Item = ..` of Counter's impl as soon as the base is ground.
    static NORMALIZER: std::cell::Cell<*const TyCtx> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// Resolve projections against `ctx` until the guard is dropped.
pub(crate) struct NormalizerGuard(*const TyCtx);
impl Drop for NormalizerGuard {
    fn drop(&mut self) { NORMALIZER.with(|n| n.set(self.0)); }
}
pub(crate) fn enter_normalizer(ctx: &TyCtx) -> NormalizerGuard {
    NormalizerGuard(NORMALIZER.with(|n| n.replace(ctx as *const TyCtx)))
}

/// If `ty` is a projection on a ground type, the associated type it names.
pub(crate) fn normalize_projection(ty: &Ty) -> Option<Ty> {
    let Ty::Named { name, args } = ty else { return None };
    let assoc = name.strip_prefix("proj#")?;
    let base = args.first()?;
    if crate::lower::has_var(base) { return None; }
    let ptr = NORMALIZER.with(|n| n.get());
    if ptr.is_null() { return None; }
    // SAFETY: the guard keeps the pointer valid for the lowering pass that
    // installed it, which outlives every substitution performed during it.
    let ctx = unsafe { &*ptr };
    ctx.associated_type(base, assoc)
}

impl TyCtx {
    /// `<base as Trait>::assoc` for the trait impl of `base` defining `assoc`.
    pub(crate) fn associated_type(&self, base: &Ty, assoc: &str) -> Option<Ty> {
        for imp in &self.impls {
            if imp.trait_ref.is_none() { continue; }
            let Some((_, written)) = imp.assoc_types.iter().find(|(n, _)| n == assoc) else { continue };
            let params: Vec<String> = imp.generics.params.iter().map(|p| p.name.clone()).collect();
            let Ok(pattern) = lower_type(&imp.self_ty, &params, self) else { continue };
            let Some(mut bindings) = type_pattern_bindings(&pattern, base) else { continue };
            bindings.insert("Self".into(), base.clone());
            let mut scope = params.clone();
            scope.push("Self".into());
            let Ok(raw) = lower_type(written, &scope, self) else { continue };
            return Some(subst_ty(&raw, &bindings));
        }
        None
    }
}

pub(crate) fn intrinsic_type_arity(name: &str) -> Option<usize> {
    if name == "Array" { Some(1) } else { prim_of(name).map(|_| 0) }
}

fn prim_of(name: &str) -> Option<Prim> {
    Some(match name {
        "i8" => Prim::I8, "i16" => Prim::I16, "i32" => Prim::I32, "i64" => Prim::I64,
        "u8" => Prim::U8, "u16" => Prim::U16, "u32" => Prim::U32, "u64" => Prim::U64,
        "f32" => Prim::F32, "f64" => Prim::F64,
        "bool" => Prim::Bool, "char" => Prim::Char, "String" => Prim::Str,
        "RawPtr" => Prim::RawPtr,
        _ => return None,
    })
}

fn const_usize(e: &Expr) -> TResult<u64> {
    match &*e.kind {
        ExprKind::Int(n, _) => Ok(*n),
        _ => Err(TypeError { msg: "array length must be an integer literal".into(), span: e.span }),
    }
}

/// The default scalar type for an unsuffixed/suffixed numeric literal.
pub fn suffix_prim(s: NumSuffix, is_float: bool) -> Prim {
    match s {
        NumSuffix::I8 => Prim::I8, NumSuffix::I16 => Prim::I16,
        NumSuffix::I32 => Prim::I32, NumSuffix::I64 => Prim::I64,
        NumSuffix::U8 => Prim::U8, NumSuffix::U16 => Prim::U16,
        NumSuffix::U32 => Prim::U32, NumSuffix::U64 => Prim::U64,
        NumSuffix::F32 => Prim::F32, NumSuffix::F64 => Prim::F64,
        NumSuffix::None => if is_float { Prim::F64 } else { Prim::I64 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse_module;
    use crate::resolve::resolve_module;

    fn ctx(src: &str) -> TyCtx {
        let m = parse_module(&lex(src).unwrap()).unwrap();
        let r = resolve_module(m).unwrap();
        TyCtx::from_globals(&r.globals)
    }

    #[test]
    fn prims_and_named() {
        let c = ctx("struct Point { x: i64 } enum E { A, B }");
        let g: Vec<String> = vec![];
        let span = Span::new(0, 0);
        let ti = |k| Type { kind: k, span };
        assert_eq!(
            lower_type(&ti(TypeKind::Path(Path::single("i64".into(), span), vec![])), &g, &c).unwrap(),
            Ty::Prim(Prim::I64)
        );
        let pt = lower_type(&ti(TypeKind::Path(Path::single("Point".into(), span), vec![])), &g, &c).unwrap();
        assert!(matches!(pt, Ty::Named { name, .. } if name == "Point"));
    }

    #[test]
    fn generic_var() {
        let c = ctx("fn id<T>(x: T) -> T { x }");
        let g = vec!["T".to_string()];
        let span = Span::new(0, 0);
        let t = Type { kind: TypeKind::Path(Path::single("T".into(), span), vec![]), span };
        assert_eq!(lower_type(&t, &g, &c).unwrap(), Ty::Var("T".into()));
    }

    #[test]
    fn variants_indexed() {
        let c = ctx("enum Option<T> { None, Some(T) }");
        assert_eq!(c.variants.get("Option::Some").unwrap().1, 1);
        assert_eq!(c.variants.get("None").unwrap().1, 0);
    }
}
