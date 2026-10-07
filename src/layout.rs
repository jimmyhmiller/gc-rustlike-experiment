//! Layout registry: maps concrete semantic types ([`Ty`]) to core-IR
//! representations ([`Repr`]) and heap/value layouts, building each shape on
//! demand and assigning stable ids.
//!
//! This is the bridge from the type system to the GC: a reference type becomes
//! a [`Layout`] with **pointer fields first** (so `gc::scan_object` traces
//! exactly the right slots), and a `value` type becomes an inline
//! [`ValueLayout`]. The registry is monomorphic-only — every `Ty` it sees must
//! be ground (no `Ty::Var`/`Ty::Infer`); the monomorphizer guarantees this.

use crate::core::*;
use crate::gc::{FieldMeta, FieldTy, ScalarKind, TypeKind, TypeMeta, VariantMeta};
use crate::types::{Prim, Ty, TyCtx};
use crate::ast::{StructBody, VariantPayload};
use std::collections::HashMap;

/// Object header size in bytes (the GC `Full` header). Reflection field offsets
/// are absolute from the object pointer, so they all include this. Kept in sync
/// with `codegen::Self::HEADER` and `gc::Full::SIZE` (both 16).
const HEADER: u16 = 16;

/// Map a compiler `ScalarRepr` to the runtime reflection `ScalarKind`.
pub(crate) fn scalar_kind(s: ScalarRepr) -> ScalarKind {
    match s {
        ScalarRepr::I8 => ScalarKind::I8,
        ScalarRepr::I16 => ScalarKind::I16,
        ScalarRepr::I32 => ScalarKind::I32,
        ScalarRepr::I64 => ScalarKind::I64,
        ScalarRepr::U8 => ScalarKind::U8,
        ScalarRepr::U16 => ScalarKind::U16,
        ScalarRepr::U32 => ScalarKind::U32,
        ScalarRepr::U64 => ScalarKind::U64,
        ScalarRepr::F32 => ScalarKind::F32,
        ScalarRepr::F64 => ScalarKind::F64,
        ScalarRepr::Bool => ScalarKind::Bool,
        ScalarRepr::Char => ScalarKind::Char,
        ScalarRepr::Ptr => ScalarKind::Ptr,
    }
}

/// Reflection metadata for a builtin / opaque / varlen layout (String, Array,
/// Vec, closure env): a name with no introspectable named fields.
fn opaque_meta(name: &str) -> TypeMeta {
    TypeMeta { type_id: 0, name: name.to_string(), kind: TypeKind::Opaque }
}

pub struct LayoutRegistry<'a> {
    ctx: &'a TyCtx,
    /// Interning: a canonical key for a `Ty` → its assigned id.
    ref_ids: HashMap<String, LayoutId>,
    value_ids: HashMap<String, ValueId>,
    pub layouts: Vec<Layout>,
    pub values: Vec<ValueLayout>,
}

#[derive(Debug)]
pub struct LayoutError(pub String);
type R<T> = Result<T, LayoutError>;

impl<'a> LayoutRegistry<'a> {
    pub fn new(ctx: &'a TyCtx) -> Self {
        LayoutRegistry { ctx, ref_ids: HashMap::new(), value_ids: HashMap::new(), layouts: vec![], values: vec![] }
    }

    /// The [`Repr`] of a ground type, registering any needed layouts.
    pub fn repr(&mut self, ty: &Ty) -> R<Repr> {
        match ty {
            Ty::Prim(Prim::Unit) => Ok(Repr::Unit),
            Ty::Prim(Prim::Str) => {
                // String is a built-in reference type (varlen bytes).
                Ok(Repr::Ref(self.string_layout()))
            }
            Ty::Prim(p) => Ok(Repr::Scalar(scalar_of(*p).expect("non-unit prim scalar"))),
            Ty::Named { name, args } => self.named_repr(name, args),
            Ty::Array(elem, n) => {
                // Fixed array → a reference object holding `n` inline elements.
                let er = self.repr(elem)?;
                Ok(Repr::Ref(self.array_layout(&er, *n)?))
            }
            Ty::Tuple(elems) => {
                // Tuples are value aggregates.
                let reprs: Vec<Repr> = elems.iter().map(|e| self.repr(e)).collect::<R<_>>()?;
                Ok(Repr::Value(self.tuple_value(&reprs)))
            }
            Ty::Fn { .. } => {
                // A closure value is a reference to its environment object; the
                // closure repr is the env layout (code ptr lives in the layout).
                Ok(Repr::Ref(self.closure_placeholder()))
            }
            // A C callback is a raw function pointer.
            Ty::ExternFn { .. } => Ok(Repr::Scalar(ScalarRepr::Ptr)),
            Ty::Var(v) => Err(LayoutError(format!("non-ground type variable `{}` reached layout", v))),
            Ty::Infer(_) => Err(LayoutError("inference hole reached layout".into())),
        }
    }

