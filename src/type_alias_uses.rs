//! Check alias applications before type erasure, in their lexical generic scope.
//! This pass borrows the qualified AST and never copies function bodies.
use super::*;
use crate::lexer::Span;

#[derive(Clone, Default)]
struct Scope {
    parameters: Vec<String>,
    substitution: HashMap<String, Ty>,
    conditions: Vec<Goal>,
    display_names: HashMap<String, Ty>,
}
struct Checker<'a> {
    ctx: DeclarationContext<'a>,
    next_variable: usize,
    // Facts proved without assumptions can be shared within this immutable
    // declaration context. Bound storage independently of program size.
    concrete_proofs: HashSet<Goal>,
}
impl Checker<'_> {
    fn variable(&mut self) -> Ty {
        let id = self.next_variable;
        self.next_variable += 1;
        Ty::Var(format!("$alias_scope{id}"))
    }
    fn bind(&mut self, outer: &Scope, generics: &Generics) -> Scope {
        let mut scope = outer.clone();
        for parameter in &generics.params {
            scope.parameters.push(parameter.name.clone());
            let variable = self.variable();
            if let Ty::Var(name) = &variable {
                scope
                    .display_names
                    .insert(name.clone(), Ty::Var(parameter.name.clone()));
            }
            scope.substitution.insert(parameter.name.clone(), variable);
        }
        scope
    }
    fn instantiate(&self, ty: &Type, scope: &Scope) -> Result<Ty, TypeError> {
        Ok(subst_ty(
            &lower_type_with(ty, &scope.parameters, &self.ctx)?,
            &scope.substitution,
        ))
    }
    fn conditions(&self, generics: &Generics, scope: &mut Scope) -> Result<(), TypeError> {
        let mut conditions = Vec::new();
        for parameter in &generics.params {
            let subject = scope.substitution[&parameter.name].clone();
            for bound in &parameter.bounds {
                conditions.push(self.bound(subject.clone(), bound, scope)?);
            }
        }
        for predicate in &generics.where_clauses {
            let subject = self.instantiate(&predicate.ty, scope)?;
            for bound in &predicate.bounds {
                conditions.push(self.bound(subject.clone(), bound, scope)?);
            }
        }
        scope.conditions.extend(conditions);
        Ok(())
    }
    fn bound(&self, subject: Ty, bound: &TraitRef, scope: &Scope) -> Result<Goal, TypeError> {
        let arguments = bound
            .args
            .iter()
            .map(|ty| self.instantiate(ty, scope))
            .collect::<Result<_, _>>()?;
        Ok(bound_goal(
            subject,
            bound.path.segments.join("::"),
            arguments,
        ))
    }
    fn scope(&mut self, outer: &Scope, generics: &Generics) -> Result<Scope, TypeError> {
        let mut scope = self.bind(outer, generics);
        self.conditions(generics, &mut scope)?;
        Ok(scope)
    }
    fn check(
        &mut self,
        name: &str,
        scope: &Scope,
        mut types: Vec<&Type>,
        span: Span,
        declaration: bool,
    ) -> Result<(), TypeError> {
        let mut solver = Solver {
            ctx: &self.ctx,
            nodes: Vec::new(),
            ids: HashMap::new(),
            assumptions: HashSet::new(),
            established_impls: None,
            symbolic: true,
        };
        let mut required = Vec::new();
        while let Some(ty) = types.pop() {
            match &ty.kind {
                TypeKind::Path(path, args) => {
                    types.extend(args);
                    if path.is_single() && scope.substitution.contains_key(path.last()) {
                        continue;
                    }
                    let Some(alias) = self.ctx.globals.aliases.get(&path.segments.join("::"))
                    else {
                        continue;
                    };
                    let arguments = args
                        .iter()
                        .map(|arg| self.instantiate(arg, scope))
                        .collect::<Result<Vec<_>, _>>()?;
                    let bindings = alias
                        .generics
                        .params
                        .iter()
                        .map(|p| p.name.clone())
                        .zip(arguments)
                        .collect();
                    let mut obligations = Vec::new();
                    solver.conditions(&alias.generics, &bindings, &mut obligations)?;
                    required.extend(obligations.into_iter().map(|goal| (goal, ty.span)));
                }
                TypeKind::Tuple(parts) => types.extend(parts),
                TypeKind::Array(element, _) => types.push(element),
                TypeKind::Fn(arguments, ret) | TypeKind::ExternFn(arguments, ret) => {
                    types.extend(arguments);
                    types.extend(ret.as_deref());
                }
                TypeKind::SelfType => {
                    if !scope.substitution.contains_key("Self") {
                        return Err(TypeError {
                            msg: "`Self` is not defined in this type scope".into(),
                            span: ty.span,
                        });
                    }
                }
            }
        }
        if required.is_empty() && !declaration {
            return Ok(());
        }
        let (assumptions, concrete): (Vec<_>, Vec<_>) = scope
            .conditions
            .iter()
            .cloned()
            .partition(Goal::has_variables);
        required.extend(concrete.into_iter().map(|goal| (goal, span)));
        solver.assume(assumptions).map_err(|error| TypeError {
            msg: error.msg,
            span,
        })?;
        // Validate resource limits before hashing or cloning a cache key.
        let mut sites = Vec::new();
        for (goal, site) in required {
            let id = solver.intern(goal).map_err(|error| TypeError {
                msg: error.msg,
                span: site,
            })?;
            sites.push((id, site));
        }
        let cacheable = scope.conditions.is_empty()
            && sites
                .iter()
                .all(|(id, _)| !solver.nodes[*id].goal.has_variables());
        if cacheable
            && sites
                .iter()
                .all(|(id, _)| self.concrete_proofs.contains(&solver.nodes[*id].goal))
        {
            return Ok(());
        }
        let required = sites
            .iter()
            .map(|(id, site)| (solver.nodes[*id].goal.clone(), *site))
            .collect::<Vec<_>>();
        if cacheable {
            for (goal, _) in &required {
                if self.concrete_proofs.contains(goal) {
                    solver.assumptions.insert(goal.clone());
                }
            }
        }
        let results = solver
            .solve_roots(sites.into_iter().map(|(id, _)| id).collect())
            .map_err(|error| TypeError {
                msg: error.msg,
                span,
            })?;
        if let Some(((goal, span), _)) = required
            .iter()
            .zip(results)
            .find(|(_, satisfied)| !satisfied)
        {
            return Err(TypeError {
                msg: format!(
                    "type use in `{name}` does not satisfy required trait obligation `{}`",
                    match goal {
                        Goal::Trait(ty, bound, args) => Goal::Trait(
                            subst_ty(ty, &scope.display_names),
                            bound.clone(),
                            args.iter()
                                .map(|ty| subst_ty(ty, &scope.display_names))
                                .collect()
                        )
                        .description(),
                        Goal::Share(ty) =>
                            Goal::Share(subst_ty(ty, &scope.display_names)).description(),
                        Goal::Slot(ty) =>
                            Goal::Slot(subst_ty(ty, &scope.display_names)).description(),
                    }
                ),
                span: *span,
            });
        }
        if cacheable {
            for (goal, _) in required {
                if self.concrete_proofs.len() == MAX_OBLIGATIONS {
                    break;
                }
                self.concrete_proofs.insert(goal);
            }
        }
        Ok(())
    }
    fn needs_scope(&self, types: &[&Type], self_is_bound: bool) -> bool {
        let mut pending = types.to_vec();
        while let Some(ty) = pending.pop() {
            match &ty.kind {
                TypeKind::Path(path, args) => {
                    if self
                        .ctx
                        .globals
                        .aliases
                        .get(&path.segments.join("::"))
                        .is_some_and(|alias| {
                            alias.generics.params.iter().any(|p| !p.bounds.is_empty())
                                || !alias.generics.where_clauses.is_empty()
                        })
                    {
                        return true;
                    }
                    pending.extend(args);
                }
                TypeKind::Tuple(parts) => pending.extend(parts),
                TypeKind::Array(element, _) => pending.push(element),
                TypeKind::Fn(arguments, ret) | TypeKind::ExternFn(arguments, ret) => {
                    pending.extend(arguments);
                    pending.extend(ret.as_deref());
                }
                TypeKind::SelfType => {
                    if !self_is_bound {
                        return true;
                    }
                }
            }
        }
        false
    }
    fn function(&mut self, name: &str, function: &FnDef, outer: &Scope) -> Result<(), TypeError> {
        let mut types = generic_types(&function.generics);
        types.extend(function.params.iter().map(|p| &p.ty));
        types.extend(function.ret.as_ref());
        body_types(&function.body, &mut types);
        // Qualification has already made alias paths canonical. Unconstrained
        // aliases have proved their targets at declaration, so only explicit
        // constrained applications need a function-local proof environment.
        if !self.needs_scope(&types, outer.substitution.contains_key("Self")) {
            return Ok(());
        }
        let scope = self.scope(outer, &function.generics)?;
        self.check(name, &scope, types, function.span, false)
    }
}

