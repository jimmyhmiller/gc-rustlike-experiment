//! Qualify type positions before type checking. The scope of an alias use is
//! the source declaration's scope; an expanded target cannot accidentally bind
//! to a same-spelled declaration at its later use site.
use crate::{
    ast::*,
    resolve::{GlobalTable, ResolveError},
    type_alias::AliasView,
};

struct Qualifier<'a> {
    g: &'a GlobalTable,
    module: &'a str,
    /// Inside an impl: the path `Self` stands for in expressions and patterns.
    self_path: Option<Vec<String>>,
    /// The trait bounds on type parameters in scope, which say which trait a
    /// bare `T::Output` projects through.
    proj_bounds: std::cell::RefCell<Vec<(String, TraitRef)>>,
    /// The alias whose target is being qualified (see `AliasView::defining`).
    defining: &'a str,
}
impl Qualifier<'_> {
    fn view(&self) -> AliasView<'_> {
        AliasView {
            globals: self.g,
            resolved: &self.g.resolved_aliases,
            module: self.module,
            defining: self.defining,
        }
    }
    fn visible(&self, name: &str, span: crate::lexer::Span) -> Result<(), ResolveError> {
        if crate::resolve::visible_from(self.g, name, self.module) {
            Ok(())
        } else {
            Err(ResolveError {
                msg: format!("`{name}` is private and not accessible from here (mark it `pub`)"),
                span,
            })
        }
    }
    fn ty(&self, t: &mut Type, generics: &[String], namespace: bool) -> Result<(), ResolveError> {
        match &mut t.kind {
            TypeKind::Path(path, args) => {
                for arg in args.iter_mut() {
                    self.ty(arg, generics, false)?;
                }
                // `impl Trait` results: only the arguments are types here.
                if path.last().starts_with("impl#") {
                    return Ok(());
                }
                // `Self::Item` / `I::Item`: an associated type of a type parameter.
                if path.segments.len() == 2 && crate::types::is_projection_args(args)
                    && (path.segments[0] == "Self" || generics.iter().any(|name| *name == path.segments[0]))
                {
                    // `T::Output` with `T: Mul<f64>`: project through that bound.
                    if args.is_empty() && path.segments[0] != "Self" {
                        let bounds = self.proj_bounds.borrow();
                        let mut through = bounds.iter().filter(|(p, b)| *p == path.segments[0]
                            && self.g.traits.get(&b.path.segments.join("::")).is_some_and(|t| t.items.iter().any(|i| matches!(i, TraitItem::AssocType(n) if *n == path.segments[1]))));
                        if let (Some((_, b)), None) = (through.next(), through.next()) {
                            let marker = Type { kind: TypeKind::Path(Path::single(format!("trait#{}", b.path.segments.join("::")), b.span), b.args.clone()), span: b.span };
                            args.push(marker);
                        }
                    } else if let Some(Type { kind: TypeKind::Path(_, targs), .. }) = args.first_mut() {
                        for a in targs { self.ty(a, generics, false)?; }
                    }
                    return Ok(());
                }
                if path.is_single() && generics.iter().any(|name| name == path.last()) {
                    if !args.is_empty() {
                        return Err(ResolveError {
                            msg: format!(
                                "type parameter `{}` takes no type arguments",
                                path.last()
                            ),
                            span: t.span,
                        });
                    }
                    return Ok(());
                }
                if path.is_single() {
                    if let Some(arity) = crate::types::intrinsic_type_arity(path.last()) {
                        if args.len() != arity {
                            return Err(ResolveError { msg: format!("type `{}` takes {arity} type argument(s), got {}", path.last(), args.len()), span: t.span });
                        }
                        return Ok(());
                    }
                }
                let written = path.segments.join("::");
                if let Some(canon) = self.view().canonical_name(&written) {
                    self.visible(&canon, t.span)?;
                    let arity = self
                        .g
                        .resolved_aliases
                        .get(&canon)
                        .map(|a| a.params.len())
                        .or_else(|| self.g.structs.get(&canon).map(|s| s.generics.params.len()))
                        .or_else(|| self.g.enums.get(&canon).map(|e| e.generics.params.len()))
                        .unwrap();
                    // Omitted trailing arguments take the type's defaults
                    // (`struct W<T = i64>` written as `W`).
                    let params = self.g.structs.get(&canon).map(|s| &s.generics.params)
                        .or_else(|| self.g.enums.get(&canon).map(|e| &e.generics.params));
                    if let Some(params) = params {
                        if !namespace && args.len() < params.len() && params[args.len()..].iter().all(|p| p.default.is_some()) {
                            for i in args.len()..params.len() {
                                let map: Vec<(String, Type)> = params.iter().zip(args.iter()).map(|(p, a)| (p.name.clone(), a.clone())).collect();
                                let mut t = params[i].default.clone().unwrap();
                                subst_written(&mut t, &map);
                                self.ty(&mut t, generics, false)?;
                                args.push(t);
                            }
                        }
                    }
                    let bare_namespace =
                        namespace && args.is_empty() && !self.g.aliases.contains_key(&canon);
                    if !bare_namespace && args.len() != arity {
                        return Err(ResolveError {
                            msg: format!(
                                "type `{canon}` takes {arity} type argument(s), got {}",
                                args.len()
                            ),
                            span: t.span,
                        });
                    }
                    path.segments = canon.split("::").map(str::to_string).collect();
                } else if !path.is_single() || !crate::resolve::is_builtin_type(path.last()) {
                    return Err(ResolveError {
                        msg: format!("unknown or ambiguous type `{written}`"),
                        span: t.span,
                    });
                }
            }
            TypeKind::Tuple(types) => {
                for ty in types {
                    self.ty(ty, generics, false)?;
                }
            }
            TypeKind::Array(elem, _) => self.ty(elem, generics, false)?,
            TypeKind::Fn(params, ret) | TypeKind::ExternFn(params, ret) => {
                for ty in params {
                    self.ty(ty, generics, false)?;
                }
                if let Some(ret) = ret {
                    self.ty(ret, generics, false)?;
                }
            }
            TypeKind::SelfType => {}
        }
        Ok(())
    }
    /// Qualify a trait reference. Omitted trailing arguments take the trait's
    /// defaults, with `Self` meaning `self_ty` (the implementing or bounded
    /// type): `impl Add for V` is `impl Add<V> for V`.
    fn trait_ref(&self, bound: &mut TraitRef, scope: &[String], self_ty: &Type) -> Result<(), ResolveError> {
        let written = bound.path.segments.join("::");
        if bound.path.is_single() && scope.contains(&written) {
            return Err(ResolveError { msg: format!("type parameter `{written}` is not a trait"), span: bound.span });
        }
        let canonical = self.view().canonical_trait(&written).map_err(|msg| ResolveError { msg, span: bound.span })?;
        self.visible(&canonical, bound.span)?;
        let params = &self.g.traits[&canonical].generics.params;
        let arity = params.len();
        if bound.args.len() < arity && params[bound.args.len()..].iter().all(|p| p.default.is_some()) {
            for i in bound.args.len()..arity {
                let mut map: Vec<(String, Type)> = params.iter().zip(&bound.args).map(|(p, a)| (p.name.clone(), a.clone())).collect();
                map.push(("Self".into(), self_ty.clone()));
                let mut t = params[i].default.clone().unwrap();
                subst_written(&mut t, &map);
                bound.args.push(t);
            }
        }
        if bound.args.len() != arity {
            return Err(ResolveError { msg: format!("trait `{canonical}` takes {arity} type argument(s), got {}", bound.args.len()), span: bound.span });
        }
        bound.path.segments = canonical.split("::").map(str::to_string).collect();
        for arg in &mut bound.args { self.ty(arg, scope, false)?; }
        for (_, ty) in &mut bound.bindings { self.ty(ty, scope, false)?; }
        Ok(())
    }
    fn generics(&self, g: &mut Generics, outer: &[String]) -> Result<Vec<String>, ResolveError> {
        let mut names = std::collections::HashSet::new();
        for parameter in &g.params {
            if !names.insert(parameter.name.as_str()) {
                return Err(ResolveError { msg: format!("duplicate type parameter `{}`", parameter.name), span: parameter.span });
            }
        }
        let mut scope = outer.to_vec();
        scope.extend(g.params.iter().map(|p| p.name.clone()));
        for param in &mut g.params {
            let me = Type { kind: TypeKind::Path(Path::single(param.name.clone(), param.span), vec![]), span: param.span };
            for bound in &mut param.bounds {
                self.trait_ref(bound, &scope, &me)?;
            }
        }
        for clause in &mut g.where_clauses {
            self.ty(&mut clause.ty, &scope, false)?;
            let me = clause.ty.clone();
            for bound in &mut clause.bounds {
                self.trait_ref(bound, &scope, &me)?;
            }
        }
        Ok(scope)
    }
    fn signature(
        &self,
        params: &mut [Param],
        ret: &mut Option<Type>,
        scope: &[String],
    ) -> Result<(), ResolveError> {
        for param in params {
            self.ty(&mut param.ty, scope, false)?;
        }
        if let Some(ret) = ret {
            self.ty(ret, scope, false)?;
        }
        Ok(())
    }
    fn function(&self, f: &mut FnDef, outer: &[String]) -> Result<(), ResolveError> {
        let scope = self.generics(&mut f.generics, outer)?;
        let mark = self.proj_bounds.borrow().len();
        self.push_bounds(&f.generics);
        let result = self.signature(&mut f.params, &mut f.ret, &scope).and_then(|_| self.block(&mut f.body, &scope));
        self.proj_bounds.borrow_mut().truncate(mark);
        result
    }
    fn push_bounds(&self, g: &Generics) {
        let mut bounds = self.proj_bounds.borrow_mut();
        for p in &g.params {
            for b in &p.bounds { bounds.push((p.name.clone(), b.clone())); }
        }
        for w in &g.where_clauses {
            if let TypeKind::Path(p, a) = &w.ty.kind {
                if p.is_single() && a.is_empty() {
                    for b in &w.bounds { bounds.push((p.last().to_string(), b.clone())); }
                }
            }
        }
    }
    fn prefix(&self, path: &mut Path) -> Result<(), ResolveError> {
        // A value suffix is a variant or associated function. Keep an alias
        // prefix intact here: lowering still needs its specialized arguments.
        for count in (1..path.segments.len()).rev() {
            let written = path.segments[..count].join("::");
            if let Some(canon) = self.view().canonical_name(&written) {
                self.visible(&canon, path.span)?;
                let mut parts: Vec<_> = canon.split("::").map(str::to_string).collect();
                parts.extend_from_slice(&path.segments[count..]);
                path.segments = parts;
                break;
            }
        }
        Ok(())
    }
    fn constructor(&self, path: &mut Path) -> Result<(), ResolveError> {
        let written = path.segments.join("::");
        if let Some(canon) = self.view().canonical_name(&written) {
            self.visible(&canon, path.span)?;
            path.segments = canon.split("::").map(str::to_string).collect();
            Ok(())
        } else {
            self.prefix(path)
        }
    }
    /// `Self::X` / `Self` in an impl names the impl's type.
    fn expand_self(&self, path: &mut Path) {
        if let (Some(sp), Some("Self")) = (&self.self_path, path.segments.first().map(String::as_str)) {
            let mut segments = sp.clone();
            segments.extend(path.segments.drain(1..));
            path.segments = segments;
        }
    }
    /// The canonical struct a constructor path names, with its body.
    fn struct_named(&self, path: &Path) -> Option<(String, &StructBody)> {
        let canon = self.view().canonical_name(&path.segments.join("::"))?;
        self.g.structs.get(&canon).map(|s| (canon, &s.body))
    }
    fn pattern(&self, pattern: &mut Pattern) -> Result<(), ResolveError> {
        // A tuple struct (`P(a, b)`) or unit struct (`U`) pattern is a record
        // pattern over its positional fields `0`, `1`, ...
        if let PatternKind::Variant { path, payload } = &mut pattern.kind {
            self.expand_self(path);
            if let Some((canon, body)) = self.struct_named(path) {
                if matches!(body, StructBody::Tuple(_) | StructBody::Unit) {
                    let arity = if let StructBody::Tuple(t) = body { t.len() } else { 0 };
                    let parts = std::mem::take(payload);
                    let parts = expand_rest(&parts, arity).unwrap_or(parts);
                    let fields = parts.into_iter().enumerate().map(|(i, p)| (i.to_string(), p)).collect();
                    let path = Path { segments: canon.split("::").map(str::to_string).collect(), span: path.span };
                    pattern.kind = PatternKind::Struct { path, fields, rest: false };
                }
            }
        }
        if let PatternKind::Struct { path, .. } = &mut pattern.kind { self.expand_self(path); }
        // An upper-case name the parser took for a unit variant is a binding
        // (`let N = 5;`) unless a variant, constant, or type has that name.
        if let PatternKind::Variant { path, payload } = &pattern.kind {
            if path.is_single() && payload.is_empty() {
                let name = path.last();
                let statics = self.g.by_last.get(name).into_iter().flatten()
                    .any(|full| self.g.consts.get(full).is_some_and(|c| c.is_static.is_some()));
                if statics {
                    return Err(ResolveError { msg: format!("bindings cannot shadow statics (`{name}` is a static)"), span: pattern.span });
                }
                let known = self.g.by_last.contains_key(name) || self.g.use_aliases.contains_key(name)
                    || self.g.enums.values().any(|e| e.variants.iter().any(|v| v.name == name));
                if !known {
                    pattern.kind = PatternKind::Binding { is_mut: false, name: name.to_string() };
                    return Ok(());
                }
            }
        }
        match &mut pattern.kind {
            PatternKind::Variant { path, payload } => {
                self.prefix(path)?;
                for p in payload {
                    self.pattern(p)?;
                }
            }
            PatternKind::Struct { path, fields, .. } => {
                self.constructor(path)?;
                for (_, p) in fields {
                    self.pattern(p)?;
                }
            }
            PatternKind::Tuple(parts) => {
                for p in parts {
                    self.pattern(p)?;
                }
            }
            PatternKind::At { pat, .. } => self.pattern(pat)?,
            PatternKind::Wildcard | PatternKind::Binding { .. } | PatternKind::Literal(_) => {}
        }
        Ok(())
    }
    fn block(&self, block: &mut Block, scope: &[String]) -> Result<(), ResolveError> {
        for stmt in &mut block.stmts {
            match stmt {
                Stmt::Let {
                    pattern, ty, init, ..
                } => {
                    self.pattern(pattern)?;
                    if let Some(ty) = ty {
                        self.ty(ty, scope, false)?;
                    }
                    if let Some(e) = init {
                        self.expr(e, scope)?;
                    }
                }
                Stmt::Expr(e) => self.expr(e, scope)?,
                Stmt::Item(_) => {}
            }
        }
        if let Some(tail) = &mut block.tail {
            self.expr(tail, scope)?;
        }
        Ok(())
    }
    fn expr(&self, e: &mut Expr, scope: &[String]) -> Result<(), ResolveError> {
        // `P(a, b)` builds a tuple struct and `U` a unit struct: record
        // literals over the positional fields.
        let span = e.span;
        let rewritten = match &*e.kind {
            ExprKind::Call(callee, args) => match &*callee.kind {
                ExprKind::Path(path) => {
                    let mut path = path.clone();
                    self.expand_self(&mut path);
                    match self.struct_named(&path) {
                        Some((canon, StructBody::Tuple(_))) if !self.g.fns.contains_key(&canon) => Some(ExprKind::StructLit {
                            path: Path { segments: canon.split("::").map(str::to_string).collect(), span: path.span },
                            fields: args.iter().enumerate().map(|(i, a)| FieldInit { name: i.to_string(), value: Some(a.clone()), span: a.span }).collect(),
                            span,
                        }),
                        _ => None,
                    }
                }
                _ => None,
            },
            ExprKind::Path(path) if path.segments.len() <= 2 => {
                let mut path = path.clone();
                self.expand_self(&mut path);
                match self.struct_named(&path) {
                    Some((canon, StructBody::Unit)) => Some(ExprKind::StructLit {
                        path: Path { segments: canon.split("::").map(str::to_string).collect(), span: path.span },
                        fields: vec![],
                        span,
                    }),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(kind) = rewritten { e.kind = std::sync::Arc::new(kind); }
        match std::sync::Arc::make_mut(&mut e.kind) {
            ExprKind::Path(path) => { self.expand_self(path); self.prefix(path)? }
            ExprKind::Call(callee, _) if matches!(&*callee.kind, ExprKind::Path(_)) => {}
            _ => {}
        }
        match std::sync::Arc::make_mut(&mut e.kind) {
            ExprKind::Path(_) => {}
            ExprKind::Call(callee, args) => {
                self.expr(callee, scope)?;
                for arg in args {
                    self.expr(arg, scope)?;
                }
            }
            ExprKind::MethodCall { recv, args, .. } => {
                self.expr(recv, scope)?;
                for arg in args {
                    self.expr(arg, scope)?;
                }
            }
            ExprKind::Field { base, .. } => self.expr(base, scope)?,
            ExprKind::Index { base, index } => {
                self.expr(base, scope)?;
                self.expr(index, scope)?;
            }
            ExprKind::Unary(_, x) | ExprKind::Try(x) => self.expr(x, scope)?,
            ExprKind::Binary(_, l, r) => {
                self.expr(l, scope)?;
                self.expr(r, scope)?;
            }
            ExprKind::Assign { target, value, .. } => {
                self.expr(target, scope)?;
                self.expr(value, scope)?;
            }
            ExprKind::Cast(value, ty) => {
                self.expr(value, scope)?;
                self.ty(ty, scope, false)?;
            }
            ExprKind::StructLit { path, fields, .. } => {
                self.expand_self(path);
                self.constructor(path)?;
                for field in fields {
                    if let Some(value) = &mut field.value {
                        self.expr(value, scope)?;
                    }
                }
            }
            ExprKind::Tuple(parts) | ExprKind::Array(ArrayLit::Elems(parts)) => {
                for part in parts {
                    self.expr(part, scope)?;
                }
            }
            ExprKind::Array(ArrayLit::Repeat(value, count)) => {
                self.expr(value, scope)?;
                self.expr(count, scope)?;
            }
            ExprKind::Block(block) | ExprKind::Unsafe(block) | ExprKind::Loop { body: block } => {
                self.block(block, scope)?
            }
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond, scope)?;
                self.block(then_branch, scope)?;
                if let Some(branch) = else_branch {
                    self.expr(branch, scope)?;
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, scope)?;
                for arm in arms {
                    self.pattern(&mut arm.pattern)?;
                    if let Some(guard) = &mut arm.guard {
                        self.expr(guard, scope)?;
                    }
                    self.expr(&mut arm.body, scope)?;
                }
            }
            ExprKind::While { cond, body } => {
                self.expr(cond, scope)?;
                self.block(body, scope)?;
            }
            ExprKind::For { pat, iter, body } => {
                self.pattern(pat)?;
                self.expr(iter, scope)?;
                self.block(body, scope)?;
            }
            ExprKind::Closure { params, ret, body } => {
                crate::resolve::validate_parameter_names(params.iter().map(|p| (p.name.as_str(), p.span)))?;
                for param in params {
                    if let Some(ty) = &mut param.ty {
                        self.ty(ty, scope, false)?;
                    }
                }
                if let Some(ret) = ret {
                    self.ty(ret, scope, false)?;
                }
                self.expr(body, scope)?;
            }
            ExprKind::Return(value) | ExprKind::Break(value) => {
                if let Some(value) = value {
                    self.expr(value, scope)?;
                }
            }
            ExprKind::Range { lo, hi, .. } => {
                if let Some(lo) = lo {
                    self.expr(lo, scope)?;
                }
                if let Some(hi) = hi {
                    self.expr(hi, scope)?;
                }
            }
            ExprKind::Int(..)
            | ExprKind::Float(..)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Bool(_)
            | ExprKind::Unit
            | ExprKind::Continue => {}
        }
        Ok(())
    }
}
/// The part of `actual` (an impl's written type) standing where `declared`
/// (the trait's) says `Self::name`.
fn infer_assoc(declared: &Type, actual: &Type, name: &str) -> Option<Type> {
    match (&declared.kind, &actual.kind) {
        (TypeKind::Path(p, _), _) if p.segments.len() == 2 && p.segments[0] == "Self" && p.segments[1] == name => {
            // The impl may also write `Self::Item` itself, which says nothing.
            match &actual.kind {
                TypeKind::Path(ap, _) if ap.segments.first().is_some_and(|s| s == "Self") => None,
                _ => Some(actual.clone()),
            }
        }
        (TypeKind::Path(dp, da), TypeKind::Path(ap, aa)) if dp.last() == ap.last() && da.len() == aa.len() =>
            da.iter().zip(aa).find_map(|(d, a)| infer_assoc(d, a, name)),
        (TypeKind::Tuple(ds), TypeKind::Tuple(as_)) if ds.len() == as_.len() => ds.iter().zip(as_).find_map(|(d, a)| infer_assoc(d, a, name)),
        (TypeKind::Array(d, _), TypeKind::Array(a, _)) => infer_assoc(d, a, name),
        (TypeKind::Fn(dp, dr), TypeKind::Fn(ap, ar)) if dp.len() == ap.len() => dp.iter().zip(ap).find_map(|(d, a)| infer_assoc(d, a, name))
            .or_else(|| match (dr, ar) { (Some(d), Some(a)) => infer_assoc(d, a, name), _ => None }),
        _ => None,
    }
}