    fn named_repr(&mut self, name: &str, args: &[Ty]) -> R<Repr> {
        // Builtin generic containers.
        match name {
            "Array" => {
                let er = self.repr(&args[0])?;
                return Ok(Repr::Ref(self.array_for(&er)?));
            }
            "Vec" => {
                let er = self.repr(&args[0])?;
                return Ok(Repr::Ref(self.vec_layout(&er)));
            }
            "Option" | "Result" => {
                // Built-in enums. Honour a `#[value]` declaration (the prelude
                // declares `Option` as a value enum so `array_get`/`vec_get` can
                // return it without per-access heap allocation); otherwise fall
                // back to the structural reference-enum layout.
                if let Some(e) = self.ctx.enums.get(name).cloned() {
                    if e.is_value {
                        return Ok(Repr::Value(self.value_enum(name, &e, args)?));
                    }
                }
                return Ok(Repr::Ref(self.builtin_enum_layout(name, args)?));
            }
            _ => {}
        }
        // User struct or enum.
        if let Some(s) = self.ctx.structs.get(name).cloned() {
            let is_value = s.is_value;
            if is_value {
                Ok(Repr::Value(self.value_struct(name, &s, args)?))
            } else {
                Ok(Repr::Ref(self.ref_struct(name, &s, args)?))
            }
        } else if let Some(e) = self.ctx.enums.get(name).cloned() {
            if e.is_value {
                Ok(Repr::Value(self.value_enum(name, &e, args)?))
            } else {
                Ok(Repr::Ref(self.ref_enum(name, &e, args)?))
            }
        } else {
            Err(LayoutError(format!("unknown named type `{}`", name)))
        }
    }

    // ---- key construction (monomorphic interning) -------------------------
    fn key(name: &str, args: &[Ty]) -> String {
        if args.is_empty() {
            name.to_string()
        } else {
            let a: Vec<String> = args.iter().map(ty_key).collect();
            format!("{}<{}>", name, a.join(","))
        }
    }

    // ---- reference structs -------------------------------------------------
    fn ref_struct(&mut self, name: &str, s: &crate::ast::StructDef, args: &[Ty]) -> R<LayoutId> {
        let key = Self::key(name, args);
        if let Some(id) = self.ref_ids.get(&key) {
            return Ok(*id);
        }
        // Reserve id up-front for recursive types.
        let id = self.layouts.len() as LayoutId;
        self.ref_ids.insert(key.clone(), id);
        self.layouts.push(placeholder_layout(&key));

        let field_tys = struct_field_tys(s, args, self.ctx)?;
        let field_names = struct_field_names(s);
        let layout = self.build_ref_layout(&key, &field_names, &field_tys)?;
        self.layouts[id as usize] = layout;
        Ok(id)
    }

    fn ref_enum(&mut self, name: &str, e: &crate::ast::EnumDef, args: &[Ty]) -> R<LayoutId> {
        let key = Self::key(name, args);
        if let Some(id) = self.ref_ids.get(&key) {
            return Ok(*id);
        }
        let id = self.layouts.len() as LayoutId;
        self.ref_ids.insert(key.clone(), id);
        self.layouts.push(placeholder_layout(&key));

        // A reference enum is one heap object: [header with u32 tag][union of
        // variant payloads]. We give it the max over variants of (ptr_fields, raw_bytes)
        // so any variant fits. Pointer payload
        // fields go in the pointer region (traced); a variant with fewer ptr
        // fields than the max simply leaves trailing ptr slots null (safe to
        // trace — null is skipped).
        let mut max_ptrs = 0u16;
        let mut max_raw = 0u16;
        for v in &e.variants {
            let payload_tys = variant_payload_tys(v, &e.generics, args, self.ctx)?;
            let (ptrs, raw) = self.count_fields(&payload_tys)?;
            max_ptrs = max_ptrs.max(ptrs);
            max_raw = max_raw.max(raw);
        }
        if HEADER as u32 + max_ptrs as u32 * 8 + max_raw as u32 > u16::MAX as u32 {
            return Err(LayoutError("enum payload exceeds layout size limit".into()));
        }
        // The tag is stored in the Full header, outside the payload regions.
        let raw_bytes = max_raw;
        // Reflection metadata: one VariantMeta per source variant, with payload
        // field offsets matching codegen's placement. Tag = variant index.
        let mut variants_meta = Vec::with_capacity(e.variants.len());
        for (vtag, v) in e.variants.iter().enumerate() {
            let payload_tys = variant_payload_tys(v, &e.generics, args, self.ctx)?;
            let reprs: Vec<Repr> = payload_tys.iter().map(|t| self.repr(t)).collect::<R<_>>()?;
            let fnames = variant_field_names(v);
            let fields = self.enum_variant_fields(&reprs, &fnames, max_ptrs);
            variants_meta.push(VariantMeta { name: v.name.clone(), tag: vtag as u32, fields });
        }
        let meta = TypeMeta {
            type_id: 0,
            name: key.clone(),
            kind: TypeKind::Enum { tag_offset: crate::gc::Full::ENUM_TAG_OFFSET as u16, variants: variants_meta },
        };
        let layout = Layout {
            ptr_fields: max_ptrs,
            raw_bytes,
            varlen: VarLen::None,
            // Fields are described per variant; the header tag is not a payload field.
            field_map: vec![],
            name: key,
            elem_stride: 0, element_box: None,
            interior_ptrs: vec![],
            meta,
        };
        self.layouts[id as usize] = layout;
        Ok(id)
    }

