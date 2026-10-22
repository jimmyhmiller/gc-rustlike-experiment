//! `#[derive(...)]`: the source text of the generated impls.
//!
//! Each derive becomes an ordinary `impl` written in gc-rust, which the parser
//! lexes and parses in place of the attribute. Like Rust, every type parameter
//! of the deriving type gets the derived trait as a bound.
//!
//! `PartialEq` and `Eq` both produce the `Eq` impl (`==`), and `PartialOrd`
//! and `Ord` both produce the `Ord` impl (`<`, `cmp`). `Copy` is accepted and
//! has no effect: values are already copied or shared by the GC.

use crate::ast::*;

/// The derive names this module understands, in the order impls are emitted.
pub const KNOWN: &[&str] = &["Debug", "Clone", "Copy", "PartialEq", "Eq", "Hash", "PartialOrd", "Ord", "Default"];

/// The source of every impl `derives` asks for on `item` (a struct or enum).
pub fn derive_impls(item: &ItemKind, derives: &[String]) -> Result<Vec<(&'static str, String)>, String> {
    let (name, generics) = match item {
        ItemKind::Struct(s) => (&s.name, &s.generics),
        ItemKind::Enum(e) => (&e.name, &e.generics),
        _ => return Err("`#[derive]` applies only to a struct or enum".into()),
    };
    let has = |d: &str| derives.iter().any(|x| x == d);
    let mut out = Vec::new();
    let mut emit = |trait_name: &'static str, body: String| {
        let params: Vec<String> = generics.params.iter().map(|p| {
            let mut bounds: Vec<String> = p.bounds.iter().map(|b| b.path.segments.join("::")).collect();
            if !bounds.iter().any(|b| b == trait_name) { bounds.push(trait_name.to_string()); }
            format!("{}: {}", p.name, bounds.join(" + "))
        }).collect();
        let args: Vec<&str> = generics.params.iter().map(|p| p.name.as_str()).collect();
        let (impl_generics, self_args) = if params.is_empty() {
            (String::new(), String::new())
        } else {
            (format!("<{}>", params.join(", ")), format!("<{}>", args.join(", ")))
        };
        out.push((trait_name, format!("impl{impl_generics} {trait_name} for {name}{self_args} {{ {body} }}")));
    };
    for d in derives {
        if !KNOWN.contains(&d.as_str()) {
            return Err(format!("cannot derive `{}` (supported: {})", d, KNOWN.join(", ")));
        }
    }
    if has("Debug") { emit("Debug", debug(item)); }
    if has("Clone") { emit("Clone", clone(item)); }
    if has("PartialEq") || has("Eq") { emit("Eq", eq(item)); }
    if has("Hash") { emit("Hash", hash(item)); }
    if has("Ord") { emit("Ord", ord(item)); }
    // Without `Ord`, `PartialOrd` compares field by field with `partial_cmp`
    // (so fields may be floats); with it, `Ord` supplies `partial_cmp`.
    if has("PartialOrd") && !has("Ord") { emit("PartialOrd", partial_ord(item)); }
    if has("Default") { emit("Default", default(item)?); }
    Ok(out)
}

/// The fields of one struct or variant: named, positional, or none.
enum Shape<'a> {
    Named(Vec<(&'a str, &'a Type)>),
    Tuple(Vec<&'a Type>),
    Unit,
}

fn struct_shape(body: &StructBody) -> Shape<'_> {
    match body {
        StructBody::Named(fields) => Shape::Named(fields.iter().map(|f| (f.name.as_str(), &f.ty)).collect()),
        StructBody::Tuple(tys) => Shape::Tuple(tys.iter().collect()),
        #[allow(unreachable_patterns)]
        _ => Shape::Unit,
    }
}

fn variant_shape(payload: &VariantPayload) -> Shape<'_> {
    match payload {
        VariantPayload::None => Shape::Unit,
        VariantPayload::Tuple(tys) => Shape::Tuple(tys.iter().collect()),
        VariantPayload::Named(fields) => Shape::Named(fields.iter().map(|f| (f.name.as_str(), &f.ty)).collect()),
    }
}