/// Replace `Self` and the named parameters in a written type.
fn subst_written(t: &mut Type, map: &[(String, Type)]) {
    match &mut t.kind {
        TypeKind::SelfType => {
            if let Some((_, r)) = map.iter().find(|(n, _)| n == "Self") { *t = r.clone(); }
        }
        TypeKind::Path(path, args) => {
            if path.is_single() && args.is_empty() {
                if let Some((_, r)) = map.iter().find(|(n, _)| n == path.last()) { *t = r.clone(); return; }
            }
            for a in args { subst_written(a, map); }
        }
        TypeKind::Tuple(ts) => for a in ts { subst_written(a, map); },
        TypeKind::Array(e, _) => subst_written(e, map),
        TypeKind::Fn(ps, r) | TypeKind::ExternFn(ps, r) => {
            for a in ps { subst_written(a, map); }
            if let Some(r) = r { subst_written(r, map); }
        }
    }
}

fn module(name: &str) -> &str {
    name.rsplit_once("::").map_or("", |(module, _)| module)
}

pub(crate) fn qualify_types(g: &mut GlobalTable) -> Result<(), ResolveError> {
    // Work on copies so every resolution sees the same complete name table,
    // independent of hash-map iteration and declaration ordering.
    let mut fns = g.fns.clone();
    let mut structs = g.structs.clone();
    let mut enums = g.enums.clone();
    let mut traits = g.traits.clone();
    let mut impls = g.impls.clone();
    let mut consts = g.consts.clone();
    let mut aliases = g.aliases.clone();
    for (name, f) in &mut fns {
        Qualifier { g, module: module(name), self_path: None, proj_bounds: Default::default(), defining: "" }
        .function(f, &[])?;
    }
    for (name, alias) in &mut aliases {
        let q = Qualifier { g, module: module(name), self_path: None, proj_bounds: Default::default(), defining: name };
        let scope = q.generics(&mut alias.generics, &[])?;
        q.ty(&mut alias.ty, &scope, false)?;
    }
    for (name, s) in &mut structs {
        let q = Qualifier { g, module: module(name), self_path: None, proj_bounds: Default::default(), defining: "" };
        let scope = q.generics(&mut s.generics, &[])?;
        match &mut s.body {
            StructBody::Named(fields) => {
                for field in fields {
                    q.ty(&mut field.ty, &scope, false)?;
                }
            }
            StructBody::Tuple(types) => {
                for ty in types {
                    q.ty(ty, &scope, false)?;
                }
            }
            StructBody::Unit => {}
        }
    }
    for (name, e) in &mut enums {
        let q = Qualifier { g, module: module(name), self_path: None, proj_bounds: Default::default(), defining: "" };
        let scope = q.generics(&mut e.generics, &[])?;
        for variant in &mut e.variants {
            match &mut variant.payload {
                VariantPayload::Named(fields) => {
                    for field in fields {
                        q.ty(&mut field.ty, &scope, false)?;
                    }
                }
                VariantPayload::Tuple(types) => {
                    for ty in types {
                        q.ty(ty, &scope, false)?;
                    }
                }
                VariantPayload::None => {}
            }
        }
    }
    for (name, t) in &mut traits {
        let q = Qualifier { g, module: module(name), self_path: None, proj_bounds: Default::default(), defining: "" };
        let scope = q.generics(&mut t.generics, &[])?;
        for bound in &mut t.supertraits {
            let me = Type { kind: TypeKind::SelfType, span: bound.span };
            q.trait_ref(bound, &scope, &me)?;
        }
        for item in &mut t.items {
            match item {
                TraitItem::Required(f) => {
                    let inner = q.generics(&mut f.generics, &scope)?;
                    q.signature(&mut f.params, &mut f.ret, &inner)?;
                }
                TraitItem::Provided(f) => q.function(f, &scope)?,
                TraitItem::Const(c) => {
                    q.ty(&mut c.ty, &scope, false)?;
                    q.expr(&mut c.value, &scope)?;
                }
                TraitItem::AssocType(_) => {}
            }
        }
    }
    for (index, i) in impls.iter_mut().enumerate() {
        let q = Qualifier { g, module: g.impl_modules.get(index).map_or("", String::as_str), self_path: None, proj_bounds: Default::default(), defining: "" };
        let scope = q.generics(&mut i.generics, &[])?;
        q.push_bounds(&i.generics);
        q.ty(&mut i.self_ty, &scope, i.trait_ref.is_none())?;
        let mut q = q;
        if let TypeKind::Path(p, _) = &i.self_ty.kind {
            if !p.is_single() || !scope.iter().any(|g| g == p.last()) { q.self_path = Some(p.segments.clone()); }
        }
        if let Some(tr) = &mut i.trait_ref {
            let me = i.self_ty.clone();
            q.trait_ref(tr, &scope, &me)?;
            // Associated types the impl leaves out take the trait's defaults,
            // or are read off the impl's method signatures (`fn next(..) ->
            // Option<i64>` against `Option<Self::Item>` gives `Item = i64`).
            if let Some(def) = g.traits.get(&tr.path.segments.join("::")) {
                for item in &def.items {
                    let TraitItem::AssocType(name) = item else { continue };
                    if i.assoc_types.iter().any(|(n, _)| n == name) || def.assoc_defaults.iter().any(|(n, _)| n == name) { continue; }
                    let found = def.items.iter().find_map(|ti| {
                        let (mname, params, ret) = match ti {
                            TraitItem::Required(f) => (&f.name, &f.params, &f.ret),
                            TraitItem::Provided(f) => (&f.name, &f.params, &f.ret),
                            _ => return None,
                        };
                        let m = i.items.iter().find(|m| &m.name == mname)?;
                        let from_ret = match (ret, &m.ret) { (Some(t), Some(a)) => infer_assoc(t, a, name), _ => None };
                        from_ret.or_else(|| params.iter().zip(&m.params).find_map(|(t, a)| infer_assoc(&t.ty, &a.ty, name)))
                    });
                    if let Some(t) = found { i.assoc_types.push((name.clone(), t)); }
                }
                for (name, default) in &def.assoc_defaults {
                    if i.assoc_types.iter().any(|(n, _)| n == name) { continue; }
                    let mut map: Vec<(String, Type)> = def.generics.params.iter().zip(&tr.args).map(|(p, a)| (p.name.clone(), a.clone())).collect();
                    map.push(("Self".into(), me.clone()));
                    let mut t = default.clone();
                    subst_written(&mut t, &map);
                    i.assoc_types.push((name.clone(), t));
                }
            }
        }
        for method in &mut i.items {
            q.function(method, &scope)?;
        }
        for (_, ty) in &mut i.assoc_types {
            q.ty(ty, &scope, false)?;
        }
        for c in &mut i.consts {
            q.ty(&mut c.ty, &scope, false)?;
            q.expr(&mut c.value, &scope)?;
        }
    }
    for (name, c) in &mut consts {
        let q = Qualifier { g, module: module(name), self_path: None, proj_bounds: Default::default(), defining: "" };
        q.ty(&mut c.ty, &[], false)?;
        q.expr(&mut c.value, &[])?;
    }
    g.fns = fns;
    g.structs = structs;
    g.enums = enums;
    g.traits = traits;
    g.impls = impls;
    g.consts = consts;
    // Expansion happens before qualification; retain the now-canonical bounds
    // with each normalized alias for inferred constructor and pattern uses.
    for (name, alias) in &aliases {
        std::rc::Rc::make_mut(g.resolved_aliases.get_mut(name).expect("resolved alias declaration")).generics = alias.generics.clone();
    }
    g.aliases = aliases;
    Ok(())
}