    fn builtin_enum_layout(&mut self, name: &str, args: &[Ty]) -> R<LayoutId> {
        // Option<T> = { None, Some(T) }; Result<T,E> = { Ok(T), Err(E) }.
        let key = Self::key(name, args);
        if let Some(id) = self.ref_ids.get(&key) {
            return Ok(*id);
        }
        let id = self.layouts.len() as LayoutId;
        self.ref_ids.insert(key.clone(), id);
        self.layouts.push(placeholder_layout(&key));

        let variants: Vec<Vec<Ty>> = match name {
            "Option" => vec![vec![], vec![args[0].clone()]],
            "Result" => vec![vec![args[0].clone()], vec![args[1].clone()]],
            _ => unreachable!(),
        };
        let variant_names: &[&str] = match name {
            "Option" => &["None", "Some"],
            "Result" => &["Ok", "Err"],
            _ => unreachable!(),
        };
        let mut max_ptrs = 0u16;
        let mut max_raw = 0u16;
        for vtys in &variants {
            let (ptrs, raw) = self.count_fields(vtys)?;
            max_ptrs = max_ptrs.max(ptrs);
            max_raw = max_raw.max(raw);
        }
        if HEADER as u32 + max_ptrs as u32 * 8 + max_raw as u32 > u16::MAX as u32 {
            return Err(LayoutError("enum payload exceeds layout size limit".into()));
        }
        let mut variants_meta = Vec::with_capacity(variants.len());
        for (vtag, vtys) in variants.iter().enumerate() {
            let reprs: Vec<Repr> = vtys.iter().map(|t| self.repr(t)).collect::<R<_>>()?;
            let fnames: Vec<String> = (0..reprs.len()).map(|i| i.to_string()).collect();
            let fields = self.enum_variant_fields(&reprs, &fnames, max_ptrs);
            variants_meta.push(VariantMeta { name: variant_names[vtag].to_string(), tag: vtag as u32, fields });
        }
        let meta = TypeMeta {
            type_id: 0,
            name: key.clone(),
            kind: TypeKind::Enum { tag_offset: crate::gc::Full::ENUM_TAG_OFFSET as u16, variants: variants_meta },
        };
        let layout = Layout {
            ptr_fields: max_ptrs,
            raw_bytes: max_raw,
            varlen: VarLen::None,
            field_map: vec![],
            name: key,
            elem_stride: 0, element_box: None,
            interior_ptrs: vec![],
            meta,
        };
        self.layouts[id as usize] = layout;
        Ok(id)
    }

    // ---- value aggregates --------------------------------------------------
    fn value_struct(&mut self, name: &str, s: &crate::ast::StructDef, args: &[Ty]) -> R<ValueId> {
        let key = Self::key(name, args);
        if let Some(id) = self.value_ids.get(&key) {
            return Ok(*id);
        }
        let field_tys = struct_field_tys(s, args, self.ctx)?;
        let fields: Vec<Repr> = field_tys.iter().map(|t| self.repr(t)).collect::<R<_>>()?;
        let (size, align) = value_size_align(&fields, &self.values);
        let field_names = struct_field_names(s);
        let id = self.values.len() as ValueId;
        self.values.push(ValueLayout { name: key.clone(), variants: None, fields, field_names, size, align });
        self.value_ids.insert(key, id);
        Ok(id)
    }

    fn value_enum(&mut self, name: &str, e: &crate::ast::EnumDef, args: &[Ty]) -> R<ValueId> {
        let key = Self::key(name, args);
        if let Some(id) = self.value_ids.get(&key) {
            return Ok(*id);
        }
        let mut variants = Vec::new();
        for v in &e.variants {
            let ptys = variant_payload_tys(v, &e.generics, args, self.ctx)?;
            let fields: Vec<Repr> = ptys.iter().map(|t| self.repr(t)).collect::<R<_>>()?;
            variants.push(ValueVariant { name: v.name.clone(), fields });
        }
        // Pointers-first layout (mirrors reference enums): `max_ptrs` leading GC
        // slots shared across variants, then the u32 tag, then the raw region
        // (scalar + value payloads with references split into the leading slots). Embedded refs land at fixed offsets
        // (`0, 8, …`) regardless of the active variant, so the GC can trace them
        // with a static interior-pointer list. `max_ptrs == 0` degenerates to the
        // compact `{ tag, padding, raw }` form. Raw payloads are 8-aligned.
        let max_ptrs = value_enum_max_ptrs(&variants, &self.values) as u32;
        let mut max_raw = 0u32;
        let mut align = 8;
        for vv in &variants {
            let mut raw = 0u32;
            for f in &vv.fields {
                match f {
                    Repr::Ref(_) | Repr::Unit => {}
                    Repr::Scalar(s) => {
                        let b = (s.bits().max(8) / 8).max(1);
                        raw = align_up(raw, b) + b;
                        align = align.max(b);
                    }
                    Repr::Value(vid) => {
                        let v = &self.values[*vid as usize];
                        raw = align_up(raw, v.align) + v.size;
                        align = align.max(v.align);
                    }
                }
            }
            max_raw = max_raw.max(raw);
        }
        let size = align_up(max_ptrs * 8 + 8 + max_raw, align);
        let id = self.values.len() as ValueId;
        self.values.push(ValueLayout { name: key.clone(), variants: Some(variants), fields: vec![], field_names: vec![], size, align });
        self.value_ids.insert(key, id);
        Ok(id)
    }

