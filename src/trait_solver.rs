//! Finite, tabled trait obligations. Ordinary impl proofs are inductive;
//! structural sharing of recursive data is coinductive. Keeping these two
//! fixed points separate prevents a circular impl from proving itself.
use crate::{
    ast::*,
    types::{
        Ty, TyCtx, TypeEnvironment, TypeError, lower_type_with, subst_ty, type_pattern_bindings,
    },
};
use std::collections::{HashMap, HashSet};

const MAX_OBLIGATIONS: usize = 4096;
const MAX_TYPE_NESTING: usize = 256;
/// Proofs need type definitions and impl candidates, not lowered functions or
/// method tables. Both environments expose the same complete proof interface.
trait ProofContext: TypeEnvironment {
    fn structure(&self, name: &str) -> Option<&StructDef>;
    fn enumeration(&self, name: &str) -> Option<&EnumDef>;
    fn trait_definition(&self, name: &str) -> Option<&TraitDef>;
    fn implementation(&self, index: usize) -> &ImplBlock;
    fn candidates(&self, name: &str, receiver: &str) -> &[usize];
}
impl ProofContext for TyCtx {
    fn structure(&self, name: &str) -> Option<&StructDef> {
        self.structs.get(name).map(|def| def.as_ref())
    }
    fn enumeration(&self, name: &str) -> Option<&EnumDef> {
        self.enums.get(name).map(|def| def.as_ref())
    }
    fn trait_definition(&self, name: &str) -> Option<&TraitDef> {
        self.traits.get(name).map(|def| def.as_ref())
    }
    fn implementation(&self, index: usize) -> &ImplBlock {
        &self.impls[index]
    }
    fn candidates(&self, name: &str, receiver: &str) -> &[usize] {
        self.trait_impls
            .get(name)
            .and_then(|index| index.get(receiver))
            .map_or(&[], Vec::as_slice)
    }
}
type ImplIndex = HashMap<String, HashMap<String, Vec<usize>>>;
struct DeclarationContext<'a> {
    globals: &'a crate::resolve::GlobalTable,
    impls: ImplIndex,
}
impl<'a> DeclarationContext<'a> {
    fn new(globals: &'a crate::resolve::GlobalTable) -> Result<Self, TypeError> {
        let mut impls: ImplIndex = HashMap::new();
        for (index, imp) in globals.impls.iter().enumerate() {
            if let Some(tr) = &imp.trait_ref {
                let receiver = crate::types::impl_receiver_base(imp, globals)?;
                impls
                    .entry(tr.path.segments.join("::"))
                    .or_default()
                    .entry(receiver)
                    .or_default()
                    .push(index);
            }
        }
        Ok(Self { globals, impls })
    }
}
impl TypeEnvironment for DeclarationContext<'_> {
    fn canonical(&self, name: &str) -> Option<String> {
        crate::type_alias::AliasView {
            globals: self.globals,
            resolved: &self.globals.resolved_aliases,
            module: "",
        }
        .canonical_name(name)
    }
    fn alias(&self, name: &str) -> Option<&crate::type_alias::ResolvedAlias> {
        self.globals
            .resolved_aliases
            .get(name)
            .map(|alias| alias.as_ref())
    }
    fn declared_nominal(&self, name: &str) -> bool {
        self.globals.structs.contains_key(name) || self.globals.enums.contains_key(name)
    }
}
impl ProofContext for DeclarationContext<'_> {
    fn structure(&self, name: &str) -> Option<&StructDef> {
        self.globals.structs.get(name)
    }
    fn enumeration(&self, name: &str) -> Option<&EnumDef> {
        self.globals.enums.get(name)
    }
    fn trait_definition(&self, name: &str) -> Option<&TraitDef> {
        self.globals.traits.get(name)
    }
    fn implementation(&self, index: usize) -> &ImplBlock {
        &self.globals.impls[index]
    }
    fn candidates(&self, name: &str, receiver: &str) -> &[usize] {
        self.impls
            .get(name)
            .and_then(|index| index.get(receiver))
            .map_or(&[], Vec::as_slice)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Goal {
    Trait(Ty, String, Vec<Ty>),
    Share(Ty),
    Slot(Ty),
}
impl Goal {
    fn has_variables(&self) -> bool {
        match self {
            Self::Trait(ty, _, args) => crate::lower::has_var(ty) || args.iter().any(crate::lower::has_var),
            Self::Share(ty) | Self::Slot(ty) => crate::lower::has_var(ty),
        }
    }
    fn description(&self) -> String {
        match self {
            Self::Trait(ty, name, args) => {
                let bound = if args.is_empty() { name.clone() } else {
                    format!("{name}<{}>", args.iter().map(crate::lower::ty_display).collect::<Vec<_>>().join(", "))
                };
                format!("{}: {bound}", crate::lower::ty_display(ty))
            }
            Self::Share(ty) | Self::Slot(ty) => format!("{}: Sync", crate::lower::ty_display(ty)),
        }
    }
    fn structural(&self) -> bool {
        !matches!(self, Self::Trait(..))
    }
}
fn bound_goal(ty: Ty, name: String, args: Vec<Ty>) -> Goal {
    if name == "Sync" && args.is_empty() {
        Goal::Share(ty)
    } else {
        Goal::Trait(ty, name, args)
    }
}
struct Node {
    goal: Goal,
    alternatives: Vec<Vec<usize>>,
}
struct Solver<'a, C: ProofContext> {
    ctx: &'a C,
    nodes: Vec<Node>,
    ids: HashMap<Goal, usize>,
    assumptions: HashSet<Goal>,
    established_impls: Option<&'a HashSet<usize>>,
    symbolic: bool,
}
impl<C: ProofContext> Solver<'_, C> {
    fn intern(&mut self, goal: Goal) -> Result<usize, TypeError> {
        // Check iteratively before recursive Ty hashing/cloning. A growing impl
        // proof must diagnose resource exhaustion rather than exhaust the stack.
        let mut types = match &goal {
            Goal::Trait(ty, _, args) => {
                let mut types = vec![(ty, 0)];
                types.extend(args.iter().map(|arg| (arg, 0)));
                types
            }
            Goal::Share(ty) | Goal::Slot(ty) => vec![(ty, 0)],
        };
        while let Some((ty, depth)) = types.pop() {
            if depth > MAX_TYPE_NESTING {
                return Err(TypeError {
                    msg: format!("trait resolution type nesting exceeds {MAX_TYPE_NESTING}"),
                    span: crate::lexer::Span::new(0, 0),
                });
            }
            match ty {
                Ty::Named { args, .. } | Ty::Tuple(args) => {
                    types.extend(args.iter().map(|ty| (ty, depth + 1)))
                }
                Ty::Array(ty, _) => types.push((ty, depth + 1)),
                Ty::Fn { params, ret } | Ty::ExternFn { params, ret } => {
                    types.extend(params.iter().map(|ty| (ty, depth + 1)));
                    types.push((ret, depth + 1));
                }
                Ty::Prim(_) | Ty::Var(_) | Ty::Infer(_) => {}
            }
        }
        if let Some(id) = self.ids.get(&goal) {
            return Ok(*id);
        }
        if self.nodes.len() == MAX_OBLIGATIONS {
            return Err(TypeError {
                msg: format!("trait resolution exceeded {MAX_OBLIGATIONS} distinct obligations"),
                span: crate::lexer::Span::new(0, 0),
            });
        }
        let id = self.nodes.len();
        self.ids.insert(goal.clone(), id);
        self.nodes.push(Node {
            goal,
            alternatives: Vec::new(),
        });
        Ok(id)
    }
    /// Close scoped assumptions under their trait declarations. These facts
    /// belong only to this proof, never to the global implementation table.
    fn assume(&mut self, assumptions: Vec<Goal>) -> Result<(), TypeError> {
        let mut pending = Vec::new();
        for goal in assumptions {
            pending.push(self.intern(goal)?);
        }
        let mut cursor = 0;
        while cursor < pending.len() {
            let goal = self.nodes[pending[cursor]].goal.clone();
            cursor += 1;
            if !self.assumptions.insert(goal.clone()) {
                continue;
            }
            let (ty, name, args) = match goal {
                Goal::Trait(ty, name, args) => (ty, name, args),
                Goal::Share(ty) => (ty, "Sync".into(), vec![]),
                Goal::Slot(_) => continue,
            };
            if let Some(def) = self.ctx.trait_definition(&name) {
                let mut subst: HashMap<_, _> = def
                    .generics
                    .params
                    .iter()
                    .map(|p| p.name.clone())
                    .zip(args)
                    .collect();
                subst.insert("Self".into(), ty.clone());
                let mut consequences = Vec::new();
                self.conditions(&def.generics, &subst, &mut consequences)?;
                let params = def
                    .generics
                    .params
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>();
                self.bounds(ty, &def.supertraits, &params, &subst, &mut consequences)?;
                for consequence in consequences {
                    pending.push(self.intern(consequence)?);
                }
            }
        }
        Ok(())
    }
    fn instantiate(
        &self,
        ty: &Type,
        params: &[String],
        subst: &HashMap<String, Ty>,
    ) -> Result<Ty, TypeError> {
        Ok(subst_ty(&lower_type_with(ty, params, self.ctx)?, subst))
    }
    fn bounds(
        &self,
        ty: Ty,
        bounds: &[TraitRef],
        params: &[String],
        subst: &HashMap<String, Ty>,
        goals: &mut Vec<Goal>,
    ) -> Result<(), TypeError> {
        for bound in bounds {
            let args = bound
                .args
                .iter()
                .map(|arg| self.instantiate(arg, params, subst))
                .collect::<Result<_, _>>()?;
            goals.push(bound_goal(ty.clone(), bound.path.segments.join("::"), args));
        }
        Ok(())
    }
    fn conditions(
        &self,
        generics: &Generics,
        subst: &HashMap<String, Ty>,
        goals: &mut Vec<Goal>,
    ) -> Result<bool, TypeError> {
        let params = generics
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>();
        for p in &generics.params {
            if p.bounds.is_empty() {
                continue;
            }
            let Some(ty) = subst.get(&p.name) else {
                return Ok(false);
            };
            self.bounds(ty.clone(), &p.bounds, &params, subst, goals)?;
        }
        for predicate in &generics.where_clauses {
            let ty = self.instantiate(&predicate.ty, &params, subst)?;
            self.bounds(ty, &predicate.bounds, &params, subst, goals)?;
        }
        Ok(true)
    }
    fn trait_rules(&self, ty: &Ty, name: &str, args: &[Ty]) -> Result<Vec<Vec<Goal>>, TypeError> {
        let mut alternatives = Vec::new();
        // Tuples compare, hash, and clone element by element.
        if matches!(name, "Eq" | "Ord" | "Hash" | "Clone") && args.is_empty() {
            match ty {
                Ty::Tuple(elems) => {
                    alternatives.push(elems.iter().map(|e| bound_goal(e.clone(), name.into(), vec![])).collect());
                    return Ok(alternatives);
                }
                Ty::Prim(crate::types::Prim::Unit) => {
                    alternatives.push(vec![]);
                    return Ok(alternatives);
                }
                _ => {}
            }
        }
        // Structural types get `Debug` from their parts (format!'s `{:?}`
        // renders them directly).
        if name == "Debug" && args.is_empty() {
            match ty {
                Ty::Tuple(elems) => {
                    alternatives.push(elems.iter().map(|e| bound_goal(e.clone(), "Debug".into(), vec![])).collect());
                    return Ok(alternatives);
                }
                Ty::Array(elem, _) => {
                    alternatives.push(vec![bound_goal((**elem).clone(), "Debug".into(), vec![])]);
                    return Ok(alternatives);
                }
                Ty::Named { name: n, args: a } if n == "Array" && a.len() == 1 => {
                    alternatives.push(vec![bound_goal(a[0].clone(), "Debug".into(), vec![])]);
                    return Ok(alternatives);
                }
                Ty::Prim(crate::types::Prim::Unit) => {
                    alternatives.push(vec![]);
                    return Ok(alternatives);
                }
                _ => {}
            }
        }
        // A trait object implements its trait and the trait's supertraits.
        if let Some(object) = crate::lower::dyn_trait(ty) {
            let mut pending = vec![object];
            let mut seen = HashSet::new();
            while let Some(t) = pending.pop() {
                if !seen.insert(t.clone()) { continue; }
                if t == name || t.rsplit("::").next() == Some(name) {
                    alternatives.push(vec![]);
                    return Ok(alternatives);
                }
                if let Some(def) = self.ctx.trait_definition(&t) {
                    pending.extend(def.supertraits.iter().map(|s| s.path.segments.join("::")));
                }
            }
        }
        let receiver = match ty {
            Ty::Named { name, .. } => name.as_str(),
            Ty::Prim(primitive) => crate::types::prim_name(*primitive),
            _ => return Ok(alternatives),
        };
        for &index in self.ctx.candidates(name, receiver) {
            let imp = self.ctx.implementation(index);
            let Some(tr) = &imp.trait_ref else {
                unreachable!("trait impl index")
            };
            if tr.path.segments.join("::") != name || tr.args.len() != args.len() {
                continue;
            }
            let params = imp
                .generics
                .params
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>();
            let mut pattern = vec![lower_type_with(&imp.self_ty, &params, self.ctx)?];
            for arg in &tr.args {
                pattern.push(lower_type_with(arg, &params, self.ctx)?);
            }
            let mut actual = vec![ty.clone()];
            actual.extend_from_slice(args);
            let Some(mut subst) = type_pattern_bindings(&Ty::Tuple(pattern), &Ty::Tuple(actual))
            else {
                continue;
            };
            subst.insert("Self".into(), ty.clone());
            let mut conditions = Vec::new();
            if !self.conditions(&imp.generics, &subst, &mut conditions)? {
                continue;
            }
            if let Some(def) = self.ctx.trait_definition(name).filter(|_| {
                !self
                    .established_impls
                    .is_some_and(|established| established.contains(&index))
            }) {
                let mut trait_subst: HashMap<_, _> = def
                    .generics
                    .params
                    .iter()
                    .map(|p| p.name.clone())
                    .zip(args.iter().cloned())
                    .collect();
                trait_subst.insert("Self".into(), ty.clone());
                if !self.conditions(&def.generics, &trait_subst, &mut conditions)? {
                    continue;
                }
                let trait_params = def
                    .generics
                    .params
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>();
                self.bounds(
                    ty.clone(),
                    &def.supertraits,
                    &trait_params,
                    &trait_subst,
                    &mut conditions,
                )?;
            }
            alternatives.push(conditions);
        }
        // Every type is `Copy` and `Send` here (see the prelude).
        if matches!(name, "Copy" | "Send") && args.is_empty() {
            alternatives.push(vec![]);
            return Ok(alternatives);
        }
        // An `Ord` type is `PartialOrd` (its `partial_cmp` comes from `cmp`).
        if name == "PartialOrd" && args.is_empty() && self.ctx.trait_definition("Ord").is_some() {
            alternatives.push(vec![bound_goal(ty.clone(), "Ord".into(), vec![])]);
        }
        // Like Rust's blanket `impl<T: Display> ToString for T`.
        if name == "ToString" && args.is_empty() && self.ctx.trait_definition("Display").is_some() {
            alternatives.push(vec![bound_goal(ty.clone(), "Display".into(), vec![])]);
        }
        Ok(alternatives)
    }
    fn structural_rules(&self, ty: &Ty, slot: bool) -> Result<Vec<Vec<Goal>>, TypeError> {
        let goal = |ty| {
            if slot {
                Goal::Slot(ty)
            } else {
                Goal::Share(ty)
            }
        };
        match ty {
            Ty::Var(_) | Ty::Infer(_) => {
                return Ok(if slot || self.symbolic {
                    vec![]
                } else {
                    vec![vec![]]
                });
            }
            Ty::Prim(_) | Ty::Fn { .. } | Ty::ExternFn { .. } => {
                return Ok(if slot { vec![] } else { vec![vec![]] });
            }
            Ty::Tuple(items) => {
                return Ok(if slot {
                    vec![]
                } else {
                    vec![items.iter().cloned().map(Goal::Share).collect()]
                });
            }
            Ty::Array(item, _) => {
                return Ok(if slot {
                    vec![]
                } else {
                    vec![vec![Goal::Share((**item).clone())]]
                });
            }
            Ty::Named { .. } => {}
        }
        let Ty::Named { name, args } = ty else {
            unreachable!()
        };
        let mut alternatives = vec![vec![Goal::Trait(ty.clone(), "Sync".into(), vec![])]];
        if matches!(name.as_str(), "Atom" | "AtomicI64") {
            alternatives.push(vec![]);
            return Ok(alternatives);
        }
        if matches!(name.as_str(), "Array" | "Vec" | "Option" | "Result") {
            alternatives.push(args.iter().cloned().map(goal).collect());
            return Ok(alternatives);
        }
        let (params, fields, is_value): (Vec<String>, Vec<&Type>, bool) =
            if let Some(def) = self.ctx.structure(name) {
                let fields = match &def.body {
                    StructBody::Named(fields) => fields.iter().map(|f| &f.ty).collect(),
                    StructBody::Tuple(fields) => fields.iter().collect(),
                    StructBody::Unit => vec![],
                };
                (
                    def.generics.params.iter().map(|p| p.name.clone()).collect(),
                    fields,
                    def.is_value,
                )
            } else if let Some(def) = self.ctx.enumeration(name) {
                let mut fields = Vec::new();
                for variant in &def.variants {
                    match &variant.payload {
                        VariantPayload::Named(fs) => fields.extend(fs.iter().map(|f| &f.ty)),
                        VariantPayload::Tuple(fs) => fields.extend(fs.iter()),
                        VariantPayload::None => {}
                    }
                }
                (
                    def.generics.params.iter().map(|p| p.name.clone()).collect(),
                    fields,
                    def.is_value,
                )
            } else {
                return Ok(alternatives);
            };
        if params.len() != args.len() {
            return Ok(alternatives);
        }
        let subst = params.iter().cloned().zip(args.iter().cloned()).collect();
        let mut conditions = Vec::new();
        for field in fields {
            let field = self.instantiate(field, &params, &subst)?;
            conditions.push(if is_value {
                Goal::Share(field)
            } else {
                Goal::Slot(field)
            });
        }
        alternatives.push(conditions);
        Ok(alternatives)
    }
    fn rules(&self, goal: &Goal) -> Result<Vec<Vec<Goal>>, TypeError> {
        if self.assumptions.contains(goal) {
            return Ok(vec![vec![]]);
        }
        match goal {
            Goal::Trait(ty, name, args) => self.trait_rules(ty, name, args),
            Goal::Share(ty) => self.structural_rules(ty, false),
            Goal::Slot(ty) => self.structural_rules(ty, true),
        }
    }
    fn solve(mut self, goals: Vec<Goal>) -> Result<Vec<bool>, TypeError> {
        let roots = goals
            .into_iter()
            .map(|goal| self.intern(goal))
            .collect::<Result<Vec<_>, _>>()?;
        self.solve_roots(roots)
    }
    fn solve_roots(mut self, roots: Vec<usize>) -> Result<Vec<bool>, TypeError> {
        let mut cursor = 0;
        while cursor < self.nodes.len() {
            let mut rules = self.rules(&self.nodes[cursor].goal)?;
            // A direct proof needs no speculative alternative expansion.
            if rules.iter().any(Vec::is_empty) {
                rules = vec![vec![]];
            }
            let mut alternatives = Vec::new();
            for rule in rules {
                let mut ids = Vec::new();
                for goal in rule {
                    ids.push(self.intern(goal)?);
                }
                alternatives.push(ids);
            }
            self.nodes[cursor].alternatives = alternatives;
            cursor += 1;
        }
        let mut proven = vec![false; self.nodes.len()];
        loop {
            // Greatest structural fixed point with ordinary trait facts held
            // fixed. Only data recursion, not a circular impl, starts true.
            for (id, node) in self.nodes.iter().enumerate() {
                if node.goal.structural() {
                    proven[id] = true;
                }
            }
            loop {
                let mut changed = false;
                for (id, node) in self.nodes.iter().enumerate() {
                    if node.goal.structural()
                        && proven[id]
                        && !node
                            .alternatives
                            .iter()
                            .any(|rule| rule.iter().all(|dep| proven[*dep]))
                    {
                        proven[id] = false;
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
            let mut changed = false;
            for (id, node) in self.nodes.iter().enumerate() {
                if !node.goal.structural()
                    && !proven[id]
                    && node
                        .alternatives
                        .iter()
                        .any(|rule| rule.iter().all(|dep| proven[*dep]))
                {
                    proven[id] = true;
                    changed = true;
                }
            }
            if !changed {
                return Ok(roots.iter().map(|root| proven[*root]).collect());
            }
        }
    }
}
pub(crate) fn prove(ctx: &TyCtx, goal: Goal) -> Result<bool, TypeError> {
    Solver {
        ctx,
        nodes: Vec::new(),
        ids: HashMap::new(),
        assumptions: HashSet::new(),
        established_impls: None,
        symbolic: false,
    }
    .solve(vec![goal])
    .map(|results| results[0])
}

/// Check impl requirements universally under their own generic assumptions.
/// Established impls have already proved their declaration requirements; their
/// applicability conditions are still checked whenever another proof uses them.
pub(crate) fn validate_impl_obligations(
    g: &crate::resolve::GlobalTable,
    trait_order: &[String],
) -> Result<(), TypeError> {
    let has_requirements = g.impls.iter().any(|imp| {
        !imp.generics.where_clauses.is_empty()
            || imp
                .trait_ref
                .as_ref()
                .and_then(|tr| g.traits.get(&tr.path.segments.join("::")))
                .is_some_and(|def| {
                    !def.supertraits.is_empty()
                        || !def.generics.where_clauses.is_empty()
                        || def.generics.params.iter().any(|p| !p.bounds.is_empty())
                })
    });
    if !has_requirements {
        return Ok(());
    }
    let ctx = DeclarationContext::new(g)?;
    let ranks: HashMap<&str, usize> = trait_order
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    let mut indices = (0..g.impls.len()).collect::<Vec<_>>();
    indices.sort_by_key(|index| {
        (
            g.impls[*index]
                .trait_ref
                .as_ref()
                .and_then(|tr| ranks.get(tr.path.segments.join("::").as_str()).copied())
                .unwrap_or(0),
            *index,
        )
    });
    let mut established = HashSet::new();
    for index in indices {
        let imp = &g.impls[index];
        let mut solver = Solver {
            ctx: &ctx,
            nodes: Vec::new(),
            ids: HashMap::new(),
            assumptions: HashSet::new(),
            established_impls: Some(&established),
            symbolic: true,
        };
        let params = imp
            .generics
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>();
        let receiver = lower_type_with(&imp.self_ty, &params, &ctx)?;
        let mut subst: HashMap<String, Ty> = params
            .iter()
            .map(|name| (name.clone(), Ty::Var(name.clone())))
            .collect();
        subst.insert("Self".into(), receiver.clone());
        let mut own_conditions = Vec::new();
        solver.conditions(&imp.generics, &subst, &mut own_conditions)?;
        let (assumptions, mut obligations): (Vec<_>, Vec<_>) =
            own_conditions.into_iter().partition(Goal::has_variables);
        if let Some(tr) = &imp.trait_ref {
            let name = tr.path.segments.join("::");
            let def = &g.traits[&name];
            let args = tr
                .args
                .iter()
                .map(|arg| lower_type_with(arg, &params, &ctx))
                .collect::<Result<Vec<_>, _>>()?;
            let mut trait_subst: HashMap<_, _> = def
                .generics
                .params
                .iter()
                .map(|p| p.name.clone())
                .zip(args)
                .collect();
            trait_subst.insert("Self".into(), receiver.clone());
            solver.conditions(&def.generics, &trait_subst, &mut obligations)?;
            let trait_params = def
                .generics
                .params
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>();
            solver.bounds(
                receiver,
                &def.supertraits,
                &trait_params,
                &trait_subst,
                &mut obligations,
            )?;
        }
        if obligations.is_empty() {
            established.insert(index);
            continue;
        }
        // A bound implies its declared supertraits and parameter conditions.
        // Scope these facts to this impl; they never become global assertions.
        let result = (|| {
            solver.assume(assumptions)?;
            solver.solve(obligations.clone())
        })()
        .map_err(|error: TypeError| TypeError {
            msg: error.msg,
            span: imp.span,
        })?;
        if let Some((goal, _)) = obligations.iter().zip(result).find(|(_, proven)| !proven) {
            let description = goal.description();
            return Err(TypeError {
                msg: format!("impl does not satisfy required trait obligation `{description}`"),
                span: imp.span,
            });
        }
        established.insert(index);
    }
    Ok(())
}

// Validate explicit alias applications before erasure, in lexical type scopes.
#[path = "type_alias_uses.rs"]
mod alias_uses;

pub(crate) fn validate_alias_obligations(g: &crate::resolve::GlobalTable) -> Result<(), TypeError> {
    if !g.aliases.values().any(|alias| alias.generics.params.iter().any(|p| !p.bounds.is_empty())
        || !alias.generics.where_clauses.is_empty()) {
        return Ok(());
    }
    alias_uses::validate(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(source: &str) -> TyCtx {
        let module = crate::parser::parse_module(&crate::lexer::lex(source).unwrap()).unwrap();
        let resolved = crate::resolve::resolve_module(module).unwrap();
        TyCtx::from_globals(&resolved.globals)
    }
    fn boxed() -> Ty {
        Ty::Named {
            name: "Boxed".into(),
            args: vec![Ty::Prim(crate::types::Prim::I64)],
        }
    }
    #[test]
    fn conditional_impls_require_inline_and_where_predicates() {
        let ctx = context(
            "trait Base {} trait Derived {} struct Boxed<T> { value: T } impl Base for i64 {} impl<T: Base> Derived for Boxed<T> {}",
        );
        assert!(
            ctx.try_type_implements_trait_args(&boxed(), "Derived", &[])
                .unwrap()
        );
        let bad = Ty::Named {
            name: "Boxed".into(),
            args: vec![Ty::Prim(crate::types::Prim::Bool)],
        };
        assert!(
            !ctx.try_type_implements_trait_args(&bad, "Derived", &[])
                .unwrap()
        );
        let ctx = context(
            "trait Base {} trait Derived {} struct Boxed<T> { value: T } impl Base for i64 {} impl<T> Derived for Boxed<T> where T: Base {}",
        );
        assert!(
            ctx.try_type_implements_trait_args(&boxed(), "Derived", &[])
                .unwrap()
        );
        assert!(
            !ctx.try_type_implements_trait_args(&bad, "Derived", &[])
                .unwrap()
        );
    }
    #[test]
    fn circular_impls_need_an_independent_base_proof() {
        let cycle = "trait First {} trait Second {} struct Boxed<T> { value: T } impl<T> First for Boxed<T> where Boxed<T>: Second {} impl<T> Second for Boxed<T> where Boxed<T>: First {}";
        let ctx = context(cycle);
        assert!(
            !ctx.try_type_implements_trait_args(&boxed(), "First", &[])
                .unwrap()
        );
        let ctx = context(&format!("{cycle} impl First for Boxed<i64> {{}}"));
        assert!(
            ctx.try_type_implements_trait_args(&boxed(), "First", &[])
                .unwrap()
        );
        assert!(
            ctx.try_type_implements_trait_args(&boxed(), "Second", &[])
                .unwrap()
        );
    }
    #[test]
    fn structural_sync_recursion_does_not_validate_circular_sync_assertions() {
        let ctx = context(
            "trait Sync {} struct Boxed<T> { value: T } impl<T> Sync for Boxed<T> where Boxed<T>: Sync {}",
        );
        assert!(!ctx.try_is_sync(&boxed()).unwrap());
        let node = Ty::Named {
            name: "Node".into(),
            args: vec![],
        };
        let ctx = context("struct Node { next: Option<Node> }");
        assert!(ctx.try_is_sync(&node).unwrap());
        let ctx = context("struct Node { next: Option<Node>, mutable: i64 }");
        assert!(!ctx.try_is_sync(&node).unwrap());
        let ctx = context("mod fake { pub struct AtomicI64 { mutable: i64 } }");
        assert!(
            !ctx.try_is_sync(&Ty::Named {
                name: "fake::AtomicI64".into(),
                args: vec![]
            })
            .unwrap()
        );
    }
    #[test]
    fn supertrait_requirements_must_have_a_proof() {
        let source = "trait Base {} trait Derived: Base {} struct Boxed<T> { value: T } impl<T> Derived for Boxed<T> {}";
        let module = crate::parser::parse_module(&crate::lexer::lex(source).unwrap()).unwrap();
        let error = crate::resolve::resolve_module(module)
            .err()
            .expect("unused missing supertrait must fail at declaration");
        assert!(
            error.msg.contains("required trait obligation"),
            "{}",
            error.msg
        );
        let ctx = context(&format!("{source} impl<T> Base for Boxed<T> {{}}"));
        assert!(
            ctx.try_type_implements_trait_args(&boxed(), "Derived", &[])
                .unwrap()
        );
    }
    #[test]
    fn expanding_type_arguments_have_a_diagnostic_instead_of_recursing_forever() {
        let ctx = context(
            "trait Marker {} struct Boxed<T> { value: T } impl<T> Marker for Boxed<T> where Boxed<Array<T>>: Marker {}",
        );
        let error = ctx
            .try_type_implements_trait_args(&boxed(), "Marker", &[])
            .unwrap_err();
        assert!(
            error.msg.contains("type nesting exceeds 256"),
            "{}",
            error.msg
        );
    }
    #[test]
    fn long_obligation_chains_are_iterative_and_exhaustion_is_diagnosed() {
        fn chain(count: usize) -> String {
            let mut source = "struct Item { value: i64 } ".to_string();
            for i in 0..count {
                if i + 1 < count {
                    source.push_str(&format!("trait T{i}: T{} {{}} ", i + 1));
                } else {
                    source.push_str(&format!("trait T{i} {{}} "));
                }
                source.push_str(&format!("impl T{i} for Item {{}} "));
            }
            source
        }
        let item = Ty::Named {
            name: "Item".into(),
            args: vec![],
        };
        let ctx = context(&chain(2000));
        assert!(
            ctx.try_type_implements_trait_args(&item, "T0", &[])
                .unwrap()
        );
        let ctx = context(&chain(MAX_OBLIGATIONS + 1));
        let error = ctx
            .try_type_implements_trait_args(&item, "T0", &[])
            .unwrap_err();
        assert!(
            error.msg.contains("4096 distinct obligations"),
            "{}",
            error.msg
        );
    }
}