/// A pattern binding every field of `variant` to `{prefix}{i}`.
fn variant_pattern(enum_name: &str, variant: &str, shape: &Shape, prefix: &str) -> String {
    match shape {
        Shape::Unit => format!("{enum_name}::{variant}"),
        Shape::Tuple(tys) => format!("{enum_name}::{variant}({})", (0..tys.len()).map(|i| format!("{prefix}{i}")).collect::<Vec<_>>().join(", ")),
        Shape::Named(fields) => format!("{enum_name}::{variant} {{ {} }}", fields.iter().enumerate().map(|(i, (n, _))| format!("{n}: {prefix}{i}")).collect::<Vec<_>>().join(", ")),
    }
}

/// The value expressions of a shape's fields: `self.x` / `self.0` for a
/// struct, `{prefix}{i}` for a variant bound by `variant_pattern`.
fn field_values(shape: &Shape, base: Option<&str>, prefix: &str) -> Vec<String> {
    match shape {
        Shape::Unit => vec![],
        Shape::Tuple(tys) => (0..tys.len()).map(|i| match base { Some(b) => format!("{b}.{i}"), None => format!("{prefix}{i}") }).collect(),
        Shape::Named(fields) => fields.iter().enumerate().map(|(i, (n, _))| match base { Some(b) => format!("{b}.{n}"), None => format!("{prefix}{i}") }).collect(),
    }
}

fn debug_body(label: &str, shape: &Shape, values: &[String]) -> String {
    match shape {
        Shape::Unit => format!("f.write_str(\"{label}\")"),
        Shape::Tuple(_) => format!("f.debug_tuple(\"{label}\"){}.finish()", values.iter().map(|v| format!(".field({v})")).collect::<String>()),
        Shape::Named(fields) => format!(
            "f.debug_struct(\"{label}\"){}.finish()",
            fields.iter().zip(values).map(|((n, _), v)| format!(".field(\"{n}\", {v})")).collect::<String>()
        ),
    }
}

fn debug(item: &ItemKind) -> String {
    let body = match item {
        ItemKind::Struct(s) => {
            let shape = struct_shape(&s.body);
            debug_body(&s.name, &shape, &field_values(&shape, Some("self"), ""))
        }
        ItemKind::Enum(e) => match_arms(e, |v, shape| {
            debug_body(&v.name, shape, &field_values(shape, None, "a"))
        }),
        _ => unreachable!(),
    };
    format!("fn fmt(self, f: Formatter) -> FmtResult {{ {body} }}")
}

/// `match self { Enum::V(a0, ..) => <arm>, ... }`.
fn match_arms(e: &EnumDef, arm: impl Fn(&VariantDef, &Shape) -> String) -> String {
    if e.variants.is_empty() { return "match self {}".into(); }
    let arms: Vec<String> = e.variants.iter().map(|v| {
        let shape = variant_shape(&v.payload);
        format!("{} => {{ {} }}", variant_pattern(&e.name, &v.name, &shape, "a"), arm(v, &shape))
    }).collect();
    format!("match self {{ {} }}", arms.join(", "))
}

/// Whether a written field type is copied rather than cloned: scalars,
/// tuples, functions, and fixed arrays (which have no `Clone` impl).
fn copied(ty: &Type) -> bool {
    match &ty.kind {
        TypeKind::Path(p, args) => args.is_empty() && p.is_single() && matches!(p.last(),
            "i8" | "i16" | "i32" | "i64" | "i128" | "u8" | "u16" | "u32" | "u64" | "u128" | "f32" | "f64" | "bool" | "char" | "RawPtr"),
        TypeKind::Tuple(_) | TypeKind::Fn(..) | TypeKind::ExternFn(..) | TypeKind::Array(..) => true,
        TypeKind::SelfType => false,
    }
}

fn shape_types<'a>(shape: &Shape<'a>) -> Vec<&'a Type> {
    match shape {
        Shape::Unit => vec![],
        Shape::Tuple(tys) => tys.clone(),
        Shape::Named(fields) => fields.iter().map(|(_, t)| *t).collect(),
    }
}

fn construct(path: &str, shape: &Shape, values: &[String]) -> String {
    match shape {
        Shape::Unit => path.to_string(),
        Shape::Tuple(_) => format!("{path}({})", values.join(", ")),
        Shape::Named(fields) => format!("{path} {{ {} }}", fields.iter().zip(values).map(|((n, _), v)| format!("{n}: {v}")).collect::<Vec<_>>().join(", ")),
    }
}