    fn tuple_value(&mut self, reprs: &[Repr]) -> ValueId {
        let key = format!("({})", reprs.iter().map(repr_key).collect::<Vec<_>>().join(","));
        if let Some(id) = self.value_ids.get(&key) {
            return *id;
        }
        let (size, align) = value_size_align(reprs, &self.values);
        let field_names: Vec<String> = (0..reprs.len()).map(|i| i.to_string()).collect();
        let id = self.values.len() as ValueId;
        self.values.push(ValueLayout { name: key.clone(), variants: None, fields: reprs.to_vec(), field_names, size, align });
        self.value_ids.insert(key, id);
        id
    }

    // ---- built-in reference layouts ---------------------------------------
    fn string_layout(&mut self) -> LayoutId {
        self.intern_ref("String", Layout {
            ptr_fields: 0, raw_bytes: 0, varlen: VarLen::Bytes,
            field_map: vec![], name: "String".into(), elem_stride: 0, element_box: None,
            interior_ptrs: vec![],
            meta: opaque_meta("String"),
        })
    }
    fn vec_layout(&mut self, elem: &Repr) -> LayoutId {
        // A Vec is a small header object {len, cap, ptr-to-backing}; for v0 we
        // model it as a reference with a pointer to a varlen backing array.
        let key = format!("Vec<{}>", repr_key(elem));
        let traced = matches!(elem, Repr::Ref(_));
        self.intern_ref(&key, Layout {
            ptr_fields: 1, // backing array pointer
            raw_bytes: 16, // len + cap
            varlen: VarLen::None,
            field_map: vec![
                FieldLoc::Ptr { idx: 0 },
                FieldLoc::Raw { offset: 0, repr: ScalarRepr::U64 },
                FieldLoc::Raw { offset: 8, repr: ScalarRepr::U64 },
            ],
            name: if traced { format!("{key}#traced") } else { key.clone() },
            elem_stride: 0, element_box: None,
            interior_ptrs: vec![],
            meta: opaque_meta(&key),
        })
    }
    /// A varlen array layout for the given element repr. Pointer elements use a
    /// traced `Values` tail; scalar elements use an untraced `Bytes` tail whose
    /// element stride is the scalar's byte size. Value elements use traced boxes.
    pub fn array_for(&mut self, elem: &Repr) -> R<LayoutId> {
        let key = format!("Array<{}>", repr_key(elem));
        if let Some(id) = self.ref_ids.get(&key) { return Ok(*id); }
        let element_box = if let Repr::Value(vid) = elem {
            let name = format!("<array-element:{}>", self.values[*vid as usize].name);
            let size = self.values[*vid as usize].size;
            if size > u16::MAX as u32 - HEADER as u32 { return Err(LayoutError("array value element exceeds pointer-offset limit".into())); }
            let raw_bytes = u16::try_from(size).map_err(|_| LayoutError("array value element exceeds layout size limit".into()))?;
            let mut interior_ptrs = Vec::new();
            self.value_ref_offsets(*vid, HEADER, &mut interior_ptrs)?;
            Some(self.intern_ref(&name, Layout {
                ptr_fields: 0, raw_bytes, varlen: VarLen::None,
                field_map: vec![FieldLoc::ValueAt { offset: 0, value: *vid }],
                name: name.clone(), elem_stride: 0, element_box: None,
                interior_ptrs, meta: opaque_meta(&name),
            }))
        } else { None };
        let traced = matches!(elem, Repr::Ref(_) | Repr::Value(_));
        let stride = match elem { Repr::Scalar(s) => (s.bits().max(8) / 8) as u16, _ => 8 };
        let id = self.layouts.len() as LayoutId;
        self.layouts.push(Layout {
            ptr_fields: 0, raw_bytes: 0,
            varlen: if traced { VarLen::Values } else { VarLen::Bytes },
            field_map: vec![], name: key.clone(), elem_stride: stride,
            element_box, interior_ptrs: vec![], meta: opaque_meta(&key),
        });
        self.ref_ids.insert(key, id);
        Ok(id)
    }