fn generic_types(generics: &Generics) -> Vec<&Type> {
    let mut types = Vec::new();
    for parameter in &generics.params {
        for bound in &parameter.bounds {
            types.extend(&bound.args);
        }
    }
    for predicate in &generics.where_clauses {
        types.push(&predicate.ty);
        for bound in &predicate.bounds {
            types.extend(&bound.args);
        }
    }
    types
}

/// Walk bodies iteratively and exhaustively so new expression kinds cannot
/// silently bypass annotation checks. Patterns contain no surface types.
enum Node<'a> {
    Block(&'a Block),
    Expr(&'a Expr),
}
fn body_types<'a>(body: &'a Block, types: &mut Vec<&'a Type>) {
    collect_types(vec![Node::Block(body)], types);
}
fn collect_types<'a>(mut pending: Vec<Node<'a>>, types: &mut Vec<&'a Type>) {
    while let Some(node) = pending.pop() {
        match node {
            Node::Block(block) => {
                for statement in &block.stmts {
                    match statement {
                        Stmt::Let { ty, init, .. } => {
                            types.extend(ty.as_ref());
                            pending.extend(init.as_ref().map(Node::Expr));
                        }
                        Stmt::Expr(expr) => pending.push(Node::Expr(expr)),
                        Stmt::Item(_) => {} // Nested items are rejected by lowering.
                    }
                }
                pending.extend(block.tail.as_ref().map(Node::Expr));
            }
            Node::Expr(expr) => match expr.kind.as_ref() {
                ExprKind::Call(callee, arguments) => {
                    pending.push(Node::Expr(callee));
                    pending.extend(arguments.iter().map(Node::Expr));
                }
                ExprKind::MethodCall { recv, args, .. } => {
                    pending.push(Node::Expr(recv));
                    pending.extend(args.iter().map(Node::Expr));
                }
                ExprKind::Field { base, .. } => pending.push(Node::Expr(base)),
                ExprKind::Index { base, index } => {
                    pending.push(Node::Expr(base));
                    pending.push(Node::Expr(index));
                }
                ExprKind::Unary(_, value) | ExprKind::Try(value) => pending.push(Node::Expr(value)),
                ExprKind::Binary(_, left, right) => {
                    pending.push(Node::Expr(left));
                    pending.push(Node::Expr(right));
                }
                ExprKind::Assign { target, value, .. } => {
                    pending.push(Node::Expr(target));
                    pending.push(Node::Expr(value));
                }
                ExprKind::Cast(value, ty) => {
                    types.push(ty);
                    pending.push(Node::Expr(value));
                }
                ExprKind::StructLit { fields, .. } => {
                    pending.extend(
                        fields
                            .iter()
                            .filter_map(|f| f.value.as_ref())
                            .map(Node::Expr),
                    );
                }
                ExprKind::Tuple(parts) | ExprKind::Array(ArrayLit::Elems(parts)) => {
                    pending.extend(parts.iter().map(Node::Expr))
                }
                ExprKind::Array(ArrayLit::Repeat(value, count)) => {
                    pending.push(Node::Expr(value));
                    pending.push(Node::Expr(count));
                }
                ExprKind::Block(block)
                | ExprKind::Unsafe(block)
                | ExprKind::Loop { body: block } => pending.push(Node::Block(block)),
                ExprKind::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    pending.push(Node::Expr(cond));
                    pending.push(Node::Block(then_branch));
                    pending.extend(else_branch.as_ref().map(Node::Expr));
                }
                ExprKind::Match { scrutinee, arms } => {
                    pending.push(Node::Expr(scrutinee));
                    for arm in arms {
                        pending.extend(arm.guard.as_ref().map(Node::Expr));
                        pending.push(Node::Expr(&arm.body));
                    }
                }
                ExprKind::While { cond, body } => {
                    pending.push(Node::Expr(cond));
                    pending.push(Node::Block(body));
                }
                ExprKind::For { iter, body, .. } => {
                    pending.push(Node::Expr(iter));
                    pending.push(Node::Block(body));
                }
                ExprKind::Closure { params, ret, body } => {
                    types.extend(params.iter().filter_map(|p| p.ty.as_ref()));
                    types.extend(ret.as_ref());
                    pending.push(Node::Expr(body));
                }
                ExprKind::Return(value) | ExprKind::Break(value) => {
                    pending.extend(value.as_ref().map(Node::Expr))
                }
                ExprKind::Range { lo, hi, .. } => {
                    pending.extend(lo.as_ref().map(Node::Expr));
                    pending.extend(hi.as_ref().map(Node::Expr));
                }
                ExprKind::Int(..)
                | ExprKind::Float(..)
                | ExprKind::Str(_)
                | ExprKind::Char(_)
                | ExprKind::Bool(_)
                | ExprKind::Unit
                | ExprKind::Continue
                | ExprKind::Path(_) => {}
            },
        }
    }
}

