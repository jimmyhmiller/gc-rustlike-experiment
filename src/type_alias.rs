//! Transparent aliases are resolved once, before checking functions/layouts.
//! Dependency ordering is iterative: long chains and cycles never recurse
//! through alias declarations on the native compiler stack.
use crate::{
    ast::{Type, TypeKind},
    lexer::Span,
    resolve::GlobalTable,
    types::{Ty, TypeEnvironment, TypeError, lower_type_with},
};
use std::{
    collections::{BTreeSet, HashMap},
    rc::Rc,
};

#[derive(Clone, Debug)]
pub(crate) struct ResolvedAlias {
    pub params: Vec<String>,
    pub target: Ty,
    pub generics: crate::ast::Generics,
}

enum LookupFailure {
    Unknown,
    NotTrait(String),
    Ambiguous(Vec<String>),
}

pub(crate) struct AliasView<'a> {
    pub globals: &'a GlobalTable,
    pub resolved: &'a HashMap<String, Rc<ResolvedAlias>>,
    pub module: &'a str,
    /// The alias whose target this view resolves: it cannot name itself, so
    /// `type Result<T> = std::result::Result<T, E>` means the outer `Result`.
    pub defining: &'a str,
}
impl AliasView<'_> {
    pub(crate) fn canonical_name(&self, name: &str) -> Option<String> {
        if crate::types::intrinsic_type_arity(name).is_some() { return None; }
        self.canonical_declaration(name, false).ok()
    }
    pub(crate) fn canonical_trait(&self, name: &str) -> Result<String, String> {
        self.canonical_declaration(name, true).map_err(|error| match error {
            LookupFailure::Unknown => format!("unknown trait `{name}`"),
            LookupFailure::NotTrait(path) => format!("`{path}` is not a trait"),
            LookupFailure::Ambiguous(mut candidates) => {
                candidates.sort(); candidates.dedup();
                format!("ambiguous trait `{name}`: candidates {}; qualify the trait path", candidates.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", "))
            }
        })
    }
    fn canonical_declaration(&self, name: &str, trait_namespace: bool) -> Result<String, LookupFailure> {
        let is_type = |key: &str| {
            if trait_namespace { self.globals.traits.contains_key(key) } else {
                self.globals.structs.contains_key(key)
                    || self.globals.enums.contains_key(key)
                    || self.globals.aliases.contains_key(key)
            }
        };
        // Resolve the lexical declaration before checking its kind. A nearer
        // struct or alias shadows an outer trait with the same name.
        let is_declaration = |key: &str| self.globals.structs.contains_key(key)
            || self.globals.enums.contains_key(key) || self.globals.aliases.contains_key(key)
            || self.globals.traits.contains_key(key);
        let validate = |key: String| {
            if is_type(&key) { Ok(key) }
            else if trait_namespace && is_declaration(&key) { Err(LookupFailure::NotTrait(key)) }
            else { Err(LookupFailure::Unknown) }
        };
        // Resolve a definition's relative target in its own module, not in a
        // later caller's module and not by hash-map iteration order.
        let mut scope = self.module;
        loop {
            let candidate = if scope.is_empty() {
                name.to_string()
            } else {
                format!("{scope}::{name}")
            };
            if is_declaration(&candidate) && candidate != self.defining {
                return validate(candidate);
            }
            let mut parts = name.split("::");
            let head = parts.next().unwrap_or_default();
            if let Some(targets) = self
                .globals
                .scoped_use_aliases
                .get(scope)
                .and_then(|imports| imports.get(head))
            {
                let tail = parts.collect::<Vec<_>>().join("::");
                let full = |target: &String| if tail.is_empty() { target.clone() } else { format!("{target}::{tail}") };
                // An import that names no type here (an enum variant from a
                // `use Enum::*`) leaves the name to outer scopes.
                let types: Vec<String> = targets.iter().map(full).filter(|t| is_declaration(t)).collect();
                if types.len() > 1 {
                    return Err(LookupFailure::Ambiguous(types));
                }
                if let Some(imported) = types.into_iter().next() {
                    return validate(imported);
                }
                if targets.len() == 1 && !tail.is_empty() {
                    return validate(full(&targets[0]));
                }
                // This module's own import of a non-type (a variant) is what
                // the name means here; an enclosing module's is not.
                if scope == self.module {
                    return Err(LookupFailure::Unknown);
                }
            }
            if scope.is_empty() {
                break;
            }
            scope = scope.rsplit_once("::").map_or("", |(parent, _)| parent);
        }
        // An alias that names only itself is a cycle, reported as such.
        if !self.defining.is_empty() && self.defining.rsplit("::").next() == Some(name) && is_declaration(self.defining) {
            let others = self.globals.structs.keys().chain(self.globals.enums.keys()).chain(self.globals.aliases.keys()).chain(self.globals.traits.keys())
                .any(|key| key.rsplit("::").next() == Some(name) && key != self.defining);
            if !others { return validate(self.defining.to_string()); }
        }
        if name.contains("::") {
            return Err(LookupFailure::Unknown);
        }
        let mut matches = self
            .globals
            .structs
            .keys()
            .chain(self.globals.enums.keys())
            .chain(self.globals.aliases.keys())
            .chain(self.globals.traits.keys())
            .filter(|key| key.rsplit("::").next() == Some(name) && key.as_str() != self.defining)
            .collect::<BTreeSet<_>>()
            .into_iter();
        let first = matches.next().ok_or(LookupFailure::Unknown)?;
        match matches.next() {
            None => validate(first.clone()),
            Some(second) => Err(LookupFailure::Ambiguous(std::iter::once(first.clone())
                .chain(std::iter::once(second.clone())).chain(matches.cloned()).collect())),
        }
    }
}
impl TypeEnvironment for AliasView<'_> {
    fn canonical(&self, name: &str) -> Option<String> {
        self.canonical_name(name)
    }
    fn alias(&self, name: &str) -> Option<&ResolvedAlias> {
        self.resolved.get(name).map(Rc::as_ref)
    }
    fn declared_nominal(&self, name: &str) -> bool {
        self.globals.structs.contains_key(name) || self.globals.enums.contains_key(name)
    }
}

fn dependencies(
    ty: &Type,
    params: &[String],
    view: &AliasView<'_>,
) -> Result<BTreeSet<String>, TypeError> {
    let mut result = BTreeSet::new();
    let mut pending = vec![ty];
    while let Some(ty) = pending.pop() {
        match &ty.kind {
            TypeKind::Path(path, args) => {
                pending.extend(args);
                if path.is_single()
                    && (params.iter().any(|p| p == path.last())
                        || crate::resolve::is_builtin_type(path.last()))
                {
                    continue;
                }
                let written = path.segments.join("::");
                let name = view.canonical_name(&written).ok_or_else(|| TypeError {
                    msg: format!("unknown or ambiguous type `{written}` in type alias"),
                    span: ty.span,
                })?;
                if !crate::resolve::visible_from(view.globals, &name, view.module) {
                    return Err(TypeError {
                        msg: format!("type `{name}` is private"),
                        span: ty.span,
                    });
                }
                if view.globals.aliases.contains_key(&name) {
                    result.insert(name);
                }
            }
            TypeKind::Tuple(types) => pending.extend(types),
            TypeKind::Array(elem, _) => pending.push(elem),
            TypeKind::Fn(params, ret) | TypeKind::ExternFn(params, ret) => {
                pending.extend(params);
                pending.extend(ret.as_deref());
            }
            TypeKind::SelfType => {
                return Err(TypeError {
                    msg: "`Self` is not defined in a type alias".into(),
                    span: ty.span,
                });
            }
        }
    }
    Ok(result)
}

fn validate_target(
    target: &Ty,
    params: &[String],
    g: &GlobalTable,
    span: Span,
) -> Result<(), TypeError> {
    let mut pending = vec![target];
    while let Some(ty) = pending.pop() {
        match ty {
            Ty::Named { name, args } => {
                let arity = g
                    .structs
                    .get(name)
                    .map(|s| s.generics.params.len())
                    .or_else(|| g.enums.get(name).map(|e| e.generics.params.len()))
                    .or_else(|| match name.as_str() {
                        "Array" | "Vec" | "Option" => Some(1),
                        "Result" => Some(2),
                        "Bytes" => Some(0),
                        _ => None,
                    })
                    .ok_or_else(|| TypeError {
                        msg: format!("unknown type `{name}` in type alias"),
                        span,
                    })?;
                if args.len() != arity {
                    return Err(TypeError {
                        msg: format!(
                            "type `{name}` takes {arity} type argument(s), got {}",
                            args.len()
                        ),
                        span,
                    });
                }
                pending.extend(args);
            }
            Ty::Var(name) if !params.contains(name) => {
                return Err(TypeError {
                    msg: format!("unbound type parameter `{name}` in type alias"),
                    span,
                });
            }
            Ty::Tuple(types) => pending.extend(types),
            Ty::Array(elem, _) => pending.push(elem),
            Ty::Fn { params, ret } | Ty::ExternFn { params, ret } => {
                pending.extend(params);
                pending.push(ret);
            }
            Ty::Infer(_) => {
                return Err(TypeError {
                    msg: "unresolved type in type alias".into(),
                    span,
                });
            }
            Ty::Prim(_) | Ty::Var(_) => {}
        }
    }
    Ok(())
}