    fn array_layout(&mut self, elem: &Repr, n: u64) -> R<LayoutId> {
        let key = format!("[{};{}]", repr_key(elem), n);
        let dynamic = self.array_for(elem)?;
        let mut layout = self.layouts[dynamic as usize].clone();
        layout.name = key.clone();
        layout.meta = opaque_meta(&key);
        Ok(self.intern_ref(&key, layout))
    }
    fn closure_placeholder(&mut self) -> LayoutId {
        // Filled per-closure during closure lowering; a generic env placeholder.
        self.intern_ref("<closure-env>", placeholder_layout("<closure-env>"))
    }

    /// Build a closure-environment layout: pointer captures first (traced),
    /// then a raw section holding [code_ptr: u64][scalar captures...]. The
    /// returned layout's `field_map` is empty (codegen addresses by section
    /// offset using `capture_kinds`). `key` makes distinct closures distinct.
    pub fn closure_env(&mut self, key: &str, captures: &[Repr]) -> R<LayoutId> {
        let ptr_fields = captures.iter().filter(|repr| matches!(repr, Repr::Ref(_))).count() as u16;
        let mut raw_bytes = 8u32;
        let mut interior_ptrs = Vec::new();
        for capture in captures {
            match capture {
                Repr::Scalar(s) => {
                    let size = s.bits().max(8) / 8;
                    raw_bytes = align_up(raw_bytes, size) + size;
                }
                Repr::Value(id) => {
                    raw_bytes = align_up(raw_bytes, 8);
                    if HEADER as u32 + ptr_fields as u32 * 8 + raw_bytes + self.values[*id as usize].size > u16::MAX as u32 { return Err(LayoutError("closure capture exceeds pointer-offset limit".into())); }
                    let base = u16::try_from(HEADER as u32 + ptr_fields as u32 * 8 + raw_bytes).map_err(|_| LayoutError("closure capture exceeds layout size limit".into()))?;
                    self.value_ref_offsets(*id, base, &mut interior_ptrs)?;
                    raw_bytes += self.values[*id as usize].size;
                }
                _ => {}
            }
        }
        let raw_bytes = u16::try_from(align_up(raw_bytes, 8)).map_err(|_| LayoutError("closure captures exceed layout size limit".into()))?;
        let id = self.layouts.len() as LayoutId;
        self.layouts.push(Layout { ptr_fields, raw_bytes, varlen: VarLen::None,
            field_map: vec![], name: key.to_string(), elem_stride: 0, element_box: None,
            interior_ptrs, meta: opaque_meta(key) });
        Ok(id)
    }

    fn intern_ref(&mut self, key: &str, layout: Layout) -> LayoutId {
        if let Some(id) = self.ref_ids.get(key) {
            return *id;
        }
        let id = self.layouts.len() as LayoutId;
        self.layouts.push(layout);
        self.ref_ids.insert(key.to_string(), id);
        id
    }

    // ---- field counting + layout building ---------------------------------
    fn count_fields(&mut self, tys: &[Ty]) -> R<(u16, u16)> {
        let mut ptrs = 0u32;
        let mut raw = 0u32;
        for t in tys {
            match self.repr(t)? {
                Repr::Ref(_) => ptrs += 1,
                Repr::Scalar(s) => {
                    let size = s.bits().max(8) / 8;
                    raw = align_up(raw, size) + size;
                }
                Repr::Value(id) => {
                    let value = &self.values[id as usize];
                    // Reference enum payloads use the same leading pointer
                    // slots as value enums, including nested value references.
                    let mut offsets = Vec::new();
                    self.value_ref_offsets(id, 0, &mut offsets)?;
                    ptrs += offsets.len() as u32;
                    raw = align_up(raw, 8) + value.size;
                }
                Repr::Unit => {}
            }
            if HEADER as u32 + ptrs * 8 + align_up(raw, 8) > u16::MAX as u32 {
                return Err(LayoutError("enum payload exceeds layout size limit".into()));
            }
        }
        Ok((ptrs as u16, align_up(raw, 8) as u16))
    }