pub(super) fn validate(globals: &crate::resolve::GlobalTable) -> Result<(), TypeError> {
    let mut checker = Checker {
        ctx: DeclarationContext::new(globals)?,
        next_variable: 0,
        concrete_proofs: HashSet::new(),
    };
    let empty = Scope::default();
    let mut names = globals.aliases.keys().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let alias = &globals.aliases[name];
        let scope = checker.scope(&empty, &alias.generics)?;
        let mut types = generic_types(&alias.generics);
        types.push(&alias.ty);
        checker.check(name, &scope, types, alias.span, true)?;
    }
    let mut names = globals.fns.keys().collect::<Vec<_>>();
    names.sort();
    for name in names {
        checker.function(name, &globals.fns[name], &empty)?;
    }
    let mut names = globals.structs.keys().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let structure = &globals.structs[name];
        let scope = checker.scope(&empty, &structure.generics)?;
        let mut types = generic_types(&structure.generics);
        match &structure.body {
            StructBody::Named(fields) => types.extend(fields.iter().map(|f| &f.ty)),
            StructBody::Tuple(parts) => types.extend(parts),
            StructBody::Unit => {}
        }
        checker.check(name, &scope, types, structure.span, false)?;
    }
    let mut names = globals.enums.keys().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let enumeration = &globals.enums[name];
        let scope = checker.scope(&empty, &enumeration.generics)?;
        let mut types = generic_types(&enumeration.generics);
        for variant in &enumeration.variants {
            match &variant.payload {
                VariantPayload::Named(fields) => types.extend(fields.iter().map(|f| &f.ty)),
                VariantPayload::Tuple(parts) => types.extend(parts),
                VariantPayload::None => {}
            }
        }
        checker.check(name, &scope, types, enumeration.span, false)?;
    }
    let mut names = globals.traits.keys().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let definition = &globals.traits[name];
        let mut scope = checker.bind(&empty, &definition.generics);
        let receiver = checker.variable();
        if let Ty::Var(variable) = &receiver {
            scope
                .display_names
                .insert(variable.clone(), Ty::Var("Self".into()));
        }
        scope.substitution.insert("Self".into(), receiver.clone());
        checker.conditions(&definition.generics, &mut scope)?;
        let arguments = definition
            .generics
            .params
            .iter()
            .map(|p| scope.substitution[&p.name].clone())
            .collect();
        scope
            .conditions
            .push(bound_goal(receiver, name.clone(), arguments));
        let mut types = generic_types(&definition.generics);
        for bound in &definition.supertraits {
            types.extend(&bound.args);
        }
        checker.check(name, &scope, types, definition.span, false)?;
        for item in &definition.items {
            match item {
                TraitItem::Const(_) => {}
                TraitItem::Provided(function) => checker.function(name, function, &scope)?,
                TraitItem::Required(signature) => {
                    let scope = checker.scope(&scope, &signature.generics)?;
                    let mut types = generic_types(&signature.generics);
                    types.extend(signature.params.iter().map(|p| &p.ty));
                    types.extend(signature.ret.as_ref());
                    checker.check(name, &scope, types, signature.span, false)?;
                }
                TraitItem::AssocType(_) => {}
            }
        }
    }
    for implementation in &globals.impls {
        let mut scope = checker.bind(&empty, &implementation.generics);
        let receiver = checker.instantiate(&implementation.self_ty, &scope)?;
        scope.substitution.insert("Self".into(), receiver);
        checker.conditions(&implementation.generics, &mut scope)?;
        let mut types = generic_types(&implementation.generics);
        types.push(&implementation.self_ty);
        if let Some(bound) = &implementation.trait_ref {
            types.extend(&bound.args);
        }
        checker.check("impl", &scope, types, implementation.span, false)?;
        for function in &implementation.items {
            checker.function(&function.name, function, &scope)?;
        }
    }
    let mut names = globals.consts.keys().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let constant = &globals.consts[name];
        let mut types = vec![&constant.ty];
        collect_types(vec![Node::Expr(&constant.value)], &mut types);
        checker.check(name, &empty, types, constant.span, false)?;
    }
    Ok(())
}
