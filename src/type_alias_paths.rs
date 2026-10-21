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
}
impl Qualifier<'_> {
    fn view(&self) -> AliasView<'_> {
        AliasView {
            globals: self.g,
            resolved: &self.g.resolved_aliases,
            module: self.module,
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
                // `Self::Item` / `I::Item`: an associated type of a type parameter.
                if path.segments.len() == 2 && args.is_empty()
                    && (path.segments[0] == "Self" || generics.iter().any(|name| *name == path.segments[0]))
                {
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
    fn trait_ref(&self, bound: &mut TraitRef, scope: &[String]) -> Result<(), ResolveError> {
        let written = bound.path.segments.join("::");
        if bound.path.is_single() && scope.contains(&written) {
            return Err(ResolveError { msg: format!("type parameter `{written}` is not a trait"), span: bound.span });
        }
        let canonical = self.view().canonical_trait(&written).map_err(|msg| ResolveError { msg, span: bound.span })?;
        self.visible(&canonical, bound.span)?;
        let arity = self.g.traits[&canonical].generics.params.len();
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
            for bound in &mut param.bounds {
                self.trait_ref(bound, &scope)?;
            }
        }
        for clause in &mut g.where_clauses {
            self.ty(&mut clause.ty, &scope, false)?;
            for bound in &mut clause.bounds {
                self.trait_ref(bound, &scope)?;
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
        self.signature(&mut f.params, &mut f.ret, &scope)?;
        self.block(&mut f.body, &scope)
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
    fn pattern(&self, pattern: &mut Pattern) -> Result<(), ResolveError> {
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
        match std::sync::Arc::make_mut(&mut e.kind) {
            ExprKind::Path(path) => self.prefix(path)?,
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
        Qualifier {
            g,
            module: module(name),
        }
        .function(f, &[])?;
    }
    for (name, alias) in &mut aliases {
        let q = Qualifier { g, module: module(name) };
        let scope = q.generics(&mut alias.generics, &[])?;
        q.ty(&mut alias.ty, &scope, false)?;
    }
    for (name, s) in &mut structs {
        let q = Qualifier {
            g,
            module: module(name),
        };
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
        let q = Qualifier {
            g,
            module: module(name),
        };
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
        let q = Qualifier {
            g,
            module: module(name),
        };
        let scope = q.generics(&mut t.generics, &[])?;
        for bound in &mut t.supertraits {
            q.trait_ref(bound, &scope)?;
        }
        for item in &mut t.items {
            match item {
                TraitItem::Required(f) => {
                    let inner = q.generics(&mut f.generics, &scope)?;
                    q.signature(&mut f.params, &mut f.ret, &inner)?;
                }
                TraitItem::Provided(f) => q.function(f, &scope)?,
                TraitItem::AssocType(_) => {}
            }
        }
    }
    for (index, i) in impls.iter_mut().enumerate() {
        let q = Qualifier {
            g,
            module: g.impl_modules.get(index).map_or("", String::as_str),
        };
        let scope = q.generics(&mut i.generics, &[])?;
        q.ty(&mut i.self_ty, &scope, i.trait_ref.is_none())?;
        if let Some(tr) = &mut i.trait_ref {
            q.trait_ref(tr, &scope)?;
        }
        for method in &mut i.items {
            q.function(method, &scope)?;
        }
        for (_, ty) in &mut i.assoc_types {
            q.ty(ty, &scope, false)?;
        }
    }
    for (name, c) in &mut consts {
        let q = Qualifier {
            g,
            module: module(name),
        };
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