    fn build_ref_layout(&mut self, name: &str, field_names: &[String], field_tys: &[Ty]) -> R<Layout> {
        // Pointer fields first (traced), then raw scalar/value bytes.
        let mut ptr_idx = 0u16;
        let mut raw_off = 0u16;
        let mut field_map = Vec::with_capacity(field_tys.len());
        // First pass: place pointers.
        let reprs: Vec<Repr> = field_tys.iter().map(|t| self.repr(t)).collect::<R<_>>()?;
        for r in &reprs {
            if let Repr::Ref(_) = r {
                field_map.push(FieldLoc::Ptr { idx: ptr_idx });
                ptr_idx += 1;
            } else {
                field_map.push(FieldLoc::Raw { offset: 0, repr: ScalarRepr::I64 }); // patched below
            }
        }
        // Second pass: place raw fields, patching the placeholders.
        for (i, r) in reprs.iter().enumerate() {
            match r {
                Repr::Ref(_) | Repr::Unit => {}
                Repr::Scalar(s) => {
                    let sz = (s.bits().max(8) / 8) as u16;
                    raw_off = align_up(raw_off as u32, sz as u32) as u16;
                    field_map[i] = FieldLoc::Raw { offset: raw_off, repr: *s };
                    raw_off += sz;
                }
                Repr::Value(vid) => {
                    // A flattened value aggregate lives inline in the raw region.
                    // References embedded in it are recorded as `interior_ptrs`
                    // below so the GC traces them; value enums carrying refs are
                    // still rejected there (their union offsets are tag-dependent).
                    let sz = self.values[*vid as usize].size as u16;
                    raw_off = align_up(raw_off as u32, 8) as u16;
                    field_map[i] = FieldLoc::ValueAt { offset: raw_off, value: *vid };
                    raw_off += sz;
                }
            }
        }
        // Reflection metadata: absolute byte offsets (header included). Pointer
        // fields live in the leading slots; raw fields after the pointer region.
        let ptr_total = ptr_idx;
        let mut meta_fields = Vec::with_capacity(reprs.len());
        for (i, r) in reprs.iter().enumerate() {
            let fname = field_names.get(i).cloned().unwrap_or_else(|| i.to_string());
            match (r, &field_map[i]) {
                (Repr::Ref(lid), FieldLoc::Ptr { idx }) => meta_fields.push(FieldMeta {
                    name: fname,
                    offset: HEADER + idx * 8,
                    ty: FieldTy::Ref(*lid as u16),
                }),
                (Repr::Scalar(s), FieldLoc::Raw { offset, .. }) => meta_fields.push(FieldMeta {
                    name: fname,
                    offset: HEADER + ptr_total * 8 + offset,
                    ty: FieldTy::Scalar(scalar_kind(*s)),
                }),
                (Repr::Value(vid), FieldLoc::ValueAt { offset, .. }) => meta_fields.push(FieldMeta {
                    name: fname,
                    offset: HEADER + ptr_total * 8 + offset,
                    ty: FieldTy::Value(*vid as u16),
                }),
                // Unit fields are zero-size (no storage); skip.
                _ => {}
            }
        }
        let meta = TypeMeta {
            type_id: 0,
            name: name.to_string(),
            kind: TypeKind::Struct { fields: meta_fields },
        };
        // Interior pointers: GC refs embedded in flattened value fields, at
        // absolute offsets, so `gc::scan_object` traces them. Nested enum references use the shared leading pointer slots.
        let mut interior_ptrs = Vec::new();
        for (i, r) in reprs.iter().enumerate() {
            if let (Repr::Value(vid), FieldLoc::ValueAt { offset, .. }) = (r, &field_map[i]) {
                let base = HEADER + ptr_total * 8 + offset;
                self.value_ref_offsets(*vid, base, &mut interior_ptrs)?;
            }
        }
        Ok(Layout {
            ptr_fields: ptr_idx,
            raw_bytes: align_up(raw_off as u32, 8) as u16,
            varlen: VarLen::None,
            field_map,
            name: name.to_string(),
            elem_stride: 0, element_box: None,
            interior_ptrs,
            meta,
        })
    }

    /// Collect byte offsets of GC references inside a flattened value aggregate
    /// `vid`, relative to its start plus `base`, into `out`. Recurses into nested
    /// value structs, mirroring `value_size_align`'s (and thus the LLVM struct's)
    /// field placement so the offsets match where codegen actually stores the refs.
    ///
    /// Value enums split direct and nested references into shared leading slots,
    /// so their offsets remain independent of the active variant.
    fn value_ref_offsets(&self, vid: ValueId, base: u16, out: &mut Vec<u16>) -> R<()> {
        let vl = &self.values[vid as usize];
        if let Some(variants) = &vl.variants {
            let max_ptrs = crate::core::value_enum_max_ptrs(variants, &self.values);
            for k in 0..max_ptrs {
                out.push(base + k * 8);
            }
            return Ok(());
        }
        let mut off = 0u32;
        for f in &vl.fields {
            let (sz, align) = match f {
                Repr::Unit => (0u32, 1u32),
                Repr::Scalar(s) => {
                    let b = (s.bits().max(8) / 8).max(1);
                    (b, b)
                }
                Repr::Ref(_) => (8, 8),
                Repr::Value(sub) => {
                    (self.values[*sub as usize].size, self.values[*sub as usize].align)
                }
            };
            off = align_up(off, align);
            match f {
                Repr::Ref(_) => out.push(base + off as u16),
                Repr::Value(sub) => self.value_ref_offsets(*sub, base + off as u16, out)?,
                _ => {}
            }
            off += sz;
        }
        Ok(())
    }


