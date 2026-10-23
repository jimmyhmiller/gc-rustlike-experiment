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
pub const KNOWN: &[&str] = &["Debug", "Clone", "Copy", "PartialEq", "Eq", "Hash", "PartialOrd", "Ord", "Default", "Serialize", "Deserialize"];

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
    // serde's derives, against a `serde` module's `Serialize`/`Deserialize`
    // traits and `Content` data model (as Rust's derives use the serde crate).
    if has("Serialize") { emit("Serialize", serialize(item)); }
    if has("Deserialize") { emit("Deserialize", deserialize(item)); }
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
    format!("#[inline] fn clone(self) -> Self {{ {body} }}")
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
        // A switch on `self`, then on `other` for that variant: both are
        // tag switches, so comparing is constant-time in the variant count.
        ItemKind::Enum(e) => {
            let arms: Vec<String> = e.variants.iter().map(|v| {
                let shape = variant_shape(&v.payload);
                let pairs: Vec<String> = field_values(&shape, None, "a").into_iter()
                    .zip(field_values(&shape, None, "b"))
                    .map(|(a, b)| format!("{a} == {b}")).collect();
                let same = if pairs.is_empty() { "true".into() } else { pairs.join(" && ") };
                let other_arms = if e.variants.len() > 1 { format!("{} => {same}, _ => false", variant_pattern(&e.name, &v.name, &shape, "b")) }
                    else { format!("{} => {same}", variant_pattern(&e.name, &v.name, &shape, "b")) };
                format!("{} => match other {{ {other_arms} }}", variant_pattern(&e.name, &v.name, &shape, "a"))
            }).collect();
            if arms.is_empty() { "true".into() } else { format!("match self {{ {} }}", arms.join(", ")) }
        }
        _ => unreachable!(),
    };
    format!("#[inline] fn eq(self, other: Self) -> bool {{ {body} }}")
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
    format!("#[inline] fn cmp(self, other: Self) -> Ordering {{ {body} }}")
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
    format!("#[inline] fn partial_cmp(self, other: Self) -> Option<Ordering> {{ {body} }}")
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

/// A written type as source text.
fn type_src(ty: &Type) -> String {
    match &ty.kind {
        // `Box<T>`, `Rc<T>`, `Arc<T>` are `T`.
        TypeKind::Path(p, args) if p.is_single() && matches!(p.last(), "Box" | "Rc" | "Arc") && args.len() == 1 => type_src(&args[0]),
        TypeKind::Path(p, args) => {
            let mut segments = p.segments.clone();
            if let Some(last) = segments.last_mut() {
                if let Some(t) = last.strip_prefix("dyn#") { *last = format!("dyn {t}"); }
            }
            let path = segments.join("::");
            if args.is_empty() { path } else { format!("{path}<{}>", args.iter().map(type_src).collect::<Vec<_>>().join(", ")) }
        }
        TypeKind::Tuple(tys) if tys.len() == 1 => format!("({},)", type_src(&tys[0])),
        TypeKind::Tuple(tys) => format!("({})", tys.iter().map(type_src).collect::<Vec<_>>().join(", ")),
        TypeKind::Array(e, n) => format!("[{}; {}]", type_src(e), match &*n.kind { ExprKind::Int(v, _) => v.to_string(), _ => "_".into() }),
        TypeKind::Fn(ps, r) | TypeKind::ExternFn(ps, r) => format!("fn({}){}", ps.iter().map(type_src).collect::<Vec<_>>().join(", "),
            r.as_ref().map(|r| format!(" -> {}", type_src(r))).unwrap_or_default()),
        TypeKind::SelfType => "Self".into(),
    }
}

/// The serialized name of a field: its `#[serde(rename = "..")]` or its own.
fn serde_name(name: &str, opts: &[String]) -> String {
    opts.iter().find_map(|o| o.strip_prefix("rename=")).unwrap_or(name).to_string()
}

fn field_opts<'a>(item: &'a ItemKind, variant: Option<&str>) -> Vec<&'a [String]> {
    let fields = match item {
        ItemKind::Struct(s) => match &s.body { StructBody::Named(fs) => Some(fs), _ => None },
        ItemKind::Enum(e) => e.variants.iter().find(|v| Some(v.name.as_str()) == variant)
            .and_then(|v| match &v.payload { VariantPayload::Named(fs) => Some(fs), _ => None }),
        _ => None,
    };
    fields.map(|fs| fs.iter().map(|f| f.serde.as_slice()).collect()).unwrap_or_default()
}