fn clone_values(shape: &Shape, values: Vec<String>) -> Vec<String> {
    shape_types(shape).into_iter().zip(values).map(|(t, v)| if copied(t) { v } else { format!("{v}.clone()") }).collect()
}

fn clone(item: &ItemKind) -> String {
    let body = match item {
        ItemKind::Struct(s) => {
            let shape = struct_shape(&s.body);
            construct(&s.name, &shape, &clone_values(&shape, field_values(&shape, Some("self"), "")))
        }
        ItemKind::Enum(e) => match_arms(e, |v, shape| {
            construct(&format!("{}::{}", e.name, v.name), shape, &clone_values(shape, field_values(shape, None, "a")))
        }),
        _ => unreachable!(),
    };
    format!("fn clone(self) -> Self {{ {body} }}")
}

fn eq(item: &ItemKind) -> String {
    let body = match item {
        ItemKind::Struct(s) => {
            let shape = struct_shape(&s.body);
            let pairs: Vec<String> = field_values(&shape, Some("self"), "").into_iter()
                .zip(field_values(&shape, Some("other"), ""))
                .map(|(a, b)| format!("{a} == {b}")).collect();
            if pairs.is_empty() { "true".into() } else { pairs.join(" && ") }
        }
        ItemKind::Enum(e) => {
            let mut arms: Vec<String> = e.variants.iter().map(|v| {
                let shape = variant_shape(&v.payload);
                let pairs: Vec<String> = field_values(&shape, None, "a").into_iter()
                    .zip(field_values(&shape, None, "b"))
                    .map(|(a, b)| format!("{a} == {b}")).collect();
                format!(
                    "({}, {}) => {}",
                    variant_pattern(&e.name, &v.name, &shape, "a"),
                    variant_pattern(&e.name, &v.name, &shape, "b"),
                    if pairs.is_empty() { "true".into() } else { pairs.join(" && ") }
                )
            }).collect();
            if e.variants.len() > 1 { arms.push("_ => false".into()); }
            format!("match (self, other) {{ {} }}", arms.join(", "))
        }
        _ => unreachable!(),
    };
    format!("fn eq(self, other: Self) -> bool {{ {body} }}")
}

fn hash_fold(values: &[String], start: &str) -> String {
    let mut h = start.to_string();
    for v in values { h = format!("({h}) * 31 + {v}.hash()"); }
    h
}

fn hash(item: &ItemKind) -> String {
    let body = match item {
        ItemKind::Struct(s) => {
            let shape = struct_shape(&s.body);
            hash_fold(&field_values(&shape, Some("self"), ""), "17")
        }
        ItemKind::Enum(e) => {
            let arms: Vec<String> = e.variants.iter().enumerate().map(|(i, v)| {
                let shape = variant_shape(&v.payload);
                format!("{} => {}", variant_pattern(&e.name, &v.name, &shape, "a"), hash_fold(&field_values(&shape, None, "a"), &(i + 17).to_string()))
            }).collect();
            format!("match self {{ {} }}", arms.join(", "))
        }
        _ => unreachable!(),
    };
    format!("fn hash(self) -> i64 {{ {body} }}")
}

/// Lexicographic comparison of paired values.
fn cmp_chain(pairs: Vec<(String, String)>) -> String {
    let mut out = String::from("{ ");
    for (i, (a, b)) in pairs.into_iter().enumerate() {
        out.push_str(&format!("let c{i} = {a}.cmp({b}); if c{i} != Ordering::Equal {{ return c{i}; }} "));
    }
    out.push_str("Ordering::Equal }");
    out
}