    /// Reflection field metadata for one enum variant, matching codegen's
    /// payload placement (`gen_alloc` / `load_enum_payload`): pointer payloads
    /// fill the shared pointer region (slots `0..`) in declaration order; scalar
    /// payloads fill the raw region after the pointer slots (at `HEADER +
    /// max_ptrs*8`). Offsets are absolute (header included). Value payloads
    /// advance both cursors but remain omitted from reflection: their nested
    /// references live separately in leading slots, which a contiguous ValueMeta
    /// cannot describe. Unit payloads have no storage and are also omitted.
    fn enum_variant_fields(
        &self,
        reprs: &[Repr],
        names: &[String],
        max_ptrs: u16,
    ) -> Vec<FieldMeta> {
        let mut ptr_slot = 0u16;
        let mut raw_cursor = HEADER + max_ptrs * 8;
        let mut out = Vec::new();
        for (i, r) in reprs.iter().enumerate() {
            let fname = names.get(i).cloned().unwrap_or_else(|| i.to_string());
            match r {
                Repr::Ref(lid) => {
                    let off = HEADER + ptr_slot * 8;
                    ptr_slot += 1;
                    out.push(FieldMeta { name: fname, offset: off, ty: FieldTy::Ref(*lid as u16) });
                }
                Repr::Scalar(s) => {
                    let sz = (s.bits().max(8) / 8) as u16;
                    raw_cursor = align_up(raw_cursor as u32, sz as u32) as u16;
                    let off = raw_cursor;
                    raw_cursor += sz;
                    out.push(FieldMeta { name: fname, offset: off, ty: FieldTy::Scalar(scalar_kind(*s)) });
                }
                Repr::Value(id) => {
                    raw_cursor = align_up(raw_cursor as u32, 8) as u16;
                    raw_cursor += self.values[*id as usize].size as u16;
                    let mut offsets = Vec::new();
                    // Already validated when the payload layout was built.
                    self.value_ref_offsets(*id, 0, &mut offsets).expect("validated value layout");
                    ptr_slot += offsets.len() as u16;
                }
                Repr::Unit => {}
            }
        }
        out
    }
}

// ---- free helpers ----------------------------------------------------------

fn placeholder_layout(name: &str) -> Layout {
    Layout { ptr_fields: 0, raw_bytes: 0, varlen: VarLen::None, field_map: vec![], name: name.to_string(), elem_stride: 0, element_box: None, interior_ptrs: vec![], meta: opaque_meta(name) }
}

/// Source-order field names of a struct, for reflection metadata. Tuple structs
/// use positional names (`"0"`, `"1"`, …); unit structs have none.
fn struct_field_names(s: &crate::ast::StructDef) -> Vec<String> {
    match &s.body {
        StructBody::Named(fields) => fields.iter().map(|f| f.name.clone()).collect(),
        StructBody::Tuple(tys) => (0..tys.len()).map(|i| i.to_string()).collect(),
        StructBody::Unit => vec![],
    }
}

/// Source-order payload field names of an enum variant, for reflection metadata.
/// Tuple payloads use positional names (`"0"`, `"1"`, …).
fn variant_field_names(v: &crate::ast::VariantDef) -> Vec<String> {
    match &v.payload {
        VariantPayload::None => vec![],
        VariantPayload::Tuple(tys) => (0..tys.len()).map(|i| i.to_string()).collect(),
        VariantPayload::Named(fields) => fields.iter().map(|f| f.name.clone()).collect(),
    }
}

fn struct_field_tys(s: &crate::ast::StructDef, args: &[Ty], ctx: &TyCtx) -> R<Vec<Ty>> {
    let gparams: Vec<String> = s.generics.params.iter().map(|p| p.name.clone()).collect();
    let subst = build_subst(&gparams, args);
    match &s.body {
        StructBody::Named(fields) => fields.iter().map(|f| surface_to_ground(&f.ty, &subst, ctx)).collect(),
        StructBody::Tuple(tys) => tys.iter().map(|t| surface_to_ground(t, &subst, ctx)).collect(),
        StructBody::Unit => Ok(vec![]),
    }
}

fn variant_payload_tys(v: &crate::ast::VariantDef, generics: &crate::ast::Generics, args: &[Ty], ctx: &TyCtx) -> R<Vec<Ty>> {
    let gparams: Vec<String> = generics.params.iter().map(|p| p.name.clone()).collect();
    let subst = build_subst(&gparams, args);
    match &v.payload {
        VariantPayload::None => Ok(vec![]),
        VariantPayload::Tuple(tys) => tys.iter().map(|t| surface_to_ground(t, &subst, ctx)).collect(),
        VariantPayload::Named(fields) => fields.iter().map(|f| surface_to_ground(&f.ty, &subst, ctx)).collect(),
    }
}

fn build_subst(params: &[String], args: &[Ty]) -> HashMap<String, Ty> {
    params.iter().cloned().zip(args.iter().cloned()).collect()
}

/// Use the type checker's canonical name resolution before substitution, so
/// module-qualified fields and container elements receive the same layouts as
/// their checked types.
fn surface_to_ground(t: &crate::ast::Type, subst: &HashMap<String, Ty>, ctx: &TyCtx) -> R<Ty> {
    let params: Vec<String> = subst.keys().cloned().collect();
    let ty = crate::types::lower_type(t, &params, ctx).map_err(|e| LayoutError(e.msg))?;
    Ok(crate::types::subst_ty(&ty, subst))
}