/// serde's default representation of one struct or variant's fields:
/// named as a map, one positional field as itself, several as a sequence.
fn serialize_shape(shape: &Shape, values: &[String], opts: &[&[String]]) -> String {
    match shape {
        Shape::Unit => "serde::Content::Unit".into(),
        Shape::Tuple(tys) if tys.len() == 1 => format!("{}.serialize()", values[0]),
        Shape::Tuple(_) => format!("serde::Content::Seq(vec![{}])", values.iter().map(|v| format!("{v}.serialize()")).collect::<Vec<_>>().join(", ")),
        Shape::Named(fields) => {
            let entries: Vec<String> = fields.iter().zip(values).enumerate()
                .filter(|(i, _)| !opts.get(*i).is_some_and(|o| o.iter().any(|x| x == "skip" || x == "skip_serializing")))
                .map(|(i, ((n, _), v))| format!("(\"{}\".to_string(), {v}.serialize())", serde_name(n, opts.get(i).copied().unwrap_or(&[]))))
                .collect();
            format!("serde::Content::Map(vec![{}])", entries.join(", "))
        }
    }
}

fn serialize(item: &ItemKind) -> String {
    let body = match item {
        ItemKind::Struct(s) => {
            let shape = struct_shape(&s.body);
            serialize_shape(&shape, &field_values(&shape, Some("self"), ""), &field_opts(item, None))
        }
        // Externally tagged: `"Unit"`, `{"Variant": <fields>}`.
        ItemKind::Enum(e) => match_arms(e, |v, shape| match shape {
            Shape::Unit => format!("serde::Content::Str(\"{}\".to_string())", v.name),
            _ => format!("serde::Content::Map(vec![(\"{}\".to_string(), {})])", v.name,
                serialize_shape(shape, &field_values(shape, None, "a"), &field_opts(item, Some(&v.name)))),
        }),
        _ => unreachable!(),
    };
    format!("fn serialize(self) -> serde::Content {{ {body} }}")
}

/// Rebuild `path`'s fields from the `Content` in `c`.
fn deserialize_shape(path: &str, what: &str, shape: &Shape, opts: &[&[String]]) -> String {
    let de = |ty: &Type, value: &str| format!("<{} as Deserialize>::deserialize({value})?", type_src(ty));
    match shape {
        Shape::Unit => format!("match c {{ serde::Content::Unit => Ok({path}), other => Err(serde::invalid_type(other, \"{what}\")) }}"),
        Shape::Tuple(tys) if tys.len() == 1 => format!("Ok({path}({}))", de(tys[0], "c")),
        Shape::Tuple(tys) => {
            let n = tys.len();
            let fields: Vec<String> = tys.iter().enumerate().map(|(i, t)| de(t, &format!("items[{i}]"))).collect();
            format!("match c {{ serde::Content::Seq(items) if items.len() == {n} => Ok({path}({})), \
                serde::Content::Seq(items) => Err(format!(\"invalid length {{}}, expected {what}\", items.len())), \
                other => Err(serde::invalid_type(other, \"{what}\")) }}", fields.join(", "))
        }
        Shape::Named(fields) => {
            let inits: Vec<String> = fields.iter().enumerate().map(|(i, (n, t))| {
                let o = opts.get(i).copied().unwrap_or(&[]);
                let key = serde_name(n, o);
                // A missing field is its default with `#[serde(default)]`,
                // and otherwise what `null` gives (`None` for an `Option`).
                let missing = if o.iter().any(|x| x == "default" || x == "skip" || x == "skip_deserializing") {
                    format!("<{} as Default>::default()", type_src(t))
                } else {
                    format!("<{} as Deserialize>::deserialize(serde::Content::Unit).map_err(|_e| \"missing field `{key}`\".to_string())?", type_src(t))
                };
                format!("{n}: match c.field(\"{key}\") {{ Some(v) => {}, None => {missing} }}", de(t, "v"))
            }).collect();
            format!("match c {{ serde::Content::Map(_m) => Ok({path} {{ {} }}), other => Err(serde::invalid_type(other, \"{what}\")) }}", inits.join(", "))
        }
    }
}

fn deserialize(item: &ItemKind) -> String {
    let body = match item {
        ItemKind::Struct(s) => {
            let shape = struct_shape(&s.body);
            deserialize_shape(&s.name, &format!("struct {}", s.name), &shape, &field_opts(item, None))
        }
        ItemKind::Enum(e) => {
            let mut arms: Vec<String> = Vec::new();
            for v in &e.variants {
                let shape = variant_shape(&v.payload);
                let path = format!("{}::{}", e.name, v.name);
                match shape {
                    Shape::Unit => arms.push(format!("serde::Content::Str(tag) if tag == \"{}\" => Ok({path})", v.name)),
                    _ => {
                        let inner = deserialize_shape(&path, &format!("variant {path}"), &shape, &field_opts(item, Some(&v.name)));
                        arms.push(format!("serde::Content::Map(entries) if entries.len() == 1 && entries[0].0 == \"{}\" => {{ let c = entries[0].1.clone(); {inner} }}", v.name));
                    }
                }
            }
            arms.push(format!("other => Err(format!(\"unknown variant of enum {} in {{}}\", other.unexpected()))", e.name));
            format!("match c {{ {} }}", arms.join(", "))
        }
        _ => unreachable!(),
    };
    format!("fn deserialize(c: serde::Content) -> Result<Self, String> {{ {body} }}")
}