pub(crate) fn resolve_aliases(
    g: &GlobalTable,
) -> Result<HashMap<String, Rc<ResolvedAlias>>, TypeError> {
    let mut resolved = HashMap::new();
    let mut deps = HashMap::new();
    let mut users: HashMap<String, Vec<String>> = HashMap::new();
    let mut ready = BTreeSet::new();
    let mut counts = HashMap::new();
    let names: BTreeSet<_> = g.aliases.keys().cloned().collect();
    for name in &names {
        let alias = &g.aliases[name];
        let module = name.rsplit_once("::").map_or("", |(module, _)| module);
        if module.is_empty() && crate::resolve::is_builtin_type(&alias.name) {
            return Err(TypeError {
                msg: format!("cannot redefine builtin type `{}`", alias.name),
                span: alias.span,
            });
        }
        let params: Vec<_> = alias
            .generics
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let mut unique = BTreeSet::new();
        for param in &alias.generics.params {
            if !unique.insert(&param.name) {
                return Err(TypeError {
                    msg: format!("duplicate type parameter `{}`", param.name),
                    span: param.span,
                });
            }
        }
        let found = dependencies(
            &alias.ty,
            &params,
            &AliasView {
                globals: g,
                resolved: &resolved,
                module,
                defining: &name,
            },
        )?;
        for dependency in &found {
            users
                .entry(dependency.clone())
                .or_default()
                .push(name.clone());
        }
        counts.insert(name.clone(), found.len());
        if found.is_empty() {
            ready.insert(name.clone());
        }
        deps.insert(name.clone(), found);
    }
    while let Some(name) = ready.pop_first() {
        let alias = &g.aliases[&name];
        let params: Vec<_> = alias
            .generics
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let module = name.rsplit_once("::").map_or("", |(module, _)| module);
        let target = lower_type_with(
            &alias.ty,
            &params,
            &AliasView {
                globals: g,
                resolved: &resolved,
                module,
                defining: &name,
            },
        )?;
        validate_target(&target, &params, g, alias.ty.span)?;
        resolved.insert(name.clone(), Rc::new(ResolvedAlias { params, target, generics: alias.generics.clone() }));
        for user in users.get(&name).into_iter().flatten() {
            let count = counts.get_mut(user).expect("collected alias user");
            *count -= 1;
            if *count == 0 {
                ready.insert(user.clone());
            }
        }
    }
    if resolved.len() != names.len() {
        let mut current = names
            .iter()
            .find(|name| !resolved.contains_key(*name))
            .unwrap()
            .clone();
        let mut path = Vec::new();
        let mut positions = HashMap::new();
        loop {
            if let Some(&start) = positions.get(&current) {
                let mut cycle = path[start..].to_vec();
                cycle.push(current.clone());
                return Err(TypeError {
                    msg: format!("cyclic type aliases: {}", cycle.join(" -> ")),
                    span: g.aliases[&current].ty.span,
                });
            }
            positions.insert(current.clone(), path.len());
            path.push(current.clone());
            current = deps[&current]
                .iter()
                .find(|next| !resolved.contains_key(*next))
                .expect("unresolved dependency in cyclic graph")
                .clone();
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        lexer::lex,
        parser::parse_module,
        resolve::resolve_module,
        types::{Prim, TyCtx, lower_type},
    };
    fn resolve(source: &str) -> Result<crate::resolve::Resolved, crate::resolve::ResolveError> {
        resolve_module(parse_module(&lex(source).unwrap()).unwrap())
    }
    #[test]
    fn aliases_in_module_signatures_and_body_annotations_keep_their_own_scope() {
        let source = "mod left { pub type Item = i64; pub fn id(value: Item) -> Item { value } } mod right { pub type Item = bool; pub fn id(value: Item) -> Item { value } } fn main() -> i64 { let flag: right::Item = true; if right::id(flag) { left::id(42) } else { 0 } }";
        let r = resolve(source).unwrap();
        crate::lower::check_program(&r.globals).unwrap();
        crate::lower::lower_program(&r.globals).unwrap();
    }
    #[test]
    fn aliases_imports_are_scoped_instead_of_chosen_by_collection_order() {
        let source = "mod one { pub type Item = i64; } mod two { pub type Item = bool; } mod left { use one::Item; pub fn id(value: Item) -> Item { value } } mod right { use two::Item; pub fn id(value: Item) -> Item { value } } fn main() -> i64 { if right::id(true) { left::id(42) } else { 0 } }";
        let r = resolve(source).unwrap();
        crate::lower::check_program(&r.globals).unwrap();
        crate::lower::lower_program(&r.globals).unwrap();
    }
    #[test]
    fn aliases_do_not_rewrite_local_value_names_or_capture_generic_parameters() {
        let source =
            "type words = Array<i64>; fn main() -> i64 { let words = |n: i64| n + 1; words(41) }";
        let r = resolve(source).unwrap();
        crate::lower::lower_program(&r.globals).unwrap();
    }
    #[test]
    fn qualified_function_calls_do_not_fall_back_to_an_unrelated_short_name() {
        let source = "mod left { pub fn id(value: i64) -> i64 { value } } fn main() -> i64 { missing::id(42) }";
        let r = resolve(source).unwrap();
        let error = crate::lower::lower_program(&r.globals).err().expect("unknown qualified function must fail");
        assert!(error.msg.contains("unknown function"), "{}", error.msg);
    }
    #[test]
    fn ambiguous_unqualified_function_calls_are_rejected() {
        let source = "mod left { pub fn id(value: i64) -> i64 { value } } mod right { pub fn id(value: i64) -> i64 { value } } fn main() -> i64 { id(42) }";
        let r = resolve(source).unwrap();
        assert!(crate::lower::lower_program(&r.globals).is_err());
    }
    #[test]
    fn alias_parameter_names_do_not_capture_enclosing_function_parameters() {
        let source = "type pair<T> = (T, i64); fn make<T>(value: T) -> pair<T> { (value, 42) } fn main() -> i64 { let result: pair<bool> = make(true); 42 }";
        let r = resolve(source).unwrap();
        crate::lower::lower_program(&r.globals).unwrap();
    }
    #[test]
    fn record_alias_constructors_preserve_nominal_identity_and_infer_arguments() {
        for source in [
            "struct Boxed<T> { value: T } type Integer = Boxed<i64>; fn main() -> i64 { let b = Integer { value: 42 }; b.value }",
            "struct Boxed<T> { value: T } type Wrapped<U> = Boxed<U>; fn main() -> i64 { let b = Wrapped { value: 42 }; b.value }",
            "struct Boxed<T> { value: T } type Wrapped<U> = Boxed<U>; fn make<T>(value: T) -> Wrapped<T> { Wrapped { value } } fn main() -> i64 { let b = make(42); b.value }",
            "struct Boxed<T> { value: T } type Words = Boxed<Array<i64>>; fn main() -> i64 { let b = Words { value: [1, 2, 3] }; array_len(b.value) }",
            "struct Empty<T> {} type Integer = Empty<i64>; fn main() -> i64 { let value = Integer {}; 42 }",
            "struct Boxed<T> { value: T } fn make<T>(unused: T) -> Boxed<i64> { Boxed { value: 42 } } fn main() -> i64 { let b = make(true); b.value }",
        ] {
            let r = resolve(source).unwrap();
            crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
        }
    }
    #[test]
    fn record_alias_specializations_reject_incompatible_fields_and_contexts() {
        for source in [
            "struct Boxed<T> { value: T } type Integer = Boxed<i64>; fn main() -> i64 { let b = Integer { value: true }; 0 }",
            "struct Boxed<T> { value: T } type Integer = Boxed<i64>; fn main() -> i64 { let b: Boxed<bool> = Integer { value: true }; 0 }",
            "struct Pair<A, B> { left: A, right: B } type Same<T> = Pair<T, T>; fn main() -> i64 { let p = Same { left: 42, right: true }; 0 }",
        ] {
            let r = resolve(source).unwrap();
            assert!(crate::lower::lower_program(&r.globals).is_err(), "{source}");
        }
    }
    #[test]
    fn enum_alias_constructors_and_patterns_preserve_specialization() {
        for source in [
            "enum Choice<T> { Value(T), Empty } type Integer = Choice<i64>; fn main() -> i64 { let c = Integer::Value(42); match c { Integer::Value(n) => n, Integer::Empty => 0 } }",
            "enum Choice<T> { Value(T), Empty } type Wrapped<U> = Choice<U>; fn main() -> i64 { let c = Wrapped::Value(42); match c { Wrapped::Value(n) => n, Wrapped::Empty => 0 } }",
            "enum Choice<T> { Value { value: T }, Empty } type Integer = Choice<i64>; fn main() -> i64 { let c = Integer::Value { value: 42 }; match c { Integer::Value { value } => value, Integer::Empty => 0 } }",
            "enum Choice<T> { Value(T), Empty } type Integer = Choice<i64>; fn main() -> i64 { let c = Integer::Empty; match c { Integer::Value(n) => n, Integer::Empty => 42 } }",
            "struct Boxed<T> { value: T } type Integer = Boxed<i64>; fn main() -> i64 { let c = Integer { value: 42 }; match c { Integer { value } => value } }",
        ] {
            let r = resolve(source).unwrap();
            crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
        }
    }
    #[test]
    fn alias_patterns_and_enum_constructors_reject_wrong_specializations() {
        for source in [
            "enum Choice<T> { Value(T) } type Integer = Choice<i64>; fn main() -> i64 { let c = Integer::Value(true); 0 }",
            "enum Choice<T> { Value(T) } type Integer = Choice<i64>; fn main() -> i64 { let c = Choice::Value(true); match c { Integer::Value(n) => 0 } }",
            "struct Boxed<T> { value: T } type Integer = Boxed<i64>; fn main() -> i64 { let c = Boxed { value: true }; match c { Integer { value } => 0 } }",
            "enum First { Value(i64) } enum Second { Value(i64) } fn main() -> i64 { let c = First::Value(42); match c { Second::Value(n) => n } }",
        ] {
            let r = resolve(source).unwrap();
            assert!(crate::lower::lower_program(&r.globals).is_err(), "{source}");
        }
    }
    #[test]
    fn alias_construction_executes_with_normal_and_stressed_collection() {
        for source in [
            "type Count = i64; struct Counter { value: i64 } trait Read { fn read(self) -> i64; } impl Read for Counter { fn read(self) -> Count { self.value } } fn main() -> i64 { Counter { value: 42 }.read() }",
            "struct Boxed<T> { value: T } type Integer = Boxed<i64>; fn main() -> i64 { let b = Integer { value: 42 }; match b { Integer { value } => value } }",
            "enum Choice<T> { Value(T), Empty } type Wrapped<U> = Choice<U>; fn main() -> i64 { let c = Wrapped::Value(42); match c { Wrapped::Value(n) => n, Wrapped::Empty => 0 } }",
            "enum Choice<T> { Value { value: T }, Empty } type Integer = Choice<i64>; fn main() -> i64 { let c = Integer::Value { value: 42 }; match c { Integer::Value { value } => value, Integer::Empty => 0 } }",
            "struct Boxed<T> { value: T } type Words = Boxed<Array<i64>>; fn main() -> i64 { let b = Words { value: [40, 2] }; b.value[0] + b.value[1] }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap();
            for stress in [false, true] {
                let actual = unsafe { crate::codegen::jit_run_i64_gc(&program, stress) }.unwrap();
                assert_eq!(actual, 42, "stress={stress}: {source}");
            }
        }
    }
    #[test]
    fn trait_signatures_compare_normalized_aliases_and_alpha_renamed_binders() {
        for source in [
            "type Count = i64; struct Counter { value: i64 } trait Read { fn read(self) -> i64; } impl Read for Counter { fn read(self) -> Count { self.value } }",
            "type Count = i64; struct Counter { value: i64 } trait Read { fn read(self) -> Count; } impl Read for Counter { fn read(self) -> i64 { self.value } }",
            "type Identity<T> = T; struct Holder<T> { value: T } trait Read<T> { fn read(self) -> Identity<T>; } impl<U> Read<U> for Holder<U> { fn read(self) -> U { self.value } }",
            "struct Holder<T> { value: T } trait Convert<T> { fn apply<U>(self, value: U) -> (T, U); } impl<U> Convert<U> for Holder<U> { fn apply<T>(self, value: T) -> (U, T) { (self.value, value) } }",
            "type Three = [i64; 3]; struct Counter { value: i64 } trait Read { fn read(self, value: Three) -> i64; } impl Read for Counter { fn read(self, value: [i64; 3]) -> i64 { self.value } }",
        ] {
            resolve(source).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
        }
    }
    #[test]
    fn trait_signatures_reject_different_array_lengths_and_nominal_modules() {
        for source in [
            "struct Counter { value: i64 } trait Read { fn read(self, value: [i64; 3]) -> i64; } impl Read for Counter { fn read(self, value: [i64; 4]) -> i64 { self.value } }",
            "mod left { pub struct Item { value: i64 } } mod right { pub struct Item { value: i64 } } struct Counter { value: i64 } trait Read { fn read(self, value: left::Item) -> i64; } impl Read for Counter { fn read(self, value: right::Item) -> i64 { self.value } }",
        ] {
            let error = resolve(source).err().expect("incompatible trait signature must fail");
            assert!(error.msg.contains("does not match trait"), "{}", error.msg);
        }
    }
    #[test]
    fn alias_impls_and_associated_calls_use_underlying_receiver_identity() {
        for source in [
            "struct Boxed<T> { value: T } type Integer = Boxed<i64>; impl Integer { fn get(self) -> i64 { self.value } } fn main() -> i64 { Boxed { value: 42 }.get() }",
            "struct Boxed<T> { value: T } impl<T> Boxed<T> { fn make(value: T) -> Self { Boxed { value } } fn get(self) -> T { self.value } } type Integer = Boxed<i64>; fn main() -> i64 { Integer::make(42).get() }",
            "struct Boxed<T> { value: T } impl<T> Boxed<T> { fn make(value: T) -> Self { Boxed { value } } } type Wrapped<U> = Boxed<U>; fn main() -> i64 { Wrapped::make(42).value }",
            "mod left { pub struct Item { value: i64 } impl Item { fn get(self) -> i64 { self.value } fn make() -> Self { Item { value: 40 } } } } mod right { pub struct Item { value: i64 } impl Item { fn get(self) -> i64 { self.value } fn make() -> Self { Item { value: 2 } } } } fn main() -> i64 { left::Item::make().get() + right::Item::make().get() }",
        ] {
            let r = resolve(source).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42, "{source}");
        }
    }
    #[test]
    fn alias_methods_reject_duplicate_identity_and_wrong_receiver_specialization() {
        let duplicate = "struct Boxed { value: i64 } type Alias = Boxed; impl Boxed { fn get(self) -> i64 { self.value } } impl Alias { fn get(self) -> i64 { self.value } }";
        assert!(resolve(duplicate).err().unwrap().msg.contains("duplicate method"));
        for source in [
            "struct Boxed<T> { value: T } impl Boxed<i64> { fn get(self) -> i64 { 42 } } fn main() -> i64 { Boxed { value: true }.get() }",
            "struct Boxed<T> { value: T } type Integer = Boxed<i64>; impl Integer { fn get(self) -> i64 { self.value } } fn main() -> i64 { Boxed { value: true }.get() }",
            "struct Boxed<T> { value: T } impl<T> Boxed<T> { fn make(value: T) -> Self { Boxed { value } } } type Integer = Boxed<i64>; fn main() -> i64 { let b = Integer::make(true); 0 }",
        ] {
            let r = resolve(source).unwrap();
            assert!(crate::lower::lower_program(&r.globals).is_err(), "{source}");
        }
    }
    #[test]
    fn trait_bounds_match_complete_receiver_patterns_and_alias_targets() {
        for source in [
            "trait Marker {} struct Boxed<T> { value: T } type Integer = Boxed<i64>; impl Marker for Integer {} fn accept<T: Marker>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: 1 }) }",
            "trait Marker {} struct Pair<A, B> { left: A, right: B } impl<T> Marker for Pair<T, T> {} fn accept<T: Marker>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Pair { left: true, right: false }) }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        }
        for source in [
            "trait Marker {} struct Boxed<T> { value: T } impl Marker for Boxed<i64> {} fn accept<T: Marker>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: true }) }",
            "trait Marker {} struct Pair<A, B> { left: A, right: B } impl<T> Marker for Pair<T, T> {} fn accept<T: Marker>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Pair { left: 1, right: false }) }",
            "trait Marker {} mod left { pub struct Item { value: i64 } } mod right { pub struct Item { value: i64 } } impl Marker for left::Item {} fn accept<T: Marker>(value: T) -> i64 { 42 } fn main() -> i64 { accept(right::Item { value: 1 }) }",
            "trait Marker {} struct Boxed<T> { value: T } impl Marker for Boxed<[i64; 3]> {} fn accept<T: Marker>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: [1, 2] }) }",
        ] {
            let r = resolve(source).unwrap();
            let error = crate::lower::lower_program(&r.globals).err().expect("incompatible trait bound must fail");
            assert!(error.msg.contains("trait bound"), "{source}: {}", error.msg);
        }
    }
    #[test]
    fn trait_names_keep_definition_scope_and_qualified_identity() {
        for source in [
            "mod left { pub trait Marker {} pub struct Item { value: i64 } impl Marker for Item {} pub fn accept<T: Marker>(value: T) -> i64 { 40 } } mod right { pub trait Marker {} pub struct Item { value: i64 } impl Marker for Item {} pub fn accept<T: Marker>(value: T) -> i64 { 2 } } fn main() -> i64 { left::accept(left::Item { value: 1 }) + right::accept(right::Item { value: 1 }) }",
            "mod left { pub trait Read { fn read(self) -> i64; } } mod right { pub trait Read { fn read(self) -> bool; } } struct First { value: i64 } struct Second { value: bool } impl left::Read for First { fn read(self) -> i64 { self.value } } impl right::Read for Second { fn read(self) -> bool { self.value } } fn main() -> i64 { if (Second { value: true }).read() { First { value: 42 }.read() } else { 0 } }",
            "mod left { pub trait Marker {} } mod consumer { use left::Marker; pub struct Item { value: i64 } impl Marker for Item {} pub fn accept<T: Marker>(value: T) -> i64 { 42 } } fn main() -> i64 { consumer::accept(consumer::Item { value: 1 }) }",
        ] {
            let r = resolve(source).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        }
        let source = "mod left { pub trait Marker {} } mod right { pub trait Marker {} } struct Item { value: i64 } impl left::Marker for Item {} fn accept<T: right::Marker>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Item { value: 1 }) }";
        let r = resolve(source).unwrap();
        assert!(crate::lower::lower_program(&r.globals).err().unwrap().msg.contains("trait bound"));
    }
    #[test]
    fn trait_resolution_rejects_unknown_ambiguous_private_and_wrong_arity_names() {
        for source in [
            "trait Marker {} struct Item { value: i64 } impl missing::Marker for Item {}",
            "mod left { pub trait Marker {} } mod right { pub trait Marker {} } fn accept<T: Marker>(value: T) {}",
            "mod left { trait Marker {} } mod right { pub fn accept<T: left::Marker>(value: T) {} }",
            "trait Marker<T> {} fn accept<T: Marker>(value: T) {}",
            "mod left { pub trait Marker {} } mod right { pub trait Marker {} } mod consumer { use left::Marker; use right::Marker; pub fn accept<T: Marker>(value: T) {} }",
        ] {
            assert!(resolve(source).is_err(), "{source}");
        }
    }
    #[test]
    fn trait_bound_arguments_share_receiver_generic_constraints() {
        for source in [
            "trait Marker<T> {} struct Boxed<T> { value: T } impl<T> Marker<T> for Boxed<T> {} fn accept<T, U: Marker<T>>(value: U, other: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: true }, false) }",
            "trait Marker<T> {} struct Item { value: i64 } type Count = i64; impl Marker<Count> for Item {} fn accept<T: Marker<i64>>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Item { value: 1 }) }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        }
        for source in [
            "trait Marker<T> {} struct Item { value: i64 } impl Marker<i64> for Item {} fn accept<T: Marker<bool>>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Item { value: 1 }) }",
            "trait Marker<T> {} struct Boxed<T> { value: T } impl<T> Marker<T> for Boxed<T> {} fn accept<T: Marker<i64>>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: true }) }",
            "trait Marker<T> {} struct Item { value: i64 } impl<T> Marker<(T, T)> for Item {} fn accept<T: Marker<(i64, bool)>>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Item { value: 1 }) }",
        ] {
            let r = resolve(source).unwrap();
            assert!(crate::lower::lower_program(&r.globals).err().unwrap().msg.contains("trait bound"), "{source}");
        }
    }
    #[test]
    fn where_clauses_are_enforced_on_free_and_associated_calls() {
        for source in [
            "trait Marker {} impl Marker for i64 {} fn accept<T>(value: T) -> i64 where T: Marker { 42 } fn main() -> i64 { accept(1) }",
            "trait Marker<T> {} struct Boxed<T> { value: T } impl<T> Marker<T> for Boxed<T> {} fn accept<T, U>(value: U, other: T) -> i64 where U: Marker<T> { 42 } fn main() -> i64 { accept(Boxed { value: true }, false) }",
            "trait Marker {} impl Marker for i64 {} struct Factory {} impl Factory { fn make<T>(value: T) -> i64 where T: Marker { 42 } } fn main() -> i64 { Factory::make(1) }",
            "trait Marker {} impl Marker for i64 {} struct Factory {} impl Factory { fn make<T>(self, value: T) -> i64 where T: Marker { 42 } } fn main() -> i64 { Factory {}.make(1) }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        }
        for source in [
            "trait Marker {} impl Marker for i64 {} fn accept<T>(value: T) -> i64 where T: Marker { 42 } fn main() -> i64 { accept(true) }",
            "trait Marker<T> {} struct Boxed<T> { value: T } impl<T> Marker<T> for Boxed<T> {} fn accept<T, U>(value: U, other: T) -> i64 where U: Marker<T> { 42 } fn main() -> i64 { accept(Boxed { value: true }, 1) }",
            "trait Marker {} impl Marker for i64 {} struct Factory {} impl Factory { fn make<T>(value: T) -> i64 where T: Marker { 42 } } fn main() -> i64 { Factory::make(true) }",
            "trait Marker {} impl Marker for i64 {} struct Factory {} impl Factory { fn make<T>(self, value: T) -> i64 where T: Marker { 42 } } fn main() -> i64 { Factory {}.make(true) }",
        ] {
            let r = resolve(source).unwrap();
            assert!(crate::lower::lower_program(&r.globals).err().unwrap().msg.contains("trait bound"), "{source}");
        }
    }
    #[test]
    fn impl_constraints_apply_to_instance_associated_and_default_methods() {
        for source in [
            "trait Marker {} impl Marker for i64 {} struct Boxed<T> { value: T } impl<T: Marker> Boxed<T> { fn answer(self) -> i64 { 42 } } fn main() -> i64 { Boxed { value: 1 }.answer() }",
            "trait Marker {} impl Marker for i64 {} struct Boxed<T> { value: T } impl<T> Boxed<T> where T: Marker { fn make(value: T) -> Self { Boxed { value } } } fn main() -> i64 { Boxed::make(42).value }",
            "trait Marker {} impl Marker for i64 {} trait Answer { fn answer(self) -> i64 { 42 } } struct Boxed<T> { value: T } impl<T: Marker> Answer for Boxed<T> {} fn main() -> i64 { Boxed { value: 1 }.answer() }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        }
        for source in [
            "trait Marker {} impl Marker for i64 {} struct Boxed<T> { value: T } impl<T> Boxed<T> where T: Marker { fn answer(value: T) -> i64 { 42 } } fn main() -> i64 { Boxed::answer(true) }",
            "trait Marker {} impl Marker for i64 {} struct Boxed<T> { value: T } impl<T: Marker> Boxed<T> { fn answer(self) -> i64 { 42 } } fn main() -> i64 { Boxed { value: true }.answer() }",
            "trait Marker {} impl Marker for i64 {} struct Boxed<T> { value: T } impl<T> Boxed<T> where T: Marker { fn make(value: T) -> Self { Boxed { value } } } fn main() -> i64 { let value = Boxed::make(true); 0 }",
            "trait Marker {} impl Marker for i64 {} trait Answer { fn answer(self) -> i64 { 42 } } struct Boxed<T> { value: T } impl<T: Marker> Answer for Boxed<T> {} fn main() -> i64 { Boxed { value: true }.answer() }",
        ] {
            let r = resolve(source).unwrap();
            assert!(crate::lower::lower_program(&r.globals).err().unwrap().msg.contains("trait bound"), "{source}");
        }
    }
    #[test]
    fn conditional_trait_obligations_are_checked_at_actual_calls() {
        for source in [
            "trait Base {} trait Derived {} impl Base for i64 {} struct Boxed<T> { value: T } impl<T: Base> Derived for Boxed<T> {} fn accept<T: Derived>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: 1 }) }",
            "trait Base {} trait Derived {} impl Base for i64 {} struct Boxed<T> { value: T } impl<T> Derived for Boxed<T> where T: Base {} fn accept<T: Derived>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: 1 }) }",
            "trait Base {} trait Derived: Base {} struct Item { value: i64 } impl Base for Item {} impl Derived for Item {} fn accept<T: Derived>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Item { value: 1 }) }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        }
        for source in [
            "trait Base {} trait Derived {} impl Base for i64 {} struct Boxed<T> { value: T } impl<T: Base> Derived for Boxed<T> {} fn accept<T: Derived>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: true }) }",
            "trait Base {} trait Derived {} impl Base for i64 {} struct Boxed<T> { value: T } impl<T> Derived for Boxed<T> where T: Base {} fn accept<T: Derived>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Boxed { value: true }) }",
        ] {
            let r = resolve(source).unwrap();
            let error = crate::lower::lower_program(&r.globals).err().unwrap();
            assert!(error.msg.contains("trait bound"), "{source}: {}", error.msg);
        }
        for source in [
            "trait First {} trait Second {} struct Item { value: i64 } impl First for Item where Item: Second {} impl Second for Item where Item: First {} fn accept<T: First>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Item { value: 1 }) }",
            "trait Base {} trait Derived: Base {} struct Item { value: i64 } impl Derived for Item {} fn accept<T: Derived>(value: T) -> i64 { 42 } fn main() -> i64 { accept(Item { value: 1 }) }",
        ] {
            let error = resolve(source).err().expect("invalid concrete impl must fail before calls");
            assert!(error.msg.contains("required trait obligation"), "{}", error.msg);
        }
    }
    #[test]
    fn spawn_capture_checks_preserve_solver_exhaustion_diagnostics() {
        let source = "trait Sync {} struct Boxed<T> { value: T } impl<T> Sync for Boxed<T> where Boxed<Array<T>>: Sync {} struct Thread {} impl Thread { fn spawn(f: fn() -> i64) -> i64 { 0 } } fn main() -> i64 { let boxed = Boxed { value: 1 }; Thread::spawn(|| boxed.value) }";
        let r = resolve(source).unwrap();
        let error = crate::lower::lower_program(&r.globals).err().expect("expanding obligation must fail");
        assert!(error.msg.contains("trait resolution type nesting exceeds 256"), "{}", error.msg);
        assert_eq!(error.span.start as usize, source.find("||").unwrap());
        assert!(error.span.end > error.span.start);
    }
    #[test]
    fn impl_parameters_must_survive_alias_normalization_in_the_header() {
        for source in [
            "trait Marker {} impl<T> Marker for i64 {}",
            "trait Marker {} impl<T: Marker> Marker for i64 {}",
            "trait Marker {} type Erased<T> = i64; impl<T> Marker for Erased<T> {}",
            "struct Item { value: i64 } impl<T> Item { fn answer(self, value: T) -> i64 { 42 } }",
        ] {
            let error = resolve(source).err().expect("unconstrained impl parameter must fail");
            assert!(error.msg.contains("unconstrained impl type parameter"), "{source}: {}", error.msg);
        }
        for source in [
            "trait Marker<T> {} impl<T> Marker<T> for i64 {}",
            "trait Marker {} struct Boxed<T> { value: T } type Wrapped<U> = Boxed<U>; impl<T> Marker for Wrapped<T> {}",
        ] { resolve(source).unwrap(); }
    }
    #[test]
    fn duplicate_generic_binders_are_rejected_at_their_declarations() {
        for source in [
            "fn identity<T, T>(value: T) -> T { value }",
            "struct Pair<T, T> { value: T }",
            "trait Marker<T, T> {}",
            "struct Item { value: i64 } impl<T, T> Item {}",
        ] {
            assert!(resolve(source).err().unwrap().msg.contains("duplicate type parameter"), "{source}");
        }
    }
    #[test]
    fn tuple_context_preserves_nested_alias_array_representations() {
        for source in [
            "type Batch = (Array<i64>, i64); fn main() -> i64 { let b: Batch = ([40, 2], 0); b.0[0] + b.0[1] }",
            "type Nested = ((Array<i64>, i64), Array<i64>); fn main() -> i64 { let b: Nested = (([40], 0), [2]); b.0.0[0] + b.1[0] }",
            "struct Boxed<T> { value: T } type Batch = Boxed<(Array<i64>, i64)>; fn main() -> i64 { let b = Batch { value: ([40, 2], 0) }; b.value.0[0] + b.value.0[1] }",
            "type Pair<T> = (T, Array<i64>); fn make<T>(value: T) -> Pair<T> { (value, [40, 2]) } fn main() -> i64 { let b = make(true); b.1[0] + b.1[1] }",
            "type Fixed = ([i64; 2], i64); fn main() -> i64 { let b: Fixed = ([40, 2], 0); b.0[0] + b.0[1] }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            for stress in [false, true] {
                assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 42, "{source}");
            }
        }
        for source in [
            "type Fixed = ([i64; 3], i64); fn main() -> i64 { let b: Fixed = ([40, 2], 0); 0 }",
            "type Pair = (i64, bool); fn main() -> i64 { let b: Pair = (1, 2); 0 }",
            "type Pair = (i64, bool); fn main() -> i64 { let b: Pair = (1, true, 2); 0 }",
        ] {
            let r = resolve(source).unwrap();
            assert!(crate::lower::lower_program(&r.globals).is_err(), "{source}");
        }
    }
    #[test]
    fn tuple_field_indices_cannot_truncate_or_use_numeric_suffixes() {
        for source in [
            "fn main() -> i64 { let pair = (42, true); pair.4294967296 }",
            "fn main() -> i64 { let pair = (42, true); pair.0i64 }",
        ] {
            let tokens = lex(source).unwrap();
            assert!(parse_module(&tokens).is_err(), "{source}");
        }
    }
    #[test]
    fn intrinsic_arrays_and_qualified_user_arrays_keep_distinct_identity() {
        let source = "mod custom { pub struct Array<T> { value: T } } type Builtin = Array<i64>; type User = custom::Array<i64>; fn main() -> i64 { let mut values: Builtin = Array::new(2); values[0] = 40; values[1] = 2; let ordinary = User { value: 0 }; values[0] + values[1] + ordinary.value }";
        let r = resolve(source).unwrap();
        assert!(matches!(&r.globals.resolved_aliases["Builtin"].target, Ty::Named { name, .. } if name == "Array"));
        assert!(matches!(&r.globals.resolved_aliases["User"].target, Ty::Named { name, .. } if name == "custom::Array"));
        let program = crate::lower::lower_program(&r.globals).unwrap();
        assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
    }
    #[test]
    fn method_generic_binders_shadow_impl_binders_without_changing_impl_conditions() {
        for source in [
            "struct Boxed<T> { value: T } impl<T> Boxed<T> { fn identity<T>(self, value: T) -> T { value } } fn main() -> i64 { Boxed { value: true }.identity(42) }",
            "trait Marker {} impl Marker for bool {} struct Boxed<T> { value: T } impl<T: Marker> Boxed<T> { fn identity<T>(self, value: T) -> T { value } } fn main() -> i64 { Boxed { value: true }.identity(42) }",
            "trait Marker {} impl Marker for i64 {} struct Boxed<T> { value: T } impl<T> Boxed<T> { fn identity<T: Marker>(self, value: T) -> T { value } } fn main() -> i64 { Boxed { value: true }.identity(42) }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        }
        for source in [
            "trait Marker {} impl Marker for i64 {} struct Boxed<T> { value: T } impl<T: Marker> Boxed<T> { fn identity<T>(self, value: T) -> T { value } } fn main() -> i64 { Boxed { value: true }.identity(42) }",
            "trait Marker {} impl Marker for bool {} struct Boxed<T> { value: T } impl<T> Boxed<T> { fn identity<T: Marker>(self, value: T) -> T { value } } fn main() -> i64 { Boxed { value: true }.identity(42) }",
        ] {
            let r = resolve(source).unwrap();
            let error = crate::lower::lower_program(&r.globals).err().unwrap();
            assert!(error.msg.contains("trait bound"), "{source}: {}", error.msg);
        }
    }
    #[test]
    fn associated_function_binders_do_not_capture_receiver_parameters() {
        for source in [
            "struct Boxed<T> { value: T } impl<T> Boxed<T> { fn identity<T>(value: T) -> T { value } } type Boolean = Boxed<bool>; fn main() -> i64 { Boolean::identity(42) }",
            "struct Boxed<T> { value: T } impl<T> Boxed<T> { fn pair<T>(value: T, receiver: Self) -> (Self, T) { (receiver, value) } } fn main() -> i64 { let pair = Boxed::pair(42, Boxed { value: true }); pair.1 }",
            "trait Receiver {} trait Argument {} impl Receiver for bool {} impl Argument for i64 {} struct Boxed<T> { value: T } impl<T: Receiver> Boxed<T> { fn identity<T: Argument>(value: T) -> T { value } } type Boolean = Boxed<bool>; fn main() -> i64 { Boolean::identity(42) }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        }
        for source in [
            "trait Marker {} impl Marker for i64 {} struct Boxed<T> { value: T } impl<T: Marker> Boxed<T> { fn identity<T>(value: T) -> T { value } } type Boolean = Boxed<bool>; fn main() -> i64 { Boolean::identity(42) }",
            "trait Marker {} impl Marker for bool {} struct Boxed<T> { value: T } impl<T> Boxed<T> { fn identity<T: Marker>(value: T) -> T { value } } type Boolean = Boxed<bool>; fn main() -> i64 { Boolean::identity(42) }",
        ] {
            let r = resolve(source).unwrap();
            let error = crate::lower::lower_program(&r.globals).err().unwrap();
            assert!(error.msg.contains("trait bound"), "{source}: {}", error.msg);
        }
    }
    #[test]
    fn compound_alias_constructors_infer_elements_without_losing_array_shape() {
        for source in [
            r#"struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<Array<T>>>; fn main() -> i64 { let rows = Rows { value: [["forty"], ["two"]] }; str_len(rows.value[0][0]) + str_len(rows.value[1][0]) + 34 }"#,
            "struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<T>>; fn main() -> i64 { let rows = Rows { value: [40, 2] }; rows.value[0] + rows.value[1] }",
            "struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<Array<T>>>; fn main() -> i64 { let rows = Rows { value: [[40], [2]] }; rows.value[0][0] + rows.value[1][0] }",
            "struct Boxed<T> { value: T } type Batch<T> = Boxed<(T, Array<T>)>; fn main() -> i64 { let batch = Batch { value: (42, []) }; batch.value.0 + array_len(batch.value.1) }",
            "enum Choice<T> { Values(Array<T>) } type Rows<T> = Choice<T>; fn main() -> i64 { let rows = Rows::Values([40, 2]); match rows { Rows::Values(values) => values[0] + values[1] } }",
            "struct Boxed<T> { value: T } type Fixed<T> = Boxed<[T; 2]>; fn main() -> i64 { let rows = Fixed { value: [40, 2] }; rows.value[0] + rows.value[1] }",
            "struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<T>>; fn main() -> i64 { let rows = Rows { value: [42; 1] }; rows.value[0] }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            for stress in [false, true] {
                assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 42, "{source}");
            }
        }
        for source in [
            "struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<Array<T>>>; fn main() -> i64 { let rows = Rows { value: [[40], [true]] }; 0 }",
            "struct Boxed<T> { value: T } type Batch<T> = Boxed<(T, Array<T>)>; fn main() -> i64 { let batch = Batch { value: (42, [true]) }; 0 }",
            "struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<T>>; fn main() -> i64 { let rows = Rows { value: [] }; 0 }",
            "struct Boxed<T> { value: T } type Fixed<T> = Boxed<[T; 3]>; fn main() -> i64 { let rows = Fixed { value: [40, 2] }; 0 }",
        ] {
            let r = resolve(source).unwrap();
            assert!(crate::lower::lower_program(&r.globals).is_err(), "{source}");
        }
    }
    #[test]
    fn record_inference_uses_later_fields_without_reordering_or_retaining_probes() {
        for source in [
            "struct Pair<T> { items: Array<T>, sample: T } type Batch<T> = Pair<T>; fn main() -> i64 { let mut calls = 0; let batch = Batch { items: { calls = calls + 1; [] }, sample: { calls = calls * 10 + 2; 42 } }; if calls == 12 && array_len(batch.items) == 0 { batch.sample } else { 0 } }",
            "struct Pair<T> { items: Array<T>, sample: T } type Batch<T> = Pair<T>; fn main() -> i64 { let mut calls = 0; let batch = Batch { items: { let scratch = [1, 2]; let action = || 42; calls = calls + 1; [] }, sample: { calls = calls * 10 + 2; 42 } }; if calls == 12 && array_len(batch.items) == 0 { batch.sample } else { 0 } }",
            "struct Pair<T> { items: Array<Array<T>>, sample: T } type Batch<T> = Pair<T>; fn main() -> i64 { let batch = Batch { items: [[]], sample: 42 }; batch.sample + array_len(batch.items[0]) }",
            "struct Pair<T> { sample: T, items: Array<T> } fn main() -> i64 { let mut calls = 0; let pair = Pair { items: { calls = calls + 1; [] }, sample: { calls = calls * 10 + 2; 42 } }; if calls == 12 { pair.sample } else { 0 } }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            for stress in [false, true] {
                assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 42, "{source}");
            }
        }
        let source = "struct Pair<T> { items: Array<T>, sample: T } type Batch<T> = Pair<T>; fn main() -> i64 { let batch = Batch { items: [true], sample: 42 }; 0 }";
        let r = resolve(source).unwrap();
        assert!(crate::lower::lower_program(&r.globals).is_err());
    }
    #[test]
    fn enum_inference_uses_later_payloads_and_preserves_written_order() {
        for source in [
            "enum Choice<T> { Values(Array<T>, T) } type Batch<T> = Choice<T>; fn main() -> i64 { let mut calls = 0; let batch = Batch::Values({ calls = calls + 1; [] }, { calls = calls * 10 + 2; 42 }); match batch { Batch::Values(items, sample) => if calls == 12 && array_len(items) == 0 { sample } else { 0 } } }",
            "enum Choice<T> { Values { sample: T, items: Array<T> } } type Batch<T> = Choice<T>; fn main() -> i64 { let mut calls = 0; let batch = Batch::Values { items: { let scratch = [1, 2]; let action = || 42; calls = calls + 1; [] }, sample: { calls = calls * 10 + 2; 42 } }; match batch { Batch::Values { items, sample } => if calls == 12 && array_len(items) == 0 { sample } else { 0 } } }",
            "enum Choice<T> { Values(Array<Array<T>>, T) } type Batch<T> = Choice<T>; fn main() -> i64 { let batch = Batch::Values([[]], 42); match batch { Batch::Values(items, sample) => sample + array_len(items[0]) } }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            for stress in [false, true] {
                assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 42, "{source}");
            }
        }
        let source = "enum Choice<T> { Values(Array<T>, T) } type Batch<T> = Choice<T>; fn main() -> i64 { let batch = Batch::Values([true], 42); 0 }";
        let r = resolve(source).unwrap();
        assert!(crate::lower::lower_program(&r.globals).is_err());
    }
    #[test]
    fn later_compound_elements_constrain_earlier_empty_literals_in_written_order() {
        for source in [
            "struct Boxed<T> { value: T } type Batch<T> = Boxed<(Array<T>, T)>; fn main() -> i64 { let mut calls = 0; let batch = Batch { value: ({ let scratch = [1, 2]; let action = || 42; calls = calls + 1; [] }, { calls = calls * 10 + 2; 42 }) }; if calls == 12 { batch.value.1 + array_len(batch.value.0) } else { 0 } }",
            "struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<Array<T>>>; fn main() -> i64 { let mut calls = 0; let rows = Rows { value: [{ calls = calls + 1; [] }, { calls = calls * 10 + 2; [42] }] }; if calls == 12 { rows.value[1][0] + array_len(rows.value[0]) } else { 0 } }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            for stress in [false, true] {
                assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 42, "{source}");
            }
        }
        let source = "struct Boxed<T> { value: T } type Rows<T> = Boxed<Array<Array<T>>>; fn main() -> i64 { let rows = Rows { value: [[true], [42]] }; 0 }";
        let r = resolve(source).unwrap();
        assert!(crate::lower::lower_program(&r.globals).is_err());
    }
    #[test]
    fn partial_receiver_inference_distinguishes_self_from_method_generic_binders() {
        let source = "enum Choice<A, B> { Empty, Value(A, B) } type Alias<T> = Choice<i64, T>; impl<A, B> Choice<A, B> { fn fallback(self, value: B) -> B { value } } fn main() -> i64 { Alias::Empty.fallback(42) }";
        let r = resolve(source).unwrap();
        let program = crate::lower::lower_program(&r.globals).unwrap();
        assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        let source = "enum Choice<T> { Empty, Value(T) } impl<T> Choice<T> { fn replace(self, other: Self) -> Self { other } } fn main() -> i64 { let value = Choice::Empty.replace(Choice::Value(42)); match value { Choice::Empty => 0, Choice::Value(n) => n } }";
        let r = resolve(source).unwrap();
        let program = crate::lower::lower_program(&r.globals).unwrap();
        assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42);
        let source = "enum Choice<T> { Empty, Value(T) } impl<T> Choice<T> { fn identity<T>(self, value: T) -> T { value } } fn main() -> i64 { Choice::Empty.identity(42) }";
        let r = resolve(source).unwrap();
        let error = crate::lower::lower_program(&r.globals).err().expect("method parameter must not bind the receiver's parameter");
        assert!(error.msg.contains("cannot infer"), "{}", error.msg);
    }
    #[test]
    fn supertrait_cycles_are_rejected_at_unused_declarations() {
        for source in [
            "trait First: First {}",
            "trait First: Second {} trait Second: First {}",
            "trait Growing<T>: Growing<Array<T>> {}",
            "mod left { pub trait First: right::Second {} } mod right { pub trait Second: left::First {} }",
        ] {
            let error = resolve(source).err().expect("unused cyclic supertraits must fail");
            assert!(error.msg.contains("cyclic supertraits"), "{source}: {}", error.msg);
            assert!(error.span.end > error.span.start);
        }
        resolve("trait Base {} trait Left: Base {} trait Right: Base {} trait Diamond: Left + Right {}").unwrap();
        resolve("mod left { pub trait Marker {} } mod right { pub trait Marker: left::Marker {} }").unwrap();
    }
    #[test]
    fn impl_supertraits_are_proved_from_scoped_generic_assumptions() {
        for source in [
            "trait Base {} trait Derived: Base {} struct Boxed<T> { value: T } impl<T> Derived for Boxed<T> where Boxed<T>: Base {}",
            "trait Root {} trait Base: Root {} trait Derived: Root {} struct Boxed<T> { value: T } impl<T: Root> Root for Boxed<T> {} impl<T: Base> Derived for Boxed<T> {}",
            "trait Base<T> {} trait Derived<T>: Base<Array<T>> {} struct Boxed<T> { value: T } impl<T> Base<Array<T>> for Boxed<T> {} impl<T> Derived<T> for Boxed<T> {}",
            "trait Sync {} trait Derived: Sync {} #[value] struct Boxed<T> { value: T } impl<T: Sync> Derived for Boxed<T> {}",
        ] { resolve(source).unwrap_or_else(|error| panic!("{source}: {}", error.msg)); }
    }
    #[test]
    fn unused_impl_requirements_reject_missing_or_leaked_proofs() {
        for source in [
            "trait Base {} trait Derived: Base {} struct Item { value: i64 } impl Derived for Item {}",
            "trait Base {} trait Derived: Base {} struct Boxed<T> { value: T } impl Base for Boxed<i64> {} impl<T> Derived for Boxed<T> {}",
            "trait Base<T> {} trait Derived<T>: Base<Array<T>> {} struct Boxed<T> { value: T } impl<T> Base<T> for Boxed<T> {} impl<T> Derived<T> for Boxed<T> {}",
            "trait Sync {} trait Derived: Sync {} #[value] struct Boxed<T> { value: T } impl<T> Derived for Boxed<T> {}",
            "trait Marker {} struct Factory {} impl Factory where i64: Marker { fn answer() -> i64 { 42 } }",
            "trait Base {} trait Derived: Base {} struct First<T> { value: T } struct Second<T> { value: T } impl<T: Base> Base for First<T> {} impl<T: Base> Derived for First<T> {} impl<T: Base> Base for Second<T> {} impl<T> Derived for Second<T> {}",
        ] {
            let error = resolve(source).err().expect("unused invalid impl must fail");
            assert!(error.msg.contains("required trait obligation"), "{source}: {}", error.msg);
            assert!(error.span.end > error.span.start);
        }
    }
    #[test]
    fn declared_generic_parameters_shadow_intrinsic_type_names() {
        for source in [
            "fn identity<i64>(value: i64) -> i64 { value } fn main() -> i64 { if identity(true) { 42 } else { 0 } }",
            "fn identity<String>(value: String) -> String { value } fn main() -> i64 { identity(42) }",
            "type Identity<i64> = i64; fn main() -> i64 { let value: Identity<bool> = true; if value { 42 } else { 0 } }",
            "struct Boxed<i64> { value: i64 } fn main() -> i64 { let value = Boxed { value: true }; if value.value { 42 } else { 0 } }",
            "trait Read<String> { fn get(self) -> String; } struct Boxed<T> { value: T } impl<T> Read<T> for Boxed<T> { fn get(self) -> T { self.value } } fn main() -> i64 { Boxed { value: 42 }.get() }",
            "fn identity<Array>(value: Array) -> Array { value } fn main() -> i64 { identity(42) }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 42, "{source}");
        }
        for source in [
            "fn invalid<i64>(value: i64<bool>) {}",
            "type Invalid<i64> = i64<bool>;",
        ] {
            let error = resolve(source).err().unwrap();
            assert!(error.msg.contains("type parameter"), "{}", error.msg);
        }
    }
    #[test]
    fn array_alias_names_construct_their_normalized_specialization() {
        for source in [
            "type Words<i64> = Array<i64>; fn main() -> i64 { let mut values: Words<bool> = Words::new(1); values[0] = true; if values[0] { 42 } else { 0 } }",
            "type Words = Array<i64>; fn main() -> i64 { let mut values = Words::new(2); values[0] = 40; values[1] = 2; values[0] + values[1] }",
            "type Words<T> = Array<T>; fn main() -> i64 { let mut values: Words<i64> = Words::new(2); values[0] = 40; values[1] = 2; values[0] + values[1] }",
            "type Words<T> = Array<T>; fn main() -> i64 { let mut values = Words::new(2); values[0] = 40; values[1] = 2; values[0] + values[1] }",
            "mod store { pub type Words = Array<i64>; } fn main() -> i64 { let mut values = store::Words::new(2); values[0] = 40; values[1] = 2; values[0] + values[1] }",
            r#"type Text = Array<String>; fn main() -> i64 { let mut values = Text::new(2); values[0] = "forty"; values[1] = "two"; str_len(values[0]) + str_len(values[1]) + 34 }"#,
            "type Units = Array<()>; fn main() -> i64 { let values = Units::new(42); array_len(values) }",
        ] {
            let r = resolve(source).unwrap();
            let program = crate::lower::lower_program(&r.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            for stress in [false, true] {
                assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 42, "{source}");
            }
        }
        for source in [
            "type Words = Array<i64>; fn main() -> i64 { let values: Array<bool> = Words::new(2); 0 }",
            "type Pairs<T> = Array<(T, T)>; fn main() -> i64 { let values: Array<(i64, bool)> = Pairs::new(2); 0 }",
            "type Words<T> = Array<T>; fn main() -> i64 { let values = Words::new(2); 0 }",
            "type Words = Array<i64>; fn main() -> i64 { let values = Words::new(); 0 }",
            "type Fixed = [i64; 2]; fn main() -> i64 { let values = Fixed::new(2); 0 }",
        ] {
            let r = resolve(source).unwrap();
            assert!(crate::lower::lower_program(&r.globals).is_err(), "{source}");
        }
    }
    #[test]
    fn aliases_normalize_scalar_array_and_function_targets() {
        let r = resolve(
            "type Count = i64; type Words = Array<Count>; type Operation = fn(Count) -> Count;",
        )
        .ok()
        .unwrap();
        assert_eq!(
            r.globals.resolved_aliases["Count"].target,
            Ty::Prim(Prim::I64)
        );
        assert_eq!(
            r.globals.resolved_aliases["Words"].target,
            Ty::Named {
                name: "Array".into(),
                args: vec![Ty::Prim(Prim::I64)]
            }
        );
        assert_eq!(
            r.globals.resolved_aliases["Operation"].target,
            Ty::Fn {
                params: vec![Ty::Prim(Prim::I64)],
                ret: Box::new(Ty::Prim(Prim::I64))
            }
        );
    }
    #[test]
    fn aliases_substitute_generic_arguments_without_capturing_parameter_names() {
        let r=resolve("type T = bool; type Swap<A,B> = (B,A); type Nested<A> = Swap<i64,A>; type Identity<T> = T; fn f(n: Identity<i64>) -> i64 { n }").ok().unwrap();
        assert_eq!(
            r.globals.resolved_aliases["Nested"].target,
            Ty::Tuple(vec![Ty::Var("A".into()), Ty::Prim(Prim::I64)])
        );
        let ctx = TyCtx::from_globals(&r.globals);
        assert_eq!(
            lower_type(&r.globals.fns["f"].params[0].ty, &[], &ctx).unwrap(),
            Ty::Prim(Prim::I64)
        );
    }
    #[test]
    fn aliases_reject_cycles_even_if_unused_or_arguments_grow() {
        for source in [
            "type A = B; type B = A;",
            "type A<T> = A<Array<T>>;",
            "type Tail = A; type A = B; type B = A;",
        ] {
            let error = resolve(source).err().unwrap();
            assert!(error.msg.contains("cyclic type aliases"), "{}", error.msg);
        }
    }
    #[test]
    fn alias_bounds_are_required_in_every_explicit_type_position() {
        let prefix = "trait Bound {} type Restricted<T: Bound> = T; ";
        for body in [
            "fn main() -> i64 { let value: Restricted<i64> = 42; value }",
            "fn unused(value: Restricted<i64>) {}",
            "fn unused() -> Restricted<i64> { 42 }",
            "struct Bad { field: Restricted<i64> }",
            "struct Bad(Restricted<i64>);",
            "enum Bad { Value(Restricted<i64>) }",
            "enum Bad { Value { field: Restricted<i64> } }",
            "const BAD: Restricted<i64> = 42;",
            "fn main() -> i64 { 42 as Restricted<i64> }",
            "fn unused() { let action = |value: Restricted<i64>| value; }",
            "fn unused() { let action = || -> Restricted<i64> { 42 }; }",
            "fn unused() { if true { unsafe { let value: Restricted<i64> = 42; } } }",
            "trait Bad { fn accept(value: Restricted<i64>); }",
            "struct Boxed {} impl Boxed { fn unused(value: Restricted<i64>) {} }",
        ] {
            let error = resolve(&format!("{prefix}{body}")).err().expect(body);
            assert!(error.msg.contains("required trait obligation"), "{body}: {}", error.msg);
            assert!(!error.msg.contains("$alias_scope"), "{}", error.msg);
            assert!(error.span.end > error.span.start);
        }
    }
    #[test]
    fn alias_type_uses_respect_binder_shadowing_and_assumption_scope() {
        for source in [
            "trait Bound {} type R<T: Bound> = T; fn bad<T>(value: R<T>) -> T { value }",
            "trait Bound {} type R<T: Bound> = T; struct Boxed<T> { value: T } impl<T: Bound> Boxed<T> { fn bad<T>(self, value: R<T>) -> T { value } }",
            "trait Bound {} type R<T: Bound> = T; fn a_good<T: Bound>(value: R<T>) -> T { value } fn z_bad<T>(value: R<T>) -> T { value }",
            "trait Sync {} type R<T: Sync> = T; fn bad<T>(value: R<T>) -> T { value }",
            "trait Bound {} trait Rel<A: Bound> {} type R<T: Bound> = T; fn a_scoped<T: Rel<i64>>(value: R<i64>) -> i64 { value } fn z_unconditional(value: R<i64>) -> i64 { value }",
            "trait Bound {} type R<T: Bound> = T; trait Bad { fn accept(value: R<Self>); }",
        ] {
            let error = resolve(source).err().expect(source);
            assert!(error.msg.contains("required trait obligation"), "{source}: {}", error.msg);
            assert!(!error.msg.contains("$alias_scope"), "{}", error.msg);
        }
        for source in [
            "trait Bound {} type R<T: Bound> = T; fn good<T: Bound>(value: R<T>) -> T { value }",
            "trait Bound {} type R<T: Bound> = T; fn good<T>(value: R<T>) -> T where T: Bound { value }",
            "trait Bound {} type R<T: Bound> = T; struct Boxed<T> { value: T } impl<T: Bound> Boxed<T> { fn good<U>(self, value: R<T>) -> T { value } }",
            "trait Bound {} type R<T: Bound> = T; struct Boxed<T> { value: T } impl<T> Boxed<T> { fn good<T: Bound>(self, value: R<T>) -> T { value } }",
            "trait Rel<A> {} type R<T: Rel<U>, U> = T; struct Boxed<T> { value: T } impl<T> Boxed<T> { fn good<U: Rel<T>>(self, value: R<U,T>) -> U { value } }",
            "trait Bound {} type R<T: Bound> = T; trait Derived: Bound { fn accept(value: R<Self>) -> Self; }",
            "trait Bound {} type R<T: Bound> = T; struct Boxed<T> { value: T } impl<T: Bound> Bound for Boxed<T> {} impl<T: Bound> Boxed<T> { fn good(self, value: R<Self>) -> Self { value } }",
            "trait Bound {} type R<i64: Bound> = i64; fn good<i64: Bound>(value: R<i64>) -> i64 { value }",
        ] { resolve(source).unwrap_or_else(|error| panic!("{source}: {}", error.msg)); }
    }
    #[test]
    fn inferred_alias_uses_cannot_bypass_declared_bounds() {
        for source in [
            "trait Bound {} struct Boxed<T> { value: T } type R<T: Bound> = Boxed<T>; fn main() -> i64 { let value = R { value: true }; 42 }",
            "trait Bound {} enum Choice<T> { Value(T), Empty } type R<T: Bound> = Choice<T>; fn main() -> i64 { let value = R::Value(true); 42 }",
            "trait Bound {} enum Choice<T> { Value { value: T } } type R<T: Bound> = Choice<T>; fn main() -> i64 { let value = R::Value { value: true }; 42 }",
            "trait Bound {} type R<T: Bound> = Array<T>; fn main() -> i64 { let mut value = R::new(1); value[0] = true; 42 }",
            "trait Bound {} struct Boxed<T> { value: T } impl<T> Boxed<T> { fn make(value: T) -> Self { Boxed { value } } } type R<T: Bound> = Boxed<T>; fn main() -> i64 { let value = R::make(true); 42 }",
            "trait Bound {} struct Boxed<T> { value: T } type R<T: Bound> = Boxed<T>; fn main() -> i64 { let value = Boxed { value: true }; match value { R { value } => 42 } }",
            "trait Bound {} struct Boxed<T> { value: T } type R<T: Bound> = Boxed<T>; fn main() -> i64 { let R { value } = Boxed { value: true }; 42 }",
            "trait Bound {} enum Choice<T> { Value(T), Empty } type R<T: Bound> = Choice<T>; fn main() -> i64 { let value = Choice::Value(true); match value { R::Value(v) => 42, R::Empty => 0 } }",
            "trait Bound {} enum Choice<T> { Value(T), Empty } type R<T: Bound> = Choice<T>; fn main() -> i64 { let value = Choice::Value(true); match value { R::Value(v) if v => 42, _ => 0 } }",
        ] {
            let resolved = resolve(source).unwrap();
            let mut errors = crate::lower::check_program(&resolved.globals).expect_err(source);
            errors.push(crate::lower::lower_program(&resolved.globals).err().expect(source));
            for error in errors {
                assert!(error.msg.contains("trait bound") && error.msg.contains("not satisfied"), "{source}: {}", error.msg);
            }
        }
        for source in [
            "trait Bound {} impl Bound for i64 {} struct Plain { value: i64 } type R<T: Bound> = Plain; fn main() -> i64 { R { value: 42 }.value }",
            "trait Rel<A> {} impl Rel<i64> for bool {} struct Boxed<T> { value: T } type R<T: Rel<U>, U> = Boxed<T>; fn main() -> i64 { let value = R { value: true }; 42 }",
        ] {
            let resolved = resolve(source).unwrap();
            let error = crate::lower::lower_program(&resolved.globals).err().expect(source);
            assert!(error.msg.contains("cannot infer"), "{source}: {}", error.msg);
        }
    }
    #[test]
    fn bounded_alias_constructors_and_patterns_run_with_valid_proofs() {
        for source in [
            "trait Bound {} impl Bound for i64 {} struct Boxed<T> { value: T } type R<T: Bound> = Boxed<T>; fn main() -> i64 { let value = R { value: 42 }; match value { R { value } => value } }",
            "trait Bound {} impl Bound for i64 {} enum Choice<T> { Value(T), Empty } type R<T: Bound> = Choice<T>; fn main() -> i64 { let value = R::Value(42); match value { R::Value(v) => v, R::Empty => 0 } }",
            "trait Bound {} impl Bound for i64 {} type R<T: Bound> = Array<T>; fn main() -> i64 { let mut value = R::new(1); value[0] = 42; value[0] }",
            "trait Bound {} impl Bound for i64 {} struct Boxed<T> { value: T } impl<T> Boxed<T> { fn make(value: T) -> Self { Boxed { value } } } type R<T: Bound> = Boxed<T>; fn main() -> i64 { R::make(42).value }",
            "trait Bound {} mod local { pub trait Bound {} impl Bound for i64 {} pub struct Boxed<T> { value: T } pub type R<T: Bound> = Boxed<T>; } fn main() -> i64 { local::R { value: 42 }.value }",
        ] {
            let resolved = resolve(source).unwrap();
            let program = crate::lower::lower_program(&resolved.globals).unwrap_or_else(|error| panic!("{source}: {}", error.msg));
            for stress in [false, true] { assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 42); }
        }
    }
    #[test]
    fn bounded_alias_annotations_execute_with_generic_proofs() {
        let source = "trait Bound {} impl Bound for i64 {} type Values<T: Bound> = Array<T>; fn first<T: Bound>(values: Values<T>) -> T { values[0] } fn main() -> i64 { let values: Values<i64> = [42]; first(values) }";
        let resolved = resolve(source).unwrap();
        let program = crate::lower::lower_program(&resolved.globals).unwrap();
        for stress in [false, true] {
            assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, stress) }.unwrap(), 42);
        }
    }
    #[test]
    fn alias_targets_require_proofs_before_constraints_are_erased() {
        for source in [
            "trait Bound {} type Restricted<T: Bound> = Array<T>; type Bad = Restricted<i64>;",
            "trait Bound {} type Restricted<T: Bound> = Array<T>; type Bad<T> = Restricted<T>;",
            "trait Bound {} type Restricted<T: Bound> = i64; type Bad = Restricted<bool>;",
            "trait Bound {} type Restricted<T: Bound> = T; type Bad = (i64, Array<Restricted<bool>>);",
            "trait Bound {} type Restricted<T: Bound> = T; type Bad = fn(i64) -> Restricted<bool>;",
            "trait Bound {} type Restricted<T: Bound> = T; type Bad = extern fn(Restricted<bool>) -> i64;",
            "trait Bound<A> {} type Restricted<T: Bound<i64>> = Array<T>; type Bad<T: Bound<bool>> = Restricted<T>;",
            "trait Bound {} type Restricted<T: Bound> = T; type AGood<T: Bound> = Restricted<T>; type ZBad<T> = Restricted<T>;",
            "trait Sync {} type Shared<T: Sync> = Array<T>; type Bad<T> = Shared<T>;",
            "trait Base {} trait Bound {} struct Boxed<T> { value: T } impl<T: Base> Bound for Boxed<T> {} type Restricted<T: Bound> = T; type Bad<T> = Restricted<Boxed<T>>;",
            "trait Bound {} trait With<A> {} type Restricted<T: Bound> = T; type Bad<T: With<Restricted<i64>>> = T;",
        ] {
            let error = resolve(source).err().expect(source);
            assert!(error.msg.contains("required trait obligation"), "{source}: {}", error.msg);
            assert!(error.span.end > error.span.start, "{source}");
        }
    }
    #[test]
    fn alias_targets_use_scoped_generic_and_conditional_proofs() {
        for source in [
            "trait Bound {} impl Bound for i64 {} type Restricted<T: Bound> = Array<T>; type Good = Restricted<i64>;",
            "trait Bound {} type Restricted<T: Bound> = Array<T>; type Good<U: Bound> = Restricted<U>;",
            "trait Base {} trait Derived: Base {} type Restricted<T: Base> = T; type Good<T: Derived> = Restricted<T>;",
            "trait Bound<A> {} type Restricted<T: Bound<U>, U> = Array<T>; type Good<A: Bound<B>, B> = Restricted<A,B>;",
            "trait Base {} trait Bound {} struct Boxed<T> { value: T } impl<T: Base> Bound for Boxed<T> {} type Restricted<T: Bound> = T; type Good<T: Base> = Restricted<Boxed<T>>;",
            "trait Sync {} type Shared<T: Sync> = Array<T>; type Good<T: Sync> = Shared<T>;",
            "trait Bound {} impl Bound for i64 {} type First<T: Bound> = T; type Second<U: Bound> = First<U>; type Good = Second<i64>;",
        ] { resolve(source).unwrap_or_else(|error| panic!("{source}: {}", error.msg)); }
    }
    #[test]
    fn alias_ast_concrete_conditions_are_proved_rather_than_assumed() {
        for implemented in [false, true] {
            let implementation = if implemented { "impl Bound for bool {}" } else { "" };
            let source = format!("trait Bound {{}} {implementation} type Value<T> = T; fn carrier<T>() where bool: Bound {{}}");
            let mut module = parse_module(&lex(&source).unwrap()).unwrap();
            let generics = module.items.iter().find_map(|item| match &item.kind {
                crate::ast::ItemKind::Fn(f) => Some(f.generics.clone()), _ => None,
            }).unwrap();
            for item in &mut module.items {
                if let crate::ast::ItemKind::TypeAlias(alias) = &mut item.kind { alias.generics = generics.clone(); }
            }
            let result = resolve_module(module);
            assert_eq!(result.is_ok(), implemented);
            if let Err(error) = result { assert!(error.msg.contains("bool: Bound"), "{}", error.msg); }
        }
    }
    #[test]
    fn bounded_alias_chains_preserve_requirements_without_recursive_expansion() {
        let mut source = String::from("trait Bound {} impl Bound for i64 {} type A0<T: Bound> = T;");
        for index in 1..2000 {
            source.push_str(&format!("type A{index}<T: Bound> = A{}<T>;", index - 1));
        }
        source.push_str("type Selected = A1999<i64>;");
        resolve(&source).unwrap_or_else(|error| panic!("{}", error.msg));
        source.push_str("type ZInvalid = A1999<bool>;");
        let error = resolve(&source).err().unwrap();
        assert!(error.msg.contains("bool: Bound"), "{}", error.msg);
    }
    #[test]
    fn alias_bound_arguments_do_not_introduce_a_self_type() {
        let error = resolve("trait Bound<A> {} type Bad<T: Bound<Self>> = T;").err().unwrap();
        assert!(error.msg.contains("Self"), "{}", error.msg);
    }
    #[test]
    fn trait_diagnostics_distinguish_missing_and_ambiguous_names() {
        let missing = resolve("type Values<T: Missing> = Array<T>;").err().unwrap();
        assert!(missing.msg.contains("unknown trait `Missing`"), "{}", missing.msg);
        assert!(!missing.msg.contains("ambiguous"));
        for imports in ["", "use left::Bound; use right::Bound;"] {
            let source = format!("mod left {{ pub trait Bound {{}} }} mod right {{ pub trait Bound {{}} }} {imports} type Values<T: Bound> = Array<T>;");
            let error = resolve(&source).err().unwrap();
            assert!(error.msg.contains("ambiguous trait `Bound`"), "{}", error.msg);
            assert!(error.msg.contains("left::Bound") && error.msg.contains("right::Bound") && error.msg.contains("qualify"), "{}", error.msg);
            assert!(!error.msg.contains("unknown"));
        }
    }
    #[test]
    fn alias_generic_bounds_resolve_in_the_definition_module() {
        for (source, needle) in [
            ("type Values<T: Missing> = Array<T>;", "unknown trait"),
            ("trait Bound<A> {} type Values<T: Bound> = Array<T>;", "takes 1"),
            ("mod hidden { trait Bound {} } type Values<T: hidden::Bound> = Array<T>;", "private"),
            ("mod left { pub trait Bound {} } mod right { pub trait Bound {} } use left::Bound; use right::Bound; type Values<T: Bound> = Array<T>;", "ambiguous"),
            ("trait Bound<A> {} type Values<T: Bound<Missing>> = Array<T>;", "unknown"),
        ] {
            let error = resolve(source).err().expect(source);
            assert!(error.msg.contains(needle), "{source}: {}", error.msg);
        }
        let resolved = resolve("trait Bound<A> {} mod local { pub trait Bound {} pub type Values<T: Bound> = Array<T>; } mod imported { use local::Bound; pub type Values<T: Bound> = Array<T>; }").unwrap();
        for name in ["local::Values", "imported::Values"] {
            assert_eq!(resolved.globals.aliases[name].generics.params[0].bounds[0].path.segments.join("::"), "local::Bound");
        }
    }
    #[test]
    fn aliases_and_modules_cannot_share_a_type_namespace() {
        for source in [
            "type words = Array<i64>; mod words {}",
            "mod words {} type words = Array<i64>;",
            "mod outer { type words = Array<i64>; mod words {} }",
            "mod outer { mod words {} type words = Array<i64>; }",
            "type words = Array<i64>; mod words { pub fn new(n: i64) -> i64 { n } }",
            "mod words { pub fn new(n: i64) -> i64 { n } } type words = Array<i64>;",
        ] {
            let error = resolve(source).err().expect(source);
            assert!(error.msg.contains("conflicts with a type alias"), "{source}: {}", error.msg);
        }
        assert!(resolve("mod left { pub type words = Array<i64>; } mod right { pub mod words {} }").is_ok());
    }
    #[test]
    fn aliases_validate_targets_arity_and_duplicate_parameters_at_declaration() {
        for (source, needle) in [
            ("type Bad = missing::Type;", "unknown"),
            (
                "type Pair<T> = Array<T>; type Bad = Pair<i64,i64>;",
                "takes 1",
            ),
            (
                "struct Pair<A,B> { a: A, b: B } type Bad = Pair<i64>;",
                "takes 2",
            ),
            ("type Bad<T,T> = Array<T>;", "duplicate type parameter"),
            ("type Bad = Self;", "Self"),
        ] {
            let error = resolve(source).err().unwrap();
            assert!(error.msg.contains(needle), "{}", error.msg);
        }
    }
    #[test]
    fn aliases_use_definition_module_instead_of_a_same_named_root_type() {
        let r=resolve("struct Item { value: bool } mod local { pub struct Item { value: i64 } pub type Alias = Item; } type Selected = local::Alias;").ok().unwrap();
        assert_eq!(
            r.globals.resolved_aliases["Selected"].target,
            Ty::Named {
                name: "local::Item".into(),
                args: vec![]
            }
        );
    }
    #[test]
    fn aliases_reject_private_targets_from_other_modules() {
        let error = resolve("mod hidden { type Private = i64; } type Leak = hidden::Private;")
            .err()
            .unwrap();
        assert!(error.msg.contains("private"));
    }
    #[test]
    fn aliases_resolve_long_forward_chains_without_recursive_alias_calls() {
        let mut source = String::new();
        for index in 0..4000 {
            source.push_str(&format!("type A{index} = A{};\n", index + 1));
        }
        source.push_str("type A4000 = i64;");
        let r = resolve(&source).ok().unwrap();
        assert_eq!(r.globals.resolved_aliases.len(), 4001);
        assert_eq!(r.globals.resolved_aliases["A0"].target, Ty::Prim(Prim::I64));
    }
}

#[path = "type_alias_paths.rs"]
mod paths;
pub(crate) use paths::qualify_types;