fn scalar_of(p: Prim) -> Option<ScalarRepr> {
    Some(match p {
        Prim::I8 => ScalarRepr::I8, Prim::I16 => ScalarRepr::I16,
        Prim::I32 => ScalarRepr::I32, Prim::I64 => ScalarRepr::I64,
        Prim::U8 => ScalarRepr::U8, Prim::U16 => ScalarRepr::U16,
        Prim::U32 => ScalarRepr::U32, Prim::U64 => ScalarRepr::U64,
        Prim::F32 => ScalarRepr::F32, Prim::F64 => ScalarRepr::F64,
        Prim::Bool => ScalarRepr::Bool, Prim::Char => ScalarRepr::Char,
        Prim::RawPtr => ScalarRepr::Ptr,
        Prim::Str | Prim::Unit => return None,
    })
}

fn value_size_align(fields: &[Repr], values: &[ValueLayout]) -> (u32, u32) {
    let mut off = 0u32;
    let mut align = 1u32;
    for f in fields {
        let (sz, a) = match f {
            Repr::Unit => (0, 1),
            Repr::Scalar(s) => { let b = (s.bits().max(8) / 8).max(1); (b, b) }
            Repr::Ref(_) => (8, 8),
            Repr::Value(vid) => (values[*vid as usize].size, values[*vid as usize].align),
        };
        off = align_up(off, a) + sz;
        align = align.max(a);
    }
    (align_up(off, align.max(1)), align.max(1))
}

fn align_up(n: u32, a: u32) -> u32 {
    if a == 0 { n } else { (n + a - 1) & !(a - 1) }
}

fn ty_key(t: &Ty) -> String {
    match t {
        Ty::Prim(p) => format!("{:?}", p),
        Ty::Named { name, args } => if args.is_empty() { name.clone() } else {
            format!("{}<{}>", name, args.iter().map(ty_key).collect::<Vec<_>>().join(","))
        },
        Ty::Var(v) => format!("'{}", v),
        Ty::Array(e, n) => format!("[{};{}]", ty_key(e), n),
        Ty::Tuple(es) => format!("({})", es.iter().map(ty_key).collect::<Vec<_>>().join(",")),
        Ty::Fn { params, ret } => format!("fn({})->{}", params.iter().map(ty_key).collect::<Vec<_>>().join(","), ty_key(ret)),
        Ty::ExternFn { params, ret } => format!("extern fn({})->{}", params.iter().map(ty_key).collect::<Vec<_>>().join(","), ty_key(ret)),
        Ty::Infer(n) => format!("?{}", n),
    }
}

fn repr_key(r: &Repr) -> String {
    match r {
        Repr::Unit => "()".into(),
        Repr::Scalar(s) => format!("{:?}", s),
        Repr::Ref(id) => format!("ref{}", id),
        Repr::Value(id) => format!("val{}", id),
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
    fn value_struct_is_inline() {
        let c = ctx("#[value] struct Vec3 { x: f64, y: f64, z: f64 }");
        let mut reg = LayoutRegistry::new(&c);
        let r = reg.repr(&Ty::Named { name: "Vec3".into(), args: vec![] }).unwrap();
        let Repr::Value(id) = r else { panic!("Vec3 should be a value type") };
        let v = &reg.values[id as usize];
        assert_eq!(v.fields.len(), 3);
        assert_eq!(v.size, 24);
    }

    #[test]
    fn ref_struct_ptr_fields_first() {
        // a struct with a scalar and a reference field: ptr must be field 0 slot.
        let c = ctx("struct Node { val: i64, next: Node }");
        let mut reg = LayoutRegistry::new(&c);
        let r = reg.repr(&Ty::Named { name: "Node".into(), args: vec![] }).unwrap();
        let Repr::Ref(id) = r else { panic!("Node is a reference type") };
        let l = &reg.layouts[id as usize];
        assert_eq!(l.ptr_fields, 1, "next is a traced pointer");
        // val:i64 lives in raw region.
        assert!(matches!(l.field_map[0], FieldLoc::Raw { repr: ScalarRepr::I64, .. }));
        assert!(matches!(l.field_map[1], FieldLoc::Ptr { idx: 0 }));
    }

    #[test]
    fn generic_struct_monomorphizes_layout() {
        let c = ctx("struct Pair<A, B> { a: A, b: B }");
        let mut reg = LayoutRegistry::new(&c);
        let r = reg.repr(&Ty::Named {
            name: "Pair".into(),
            args: vec![Ty::Prim(Prim::I64), Ty::Prim(Prim::F64)],
        }).unwrap();
        let Repr::Ref(id) = r else { panic!() };
        let l = &reg.layouts[id as usize];
        assert_eq!(l.ptr_fields, 0);
        assert_eq!(l.raw_bytes, 16); // i64 + f64
    }

    #[test]
    fn option_layout() {
        let c = ctx("");
        let mut reg = LayoutRegistry::new(&c);
        let r = reg.repr(&Ty::Named { name: "Option".into(), args: vec![Ty::Prim(Prim::I64)] }).unwrap();
        assert!(matches!(r, Repr::Ref(_)));
    }
}