fn ord(item: &ItemKind) -> String {
    let body = match item {
        ItemKind::Struct(s) => {
            let shape = struct_shape(&s.body);
            cmp_chain(field_values(&shape, Some("self"), "").into_iter().zip(field_values(&shape, Some("other"), "")).collect())
        }
        ItemKind::Enum(e) => {
            let index = |who: &str| {
                let arms: Vec<String> = e.variants.iter().enumerate().map(|(i, v)| {
                    let shape = variant_shape(&v.payload);
                    format!("{} => {i}", variant_pattern(&e.name, &v.name, &shape, "_unused"))
                }).collect();
                if arms.is_empty() { format!("match {who} {{}}") } else { format!("match {who} {{ {} }}", arms.join(", ")) }
            };
            let mut arms: Vec<String> = e.variants.iter().map(|v| {
                let shape = variant_shape(&v.payload);
                format!(
                    "({}, {}) => {}",
                    variant_pattern(&e.name, &v.name, &shape, "a"),
                    variant_pattern(&e.name, &v.name, &shape, "b"),
                    cmp_chain(field_values(&shape, None, "a").into_iter().zip(field_values(&shape, None, "b")).collect())
                )
            }).collect();
            if e.variants.len() > 1 { arms.push("_ => Ordering::Equal".into()); }
            format!(
                "let ia: i64 = {}; let ib: i64 = {}; if ia != ib {{ return ia.cmp(ib); }} match (self, other) {{ {} }}",
                index("self"), index("other"), arms.join(", ")
            )
        }
        _ => unreachable!(),
    };
    format!("fn cmp(self, other: Self) -> Ordering {{ {body} }}")
}

/// Lexicographic `partial_cmp` of paired values.
fn partial_chain(pairs: Vec<(String, String)>) -> String {
    let mut out = String::from("{ ");
    for (i, (a, b)) in pairs.into_iter().enumerate() {
        out.push_str(&format!("let c{i} = {a}.partial_cmp({b}); if c{i} != Option::Some(Ordering::Equal) {{ return c{i}; }} "));
    }
    out.push_str("Option::Some(Ordering::Equal) }");
    out
}

fn partial_ord(item: &ItemKind) -> String {
    let body = match item {
        ItemKind::Struct(s) => {
            let shape = struct_shape(&s.body);
            partial_chain(field_values(&shape, Some("self"), "").into_iter().zip(field_values(&shape, Some("other"), "")).collect())
        }
        ItemKind::Enum(e) => {
            let index = |who: &str| {
                let arms: Vec<String> = e.variants.iter().enumerate().map(|(i, v)| {
                    let shape = variant_shape(&v.payload);
                    format!("{} => {i}", variant_pattern(&e.name, &v.name, &shape, "_unused"))
                }).collect();
                if arms.is_empty() { format!("match {who} {{}}") } else { format!("match {who} {{ {} }}", arms.join(", ")) }
            };
            let mut arms: Vec<String> = e.variants.iter().map(|v| {
                let shape = variant_shape(&v.payload);
                format!(
                    "({}, {}) => {}",
                    variant_pattern(&e.name, &v.name, &shape, "a"),
                    variant_pattern(&e.name, &v.name, &shape, "b"),
                    partial_chain(field_values(&shape, None, "a").into_iter().zip(field_values(&shape, None, "b")).collect())
                )
            }).collect();
            if e.variants.len() > 1 { arms.push("_ => Option::Some(Ordering::Equal)".into()); }
            format!(
                "let ia: i64 = {}; let ib: i64 = {}; if ia != ib {{ return Option::Some(ia.cmp(ib)); }} match (self, other) {{ {} }}",
                index("self"), index("other"), arms.join(", ")
            )
        }
        _ => unreachable!(),
    };
    format!("fn partial_cmp(self, other: Self) -> Option<Ordering> {{ {body} }}")
}

/// `T::default()` for a written field type.
fn default_value(ty: &Type) -> Result<String, String> {
    match &ty.kind {
        TypeKind::Path(p, _) => Ok(format!("{}::default()", p.segments.join("::"))),
        TypeKind::Tuple(tys) if tys.is_empty() => Ok("()".into()),
        TypeKind::Tuple(tys) => {
            let parts = tys.iter().map(default_value).collect::<Result<Vec<_>, _>>()?;
            Ok(if parts.len() == 1 { format!("({},)", parts[0]) } else { format!("({})", parts.join(", ")) })
        }
        _ => Err("`#[derive(Default)]` supports fields whose types are named or tuples".into()),
    }
}

fn default(item: &ItemKind) -> Result<String, String> {
    let ItemKind::Struct(s) = item else {
        return Err("`#[derive(Default)]` is supported on structs".into());
    };
    let shape = struct_shape(&s.body);
    let values = shape_types(&shape).into_iter().map(default_value).collect::<Result<Vec<_>, _>>()?;
    Ok(format!("fn default() -> Self {{ {} }}", construct(&s.name, &shape, &values)))
}
