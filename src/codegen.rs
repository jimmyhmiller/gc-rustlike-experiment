//! LLVM codegen (via inkwell): [`CoreProgram`] → an executable LLVM module.
//!
//! v0 slice: scalar functions (the fib class). Every compiled function takes a
//! `Thread*` as its first parameter — matching the GC ABI in `docs/gc.md` — even
//! when it doesn't allocate, so the calling convention is uniform once heap
//! allocation + GC frames are layered on. No frames are emitted yet for
//! allocation-free functions (they have no roots to track).

use crate::ast::{BinOp, UnOp};
use crate::core::*;

use inkwell::OptimizationLevel;
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::debug_info::{
    AsDIScope, DICompileUnit, DIFile, DIFlags, DIFlagsConstants, DIScope, DISubprogram, DIType,
    DWARFEmissionKind, DWARFSourceLanguage, DebugInfoBuilder,
};
use inkwell::module::{FlagBehavior, Module};
use inkwell::types::{AnyType, BasicMetadataTypeEnum, BasicType, BasicTypeEnum};
use inkwell::values::{AsValueRef, BasicValue, BasicValueEnum, FunctionValue, IntValue, PointerValue};
use inkwell::{AddressSpace, AtomicOrdering, FloatPredicate, IntPredicate};
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Copy)]
enum ForeignArg<'ctx> {
    Scalar,
    Direct(BasicTypeEnum<'ctx>),
    Indirect { byval: bool },
}

/// DWARF line-table emission state (debugger P2). Present only for AOT builds
/// (`gcr build`) — JIT-code DWARF needs the LLDB JIT-registration interface
/// (deferred). One [`DIFile`] per [`crate::lexer::SourceId`] (keyed by the
/// SourceMap index) so a span resolves to the RIGHT source file in the line
/// table — the multi-source analog of the source-id alloc-site fix.
struct DebugCx<'ctx> {
    di: DebugInfoBuilder<'ctx>,
    cu: DICompileUnit<'ctx>,
    /// `DIFile` per `SourceId` (index = source id; same order as `prog.sources`).
    files: Vec<DIFile<'ctx>>,
    /// `true` for **full** DWARF (debugger P3): emit local/parameter variable
    /// DIEs so `frame variable` shows source names + values. `false` =
    /// line-tables-only (P2): stepping + breakpoints only. Full debug also
    /// implies the AOT path skips optimization (so allocas/locals survive).
    full: bool,
    /// Native DWARF composite type per heap layout (`type_id` → struct DIE), so
    /// a `Ref` local renders as `*p = (Point) { x = 3, y = 4 }` natively in lldb
    /// — no Python pretty-printer. `None` for enum/opaque/varlen layouts (those
    /// fall back to an address). Empty unless `full`. Members sit at their
    /// absolute, header-included offsets (see `gc::reflect`), so dereferencing
    /// the pointer reads the right bytes.
    layout_types: Vec<Option<DIType<'ctx>>>,
}

/// How much DWARF a build emits. `None` for JIT / `emit llvm` (no debug info);
/// `LineTables` is the default `gcr build` (P2 — stepping + breakpoints, kept
/// even under O3); `Full` is `gcr build --debug` (P3 — local-variable
/// inspection, requires unoptimized codegen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugLevel {
    None,
    LineTables,
    Full,
}

impl DebugLevel {
    fn emits_dwarf(self) -> bool {
        !matches!(self, DebugLevel::None)
    }
    fn is_full(self) -> bool {
        matches!(self, DebugLevel::Full)
    }
}

#[derive(Debug)]
pub struct CodegenError(pub String);

/// A compiled, JIT-ready program.
pub struct Compiled<'ctx> {
    pub module: Module<'ctx>,
    pub entry_name: String,
    /// Allocation-site table (Target-1b), indexed by `site_id`. Installed into
    /// the heap via `Heap::set_alloc_sites` (JIT) or baked into the reflection
    /// blob (AOT) so a profile can name each site's `(function, type)`.
    pub alloc_sites: Vec<crate::gc::AllocSite>,
}

/// Codegen without DWARF (JIT / `emit llvm`).
pub fn codegen<'ctx>(
    ctx: &'ctx Context,
    prog: &CoreProgram,
) -> Result<Compiled<'ctx>, CodegenError> {
    codegen_with_debug(ctx, prog, DebugLevel::None)
}

/// Codegen, optionally emitting DWARF (debugger P2/P3). DWARF should be emitted
/// ONLY for AOT builds — it lands in the emitted object where `lldb`/
/// `llvm-dwarfdump` read it; the JIT can't use it without the LLDB
/// JIT-registration interface (deferred). [`DebugLevel::Full`] also emits
/// local-variable DIEs (P3) and must be paired with unoptimized codegen.
pub fn codegen_with_debug<'ctx>(
    ctx: &'ctx Context,
    prog: &CoreProgram,
    level: DebugLevel,
) -> Result<Compiled<'ctx>, CodegenError> {
    if prog.layouts.len() > u16::MAX as usize + 1 || prog.values.len() > u16::MAX as usize + 1 {
        return Err(CodegenError("layout/type count exceeds representable ID limit".into()));
    }
    let mut outlined = if level.is_full() {
        crate::codegen_outline::Outlined {
            program: prog.clone(),
            helpers: HashMap::new(),
        }
    } else {
        crate::codegen_outline::outline(prog)
    };
    crate::codegen_tail::eliminate(&mut outlined.program);
    let prog = &outlined.program;
    let module = ctx.create_module("gcr");
    // Optimizers must use the same aggregate layout as JIT/AOT emission and
    // the runtime's native buffers. LLVM's empty default layout can place i64
    // fields at four-byte boundaries before the backend later selects eight.
    let machine = host_target_machine()?;
    module.set_triple(&machine.get_triple());
    module.set_data_layout(&machine.get_target_data().get_data_layout());
    let builder = ctx.create_builder();
    let debug = if level.emits_dwarf() {
        build_debug_cx(ctx, &module, prog, level.is_full())
    } else {
        None
    };
    let private_heap = if level.is_full() { Default::default() } else { crate::codegen_escape::PrivateHeap::analyze(prog) };
    let bounded_loops = if level.is_full() { Default::default() } else { crate::codegen_loops::BoundedLoops::analyze(prog, &private_heap) };
    let domain = ctx.metadata_node(&[ctx.metadata_string("gcr.private.heap").into()]);
    let heap_scopes = private_heap.origin_sets().into_iter().map(|origins| {
        let name = format!("gcr.heap.origins.{origins:?}");
        let scope = ctx.metadata_node(&[ctx.metadata_string(&name).into(), domain.into()]);
        (origins, scope)
    }).collect();
    let nonrelocating = crate::codegen_effects::nonrelocating_functions(prog);
    let diverging = crate::codegen_effects::diverging_functions(prog);
    let root_liveness= if level.is_full() { Default::default() } else { crate::codegen_liveness::RootLiveness::analyze(prog) };
    let mut cg = Codegen {
        ctx,
        module,
        builder,
        prog,
        funcs: HashMap::new(),
        outlined_helpers: outlined.helpers,
        nonrelocating,
        diverging,
        private_heap,
        bounded_loops,
        heap_scopes,
        target_data: machine.get_target_data(),
        closed_world: !level.is_full(),
        physical_layouts: physical_layouts(prog),
        root_liveness,
        trampolines: HashMap::new(),
        alloc_sites: Vec::new(),
        alloc_site_ids: HashMap::new(),
        debug,
    };
    cg.declare_runtime_externs();

    // Reserve C symbols before selecting names for managed functions. C names
    // are ABI identities; managed display names may collide with them.
    for (i, f) in prog.funcs.iter().enumerate().filter(|(_, f)| f.is_extern) {
        let fv = cg.declare_fn(i as FuncId, f);
        cg.funcs.insert(i as FuncId, fv);
    }

    // Declare every function first (so calls can reference forward decls).
    for (i, f) in prog.funcs.iter().enumerate() {
        let fv = if f.is_extern { cg.funcs[&(i as FuncId)] } else { cg.declare_fn(i as FuncId, f) };
        if !f.is_extern {
            // Backend-created recursive loops would bypass Core safepoint
            // analysis. Supported self tail calls are lowered before codegen.
            fv.add_attribute(
                inkwell::attributes::AttributeLoc::Function,
                ctx.create_string_attribute("disable-tail-calls", "true"),
            );
        }
        let mut attrs: Vec<&str> = Vec::new();
        if cg.outlined_helpers.contains_key(&(i as FuncId)) { attrs.push("noinline"); }
        else if !f.is_extern {
            match f.hints.inline {
                crate::ast::InlineHint::None => {}
                crate::ast::InlineHint::Hint => attrs.push("inlinehint"),
                crate::ast::InlineHint::Always => attrs.push("alwaysinline"),
                crate::ast::InlineHint::Never => attrs.push("noinline"),
            }
            if f.hints.cold { attrs.push("cold"); }
        }
        for name in attrs {
            let kind = inkwell::attributes::Attribute::get_named_enum_kind_id(name);
            fv.add_attribute(inkwell::attributes::AttributeLoc::Function, ctx.create_enum_attribute(kind, 0));
        }
        cg.funcs.insert(i as FuncId, fv);
    }
    // Define bodies.
    for (i, f) in prog.funcs.iter().enumerate() {
        cg.define_fn(i as FuncId, f)?;
    }

    // Finalize DWARF before verify/optimize (resolves forward metadata refs).
    if let Some(d) = &cg.debug {
        d.di.finalize();
    }

    let entry = prog.entry.ok_or_else(|| CodegenError("no entry".into()))?;
    let entry_name = cg.funcs[&entry].get_name().to_str().map_err(|_| CodegenError("entry symbol is not UTF-8".into()))?.to_string();

    if std::env::var("GCR_DUMP_IR").is_ok() {
        eprintln!("{}", cg.module.print_to_string().to_string());
    }
    cg.module
        .verify()
        .map_err(|e| CodegenError(format!("LLVM module verify failed: {}", e.to_string())))?;
    Ok(Compiled { module: cg.module, entry_name, alloc_sites: cg.alloc_sites })
}

/// First node with a real span in a block (shallow: statement values + the tail
/// expr). Used to locate a function for its `DISubprogram`.
fn first_block_span(b: &CoreBlock) -> Option<SpanId> {
    for s in &b.stmts {
        let e = match s {
            CoreStmt::Let(_, e) => e,
            CoreStmt::Expr(e) => e,
        };
        if e.span != NO_SPAN {
            return Some(e.span);
        }
    }
    b.tail.as_ref().filter(|e| e.span != NO_SPAN).map(|e| e.span)
}

/// Split a source path into `(filename, directory)` for DWARF `DIFile`.
fn split_path(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(i) => (path[i + 1..].to_string(), path[..i].to_string()),
        None => (path.to_string(), ".".to_string()),
    }
}

/// A scalar reflection kind → its DWARF `(name, DW_ATE encoding, bit width)`.
/// Mirrors [`Codegen::di_type_for_repr`]'s scalar arm but keyed on the runtime
/// reflection `ScalarKind` (what field metadata carries). `None` for the FFI raw
/// pointer (no pointee type yet).
fn scalar_kind_di(s: crate::gc::ScalarKind) -> Option<(&'static str, u32, u64)> {
    use crate::gc::ScalarKind::*;
    Some(match s {
        I8 => ("i8", 5, 8),
        I16 => ("i16", 5, 16),
        I32 => ("i32", 5, 32),
        I64 => ("i64", 5, 64),
        U8 => ("u8", 7, 8),
        U16 => ("u16", 7, 16),
        U32 => ("u32", 7, 32),
        U64 => ("u64", 7, 64),
        I128 => ("i128", 5, 128),
        U128 => ("u128", 7, 128),
        F32 => ("f32", 4, 32),
        F64 => ("f64", 4, 64),
        Bool => ("bool", 2, 8),
        Char => ("char", 7, 32),
        Ptr => return None,
    })
}

/// Builds native DWARF composite types from the reflection metadata (debugger
/// P3), so lldb renders heap structs/enums by field name with no Python printer.
/// One memoized struct DIE per heap `type_id`; member offsets are absolute
/// (header included), so dereferencing the `Ref` pointer reads the right bytes.
/// An enum becomes a struct with one `tag` member typed as a DWARF enumeration
/// (tag value → variant name) — the active variant shows, the payload doesn't
/// (needs variant parts, absent from the C API). Recursion (a `List` whose tail
/// points back at itself) is broken by an in-progress guard: a still-being-built
/// layout reached through a pointer degrades to a raw address. Opaque and varlen
/// (Array/String) layouts are `None` (rendered as an address). Value-typed
/// fields are skipped (no per-value offset metadata here yet) — other members
/// keep their explicit offsets, so the struct still renders minus that field.
struct DiTypeBuilder<'a, 'ctx> {
    di: &'a DebugInfoBuilder<'ctx>,
    scope: DIScope<'ctx>,
    file: DIFile<'ctx>,
    prog: &'a CoreProgram,
    layout: Vec<Option<DIType<'ctx>>>,
    layout_inprog: Vec<bool>,
}

impl<'a, 'ctx> DiTypeBuilder<'a, 'ctx> {
    fn new(
        di: &'a DebugInfoBuilder<'ctx>,
        scope: DIScope<'ctx>,
        file: DIFile<'ctx>,
        prog: &'a CoreProgram,
    ) -> Self {
        let nl = prog.layouts.len();
        DiTypeBuilder {
            di,
            scope,
            file,
            prog,
            layout: vec![None; nl],
            layout_inprog: vec![false; nl],
        }
    }

    /// Run the builder over every layout, returning the `type_id`→struct cache.
    fn build_all(mut self) -> Vec<Option<DIType<'ctx>>> {
        for tid in 0..self.prog.layouts.len() {
            self.layout_type(tid as u16);
        }
        self.layout
    }

    fn scalar(&self, s: crate::gc::ScalarKind) -> Option<DIType<'ctx>> {
        let (name, enc, bits) = scalar_kind_di(s)?;
        Some(self.di.create_basic_type(name, bits, enc, DIFlags::ZERO).ok()?.as_type())
    }

    /// A `DW_ATE_address` basic type — an opaque pointer shown as a hex address
    /// (used when a field/local points at a type we don't model yet).
    fn opaque_addr(&self) -> Option<DIType<'ctx>> {
        Some(self.di.create_basic_type("ptr", 64, 1, DIFlags::ZERO).ok()?.as_type())
    }

    /// The DWARF type for a reference to heap layout `tid`: a pointer to its
    /// struct DIE, or an opaque address if that layout isn't modeled / is still
    /// being built (cycle).
    fn ref_ptr(&mut self, tid: u16) -> Option<DIType<'ctx>> {
        match self.layout_type(tid) {
            Some(p) => Some(
                self.di
                    .create_pointer_type("", p, 64, 64, AddressSpace::default())
                    .as_type(),
            ),
            None => self.opaque_addr(),
        }
    }

    fn field_ty(&mut self, ft: &crate::gc::FieldTy) -> Option<DIType<'ctx>> {
        use crate::gc::FieldTy;
        match ft {
            FieldTy::Scalar(s) => self.scalar(*s),
            FieldTy::Ref(tid) => self.ref_ptr(*tid),
            // Value-typed fields are not modeled yet (no per-value offsets here);
            // skip the member — siblings keep their explicit offsets.
            FieldTy::Value(_) | FieldTy::ProjectedValue { .. } => None,
        }
    }

    /// Memoized DWARF struct DIE for heap layout `tid`. `None` for enum / opaque
    /// / varlen layouts and during in-progress recursion.
    fn layout_type(&mut self, tid: u16) -> Option<DIType<'ctx>> {
        let i = tid as usize;
        if i >= self.prog.layouts.len() {
            return None;
        }
        if let Some(t) = self.layout[i] {
            return Some(t);
        }
        if self.layout_inprog[i] {
            return None; // cycle: caller (a pointer) falls back to an address
        }
        let layout = &self.prog.layouts[i];
        // Varlen (Array/String) → opaque (address). Extract owned kind data so we
        // can take `&mut self` for the recursive member builds below (`layout`
        // borrows `prog`, not `self`, but cloning keeps it simple).
        if !matches!(layout.varlen, VarLen::None) {
            return None;
        }
        let total_bytes = 16u64 + layout.ptr_fields as u64 * 8 + layout.raw_bytes as u64;
        let name = layout.meta.name.clone();
        enum Kind {
            Struct(Vec<crate::gc::FieldMeta>),
            Enum(u16, Vec<crate::gc::VariantMeta>),
        }
        let kind = match &layout.meta.kind {
            crate::gc::TypeKind::Struct { fields } => Kind::Struct(fields.clone()),
            crate::gc::TypeKind::Enum { tag_offset, variants } => Kind::Enum(*tag_offset, variants.clone())
            ,
            crate::gc::TypeKind::Opaque => return None,
        };
        self.layout_inprog[i] = true;
        let members: Vec<DIType<'ctx>> = match kind {
            Kind::Struct(fields) => fields
                .iter()
                .filter_map(|f| {
                    let fty = self.field_ty(&f.ty)?;
                    let size_bits = di_field_size_bits(self.prog, &f.ty);
                    Some(
                        self.di
                            .create_member_type(
                                self.scope,
                                &f.name,
                                self.file,
                                0,
                                size_bits,
                                0,
                                f.offset as u64 * 8,
                                DIFlags::ZERO,
                                fty,
                            )
                            .as_type(),
                    )
                })
                .collect(),
            // A heap enum: model the u32 discriminant at `tag_offset` as a DWARF
            // enumeration type (tag value → variant name), so `frame variable e`
            // shows `{ tag = Node }` — the active variant. Per-variant PAYLOAD
            // isn't rendered (needs DWARF variant parts, absent from the C API,
            // or a reflection synthetic provider — see docs/reflection.md).
            Kind::Enum(tag_offset, variants) => {
                let enumerators: Vec<_> = variants
                    .iter()
                    .map(|v| self.di.create_enumerator(&v.name, v.tag as i64, true))
                    .collect();
                match self.di.create_basic_type("u32", 32, 7, DIFlags::ZERO) {
                    Ok(u32t) => {
                        let enum_ty = self
                            .di
                            .create_enumeration_type(
                                self.scope,
                                &format!("{}::tag", name),
                                self.file,
                                0,
                                32,
                                0,
                                &enumerators,
                                u32t.as_type(),
                            )
                            .as_type();
                        vec![self
                            .di
                            .create_member_type(
                                self.scope,
                                "tag",
                                self.file,
                                0,
                                32,
                                0,
                                tag_offset as u64 * 8,
                                DIFlags::ZERO,
                                enum_ty,
                            )
                            .as_type()]
                    }
                    Err(_) => vec![],
                }
            }
        };
        let st = self
            .di
            .create_struct_type(
                self.scope,
                &name,
                self.file,
                0,
                total_bytes * 8,
                0,
                DIFlags::ZERO,
                None,
                &members,
                0,
                None,
                "",
            )
            .as_type();
        self.layout_inprog[i] = false;
        self.layout[i] = Some(st);
        Some(st)
    }
}

/// Size in bits of a field's value (the storage the member occupies).
fn di_field_size_bits(prog: &CoreProgram, ft: &crate::gc::FieldTy) -> u64 {
    use crate::gc::FieldTy;
    match ft {
        FieldTy::Scalar(s) => scalar_kind_di(*s).map(|(_, _, b)| b).unwrap_or(64),
        FieldTy::Ref(_) => 64,
        FieldTy::Value(vid) | FieldTy::ProjectedValue { value_id: vid, .. } => prog
            .values
            .get(*vid as usize)
            .map(|v| v.size as u64 * 8)
            .unwrap_or(0),
    }
}

/// Build the DWARF emitter + a `DIFile` per source. Returns `None` if there is no
/// source map (e.g. a program built directly in a test) — nothing to attribute.
fn build_debug_cx<'ctx>(
    ctx: &'ctx Context,
    module: &Module<'ctx>,
    prog: &CoreProgram,
    full: bool,
) -> Option<DebugCx<'ctx>> {
    if prog.sources.is_empty() {
        return None;
    }
    let emission = if full {
        DWARFEmissionKind::Full
    } else {
        DWARFEmissionKind::LineTablesOnly
    };
    let (filename, directory) = split_path(&prog.sources[0].path);
    let (di, cu) = module.create_debug_info_builder(
        /* allow_unresolved */ true,
        DWARFSourceLanguage::C,
        &filename,
        &directory,
        /* producer */ "gcr",
        /* is_optimized */ false,
        /* flags */ "",
        /* runtime_ver */ 0,
        /* split_name */ "",
        emission,
        /* dwo_id */ 0,
        /* split_debug_inlining */ false,
        /* debug_info_for_profiling */ false,
        /* sysroot */ "",
        /* sdk */ "",
    );
    // The backend only emits DWARF if the module declares the debug-info version.
    let i32t = ctx.i32_type();
    module.add_basic_value_flag(
        "Debug Info Version",
        FlagBehavior::Warning,
        i32t.const_int(inkwell::debug_info::debug_metadata_version() as u64, false),
    );
    if cfg!(target_os = "macos") {
        module.add_basic_value_flag(
            "Dwarf Version",
            FlagBehavior::Warning,
            i32t.const_int(4, false),
        );
    }
    // One DIFile per source (index = SourceId), so a span resolves to the right
    // file in the line table.
    let files: Vec<DIFile<'ctx>> = prog
        .sources
        .iter()
        .map(|s| {
            let (f, d) = split_path(&s.path);
            di.create_file(&f, &d)
        })
        .collect();
    // Full debug: pre-build a native DWARF struct DIE per heap layout so `Ref`
    // locals render by field name in lldb. Built before `di` is moved into the
    // DebugCx (the builder only borrows it; `DIType` is `'ctx`-scoped, not tied
    // to the borrow).
    let layout_types = if full {
        DiTypeBuilder::new(&di, cu.as_debug_info_scope(), files[0], prog).build_all()
    } else {
        Vec::new()
    };
    Some(DebugCx { di, cu, files, full, layout_types })
}

/// Emit the LLVM IR text for a whole program — the `gcr emit llvm` tap that
/// completes the source→silicon chain. With `optimize`, runs the same O3
/// pipeline the JIT/AOT paths use, so you see the IR that actually executes;
/// without it, the naive pre-optimization IR (every local a stack slot), which
/// maps more directly onto the Core IR.
pub fn emit_llvm_ir(prog: &CoreProgram, optimize: bool) -> Result<String, CodegenError> {
    let ctx = Context::create();
    let compiled = codegen(&ctx, prog)?;
    if optimize {
        optimize_module(&compiled.module)?;
    }
    Ok(compiled.module.print_to_string().to_string())
}

struct Codegen<'ctx, 'p> {
    ctx: &'ctx Context,
    module: Module<'ctx>,
    builder: Builder<'ctx>,
    prog: &'p CoreProgram,
    funcs: HashMap<FuncId, FunctionValue<'ctx>>,
    outlined_helpers: HashMap<FuncId, String>,
    nonrelocating: Vec<bool>,
    closed_world: bool,
    physical_layouts: PhysicalLayouts,
    root_liveness: crate::codegen_liveness::RootLiveness,
    private_heap: crate::codegen_escape::PrivateHeap,
    bounded_loops: crate::codegen_loops::BoundedLoops,
    heap_scopes: HashMap<BTreeSet<usize>, inkwell::values::MetadataValue<'ctx>>,
    /// Functions that never return (see `codegen_effects::diverging_functions`).
    diverging: Vec<bool>,
    /// The module's data layout, for byte offsets within GC frames.
    target_data: inkwell::targets::TargetData,
    /// Cache of synthesized FFI callback trampolines, keyed by the gc-rust
    /// FuncId they wrap (one trampoline per referenced function).
    trampolines: HashMap<FuncId, FunctionValue<'ctx>>,
    /// Allocation-site table (Target-1b), indexed by the `site_id` constant
    /// passed to `ai_gc_alloc_*`. One entry per unique `(function, type_id)`
    /// pair — the honest v1 granularity (Core IR has no source span, so two
    /// allocations of the same type in the same function share a site).
    alloc_sites: Vec<crate::gc::AllocSite>,
    /// Dedup index: `(function symbol, type_id) -> site_id`, so a repeated
    /// `(function, type)` pair reuses its id instead of minting a distinct id
    /// we couldn't label distinctly.
    alloc_site_ids: HashMap<(String, u16, String), u32>,
    /// DWARF line-table state — `Some` only when codegen is asked to emit debug
    /// info (AOT builds). `None` for JIT/`emit llvm` (no DWARF, zero overhead).
    debug: Option<DebugCx<'ctx>>,
}

impl<'ctx, 'p> Codegen<'ctx, 'p> {
    fn llvm_ty(&self, repr: &Repr) -> Option<BasicTypeEnum<'ctx>> {
        match repr {
            Repr::Unit => None,
            Repr::Scalar(s) => Some(self.scalar_ty(*s)),
            // Heap refs are opaque pointers.
            Repr::Ref(_) => Some(self.ctx.ptr_type(AddressSpace::default()).as_basic_type_enum()),
            // Value aggregates are inline LLVM structs (passed by value).
            Repr::Value(vid) => Some(self.value_struct_ty(*vid).as_basic_type_enum()),
        }
    }

    /// The LLVM aggregate type for an inline value layout. Value structs store
    /// their fields in declaration order; value enums lower as `{ i32 tag,
    /// [N x i8] payload }` (handled by their byte size).
    fn value_struct_ty(&self, vid: ValueId) -> inkwell::types::StructType<'ctx> {
        let v = &self.prog.values[vid as usize];
        if let Some(variants) = &v.variants {
            let max_ptrs = crate::core::value_enum_max_ptrs(variants, &self.prog.values) as u32;
            if max_ptrs == 0 {
                // Compact value enum: `{ i32 tag, [payload bytes] }`.
                let payload_bytes = v.size.saturating_sub(8) / 8;
                return self.ctx.struct_type(
                    &[
                        self.ctx.i32_type().as_basic_type_enum(),
                        self.ctx.i64_type().array_type(payload_bytes).as_basic_type_enum(),
                    ],
                    false,
                );
            }
            // Pointers-first value enum: `{ [max_ptrs x ptr], i32 tag, [raw] }`.
            // Ref payloads share the leading slots across variants (fixed offsets,
            // GC-traceable); scalars/POD-values go in the raw byte region.
            let ptr = self.ctx.ptr_type(AddressSpace::default());
            let raw_bytes = v.size.saturating_sub(max_ptrs * 8 + 8) / 8;
            return self.ctx.struct_type(
                &[
                    ptr.array_type(max_ptrs).as_basic_type_enum(),
                    self.ctx.i32_type().as_basic_type_enum(),
                    self.ctx.i64_type().array_type(raw_bytes).as_basic_type_enum(),
                ],
                false,
            );
        }
        let fields: Vec<BasicTypeEnum> = v.fields.iter()
            .filter_map(|r| self.llvm_ty(r))
            .collect();
        self.ctx.struct_type(&fields, false)
    }

    /// If value layout `vid` is a homogeneous floating-point aggregate (AAPCS64
    /// HFA: 1–4 members, all the same float type, recursing into nested value
    /// structs), returns `(is_f64, count)`. Such an aggregate is passed/returned
    /// in consecutive SIMD registers as `[count x float|double]`. `None` for
    /// anything else (mixed/int fields, value enums, > 4 elements).
    fn value_hfa(&self, vid: ValueId) -> Option<(bool, u32)> {
        fn walk(
            values: &[crate::core::ValueLayout],
            vid: u32,
            kind: &mut Option<bool>,
            count: &mut u32,
        ) -> bool {
            let vl = &values[vid as usize];
            if vl.variants.is_some() {
                return false; // a value enum is never an HFA
            }
            for f in &vl.fields {
                match f {
                    Repr::Scalar(ScalarRepr::F32) => {
                        if *kind == Some(true) { return false; }
                        *kind = Some(false);
                        *count += 1;
                    }
                    Repr::Scalar(ScalarRepr::F64) => {
                        if *kind == Some(false) { return false; }
                        *kind = Some(true);
                        *count += 1;
                    }
                    Repr::Value(v2) => {
                        if !walk(values, *v2, kind, count) { return false; }
                    }
                    _ => return false,
                }
                if *count > 4 { return false; }
            }
            true
        }
        let mut kind = None;
        let mut count = 0;
        if walk(&self.prog.values, vid, &mut kind, &mut count) && count >= 1 && count <= 4 {
            Some((kind.unwrap(), count))
        } else {
            None
        }
    }

    fn uses_sysv_aggregate_abi(&self) -> bool {
        let triple = self.module.get_triple();
        let triple = triple.as_str().to_string_lossy();
        triple.starts_with("x86_64-") && !triple.contains("windows")
    }

    /// Classify the supported C subset (naturally aligned scalar/nested struct
    /// fields) into System V AMD64 eightbytes. INTEGER dominates SSE when
    /// fields share a chunk. Language values contain no x87 or SIMD fields.
    fn sysv_chunks(&self, vid: ValueId) -> Option<Vec<BasicTypeEnum<'ctx>>> {
        #[derive(Clone, Copy, Default)]
        struct Chunk {
            integer: bool,
            doubles: bool,
            floats: u8,
        }
        fn walk(values: &[ValueLayout], vid: ValueId, base: u32, chunks: &mut [Chunk; 2]) {
            let mut offset = base;
            for field in &values[vid as usize].fields {
                let (size, align) = value_field_size_align(values, field);
                offset = (offset + align - 1) & !(align - 1);
                match field {
                    Repr::Value(nested) => walk(values, *nested, offset, chunks),
                    Repr::Scalar(ScalarRepr::F32) => chunks[(offset / 8) as usize].floats += 1,
                    Repr::Scalar(ScalarRepr::F64) => chunks[(offset / 8) as usize].doubles = true,
                    Repr::Unit => {}
                    _ => {
                        for byte in offset..offset + size {
                            chunks[(byte / 8) as usize].integer = true;
                        }
                    }
                }
                offset += size;
            }
        }
        let size = self.prog.values[vid as usize].size;
        if size > 16 {
            return None;
        }
        let mut chunks = [Chunk::default(); 2];
        walk(&self.prog.values, vid, 0, &mut chunks);
        Some(
            chunks[..size.div_ceil(8) as usize]
                .iter()
                .enumerate()
                .map(|(index, chunk)| {
                    if chunk.integer || (!chunk.doubles && chunk.floats == 0) {
                        self.ctx
                            .custom_width_int_type((size - index as u32 * 8).min(8) * 8)
                            .into()
                    } else if chunk.doubles {
                        self.ctx.f64_type().into()
                    } else if chunk.floats == 2 {
                        self.ctx.f32_type().vec_type(2).into()
                    } else {
                        self.ctx.f32_type().into()
                    }
                })
                .collect(),
        )
    }

    /// SysV aggregates must fit entirely in the remaining integer/SSE
    /// registers, or be copied to the stack. A byval argument consumes no GPR.
    fn foreign_args(&self, function: &CoreFn) -> Vec<ForeignArg<'ctx>> {
        let sysv = self.uses_sysv_aggregate_abi();
        let (mut integers, mut vectors) = (0, 0);
        function
            .params
            .iter()
            .enumerate()
            .map(|(index, repr)| match repr {
                Repr::Value(vid) => {
                    if function.extern_by_ref.get(index).copied().unwrap_or(false) {
                        integers = (integers + 1).min(6);
                        return ForeignArg::Indirect { byval: false };
                    }
                    if sysv {
                        if let Some(chunks) = self.sysv_chunks(*vid) {
                            let sse = chunks
                                .iter()
                                .filter(|t| t.is_float_type() || t.is_vector_type())
                                .count();
                            let gpr = chunks.len() - sse;
                            if integers + gpr <= 6 && vectors + sse <= 8 {
                                integers += gpr;
                                vectors += sse;
                                return ForeignArg::Direct(self.abi_coerce(*vid, false).unwrap());
                            }
                        }
                        ForeignArg::Indirect { byval: true }
                    } else {
                        self.abi_coerce(*vid, false)
                            .map_or(ForeignArg::Indirect { byval: false }, ForeignArg::Direct)
                    }
                }
                Repr::Scalar(ScalarRepr::F32 | ScalarRepr::F64) => {
                    vectors = (vectors + 1).min(8);
                    ForeignArg::Scalar
                }
                _ => {
                    integers = (integers + 1).min(6);
                    ForeignArg::Scalar
                }
            })
            .collect()
    }

    fn foreign_byval_attribute(&self, vid: ValueId) -> inkwell::attributes::Attribute {
        self.ctx.create_type_attribute(
            inkwell::attributes::Attribute::get_named_enum_kind_id("byval"),
            self.value_struct_ty(vid).as_any_type_enum(),
        )
    }

    /// Target ABI classification of a blittable value struct crossing FFI **by
    /// value** (non-`mut` extern param, or a return). Returns the LLVM type the
    /// struct is COERCED to for the call/return, or `None` when it must go
    /// indirect (> 16 bytes → a pointer to a caller-allocated copy). `is_return`
    /// picks the tighter integer coercion the C ABI uses for small returns
    /// (a 4-byte struct returns as `i32`, but an argument occupies a full `i64`
    /// register slot) on AAPCS64. SysV uses INTEGER/SSE eightbyte coercions.
    fn abi_coerce(&self, vid: ValueId, is_return: bool) -> Option<BasicTypeEnum<'ctx>> {
        if self.uses_sysv_aggregate_abi() {
            let chunks = self.sysv_chunks(vid)?;
            return Some(if chunks.len() == 1 { chunks[0] }
                else { self.ctx.struct_type(&chunks, false).into() });
        }
        if let Some((is_f64, n)) = self.value_hfa(vid) {
            let elem = if is_f64 { self.ctx.f64_type() } else { self.ctx.f32_type() };
            return Some(elem.array_type(n).as_basic_type_enum());
        }
        let size = self.prog.values[vid as usize].size;
        let i64t = self.ctx.i64_type();
        if size <= 8 {
            if is_return && size <= 4 {
                Some(self.ctx.i32_type().as_basic_type_enum())
            } else {
                Some(i64t.as_basic_type_enum())
            }
        } else if size <= 16 {
            Some(i64t.array_type(2).as_basic_type_enum())
        } else {
            None // indirect (by pointer)
        }
    }

    fn scalar_ty(&self, s: ScalarRepr) -> BasicTypeEnum<'ctx> {
        match s {
            ScalarRepr::I8 | ScalarRepr::U8 => self.ctx.i8_type().as_basic_type_enum(),
            ScalarRepr::I16 | ScalarRepr::U16 => self.ctx.i16_type().as_basic_type_enum(),
            ScalarRepr::I32 | ScalarRepr::U32 | ScalarRepr::Char => {
                self.ctx.i32_type().as_basic_type_enum()
            }
            ScalarRepr::I64 | ScalarRepr::U64 => self.ctx.i64_type().as_basic_type_enum(),
            ScalarRepr::I128 | ScalarRepr::U128 => self.ctx.i128_type().as_basic_type_enum(),
            ScalarRepr::F32 => self.ctx.f32_type().as_basic_type_enum(),
            ScalarRepr::F64 => self.ctx.f64_type().as_basic_type_enum(),
            ScalarRepr::Bool => self.ctx.bool_type().as_basic_type_enum(),
            ScalarRepr::Ptr => self.ctx.ptr_type(AddressSpace::default()).as_basic_type_enum(),
        }
    }

    /// Load one mutable managed slot. Shared slots use LLVM `unordered`
    /// atomics: Java plain-field semantics (no tearing, no ordering; DRF-SC
    /// through the synchronizing operations). Bool has
    /// byte storage even though its SSA representation is i1.
    fn managed_load_idle(&self, addr: PointerValue<'ctx>, repr: &Repr, name: &str, owner: &CoreExpr) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let storage = if *repr == Repr::Scalar(ScalarRepr::Bool) {
            self.ctx.i8_type().as_basic_type_enum()
        } else { self.llvm_ty(repr).ok_or_else(|| CodegenError("invalid managed scalar load".into()))? };
        let alignment = match repr {
            Repr::Scalar(s) => s.bits().max(8) / 8,
            Repr::Ref(_) => 8,
            _ => return Err(CodegenError("aggregate must use a managed snapshot".into())),
        };
        let value = self.builder.build_load(storage, addr, name).unwrap();
        let instruction = value.as_instruction_value().unwrap();
        instruction.set_alignment(alignment).map_err(|e| CodegenError(e.to_string()))?;
        let private = self.private_heap.contains(owner);
        self.set_heap_alias_metadata(instruction, owner)?;
        if !private { instruction.set_atomic_ordering(AtomicOrdering::Unordered).map_err(|e| CodegenError(e.to_string()))?; }
        Ok(if *repr == Repr::Scalar(ScalarRepr::Bool) {
            self.builder.build_int_truncate(value.into_int_value(), self.ctx.bool_type(), "managed.bool").unwrap().into()
        } else { value })
    }

    fn managed_store_idle(&self, addr: PointerValue<'ctx>, value: BasicValueEnum<'ctx>, repr: &Repr, owner: &CoreExpr) -> Result<(), CodegenError> {
        let (stored, alignment) = match repr {
            Repr::Scalar(ScalarRepr::Bool) => (
                self.builder.build_int_z_extend(value.into_int_value(), self.ctx.i8_type(), "managed.bool.byte").unwrap().into(), 1,
            ),
            Repr::Scalar(s) => (value, s.bits().max(8) / 8),
            Repr::Ref(_) => (value, 8),
            _ => return Err(CodegenError("aggregate must use a managed snapshot".into())),
        };
        let instruction = self.builder.build_store(addr, stored).unwrap();
        instruction.set_alignment(alignment).map_err(|e| CodegenError(e.to_string()))?;
        let private = self.private_heap.contains(owner);
        self.set_heap_alias_metadata(instruction, owner)?;
        if !private { instruction.set_atomic_ordering(AtomicOrdering::Unordered).map_err(|e| CodegenError(e.to_string()))?; }
        Ok(())
    }

    /// Select the non-safepointing scoped path while concurrent copying is active.
    /// Read the heap's copying phase. A plain load: copying can only start
    /// in a pause, so an idle phase stays idle until the next safepoint. A
    /// stale "active" only takes the slow path, correct in either phase.
    ///
    /// The result packs two bits: 1 while copying, 2 once more than one
    /// mutator exists. Inline aggregates are accessed directly only when
    /// both are clear (no collector copy and no thread to race with).
    fn load_copy_phase(&self, fcx: &FnCtx<'ctx>) -> BasicValueEnum<'ctx> {
        let i8t = self.ctx.i8_type();
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let address = self.obj_addr(fcx.thread, crate::runtime::thread_offsets::COPYING_PHASE as u64);
        let flag = self.builder.build_load(ptr, address, "copy.phase.ptr").unwrap().into_pointer_value();
        let phase = self.builder.build_load(i8t, flag, "copy.phase").unwrap();
        phase.as_instruction_value().unwrap().set_alignment(1).unwrap();
        let address = self.obj_addr(fcx.thread, crate::runtime::thread_offsets::MULTI_MUTATOR as u64);
        let flag = self.builder.build_load(ptr, address, "multi.mutator.ptr").unwrap().into_pointer_value();
        let multi = self.builder.build_load(i8t, flag, "multi.mutator").unwrap();
        multi.as_instruction_value().unwrap().set_alignment(1).unwrap();
        let copying = self.builder.build_int_compare(IntPredicate::NE, phase.into_int_value(), i8t.const_zero(), "copying").unwrap();
        let shared = self.builder.build_int_compare(IntPredicate::NE, multi.into_int_value(), i8t.const_zero(), "shared").unwrap();
        let copying = self.builder.build_int_z_extend(copying, i8t, "copying.bit").unwrap();
        let shared = self.builder.build_int_z_extend(shared, i8t, "shared.bit").unwrap();
        let shared = self.builder.build_left_shift(shared, i8t.const_int(1, false), "shared.bit2").unwrap();
        self.builder.build_or(copying, shared, "access.mode").unwrap().into()
    }

    /// The cached access mode (see `load_copy_phase`).
    fn access_mode(&self, fcx: &FnCtx<'ctx>) -> IntValue<'ctx> {
        match fcx.phase {
            Some(slot) => self.builder.build_load(self.ctx.i8_type(), slot, "access.mode.cached").unwrap().into_int_value(),
            None => self.load_copy_phase(fcx).into_int_value(),
        }
    }

    /// May inline aggregates be read and written directly (no copy in
    /// progress and no other mutator)?
    fn aggregate_direct(&self, fcx: &FnCtx<'ctx>) -> IntValue<'ctx> {
        let mode = self.access_mode(fcx);
        self.builder.build_int_compare(IntPredicate::EQ, mode, self.ctx.i8_type().const_zero(), "aggregate.direct").unwrap()
    }

    /// After a call that may safepoint, the copying phase may have changed.
    fn refresh_copy_phase(&self, fcx: &FnCtx<'ctx>) {
        if let Some(slot) = fcx.phase {
            if self.builder.get_insert_block().is_some_and(|b| b.get_terminator().is_none()) {
                let current = self.load_copy_phase(fcx);
                self.builder.build_store(slot, current).unwrap();
            }
        }
    }

    fn managed_access_blocks(&self, fcx: &FnCtx<'ctx>) -> (inkwell::basic_block::BasicBlock<'ctx>, inkwell::basic_block::BasicBlock<'ctx>, inkwell::basic_block::BasicBlock<'ctx>) {
        let mode = self.access_mode(fcx);
        let phase = self.builder.build_and(mode, self.ctx.i8_type().const_int(1, false), "copy.phase.bit").unwrap();
        let active = self.builder.build_int_compare(IntPredicate::NE, phase, self.ctx.i8_type().const_zero(), "copy.active").unwrap();
        let idle = self.ctx.append_basic_block(fcx.func, "access.idle");
        let copying = self.ctx.append_basic_block(fcx.func, "access.copying");
        let merge = self.ctx.append_basic_block(fcx.func, "access.merge");
        self.builder.build_conditional_branch(active, copying, idle).unwrap();
        (idle, copying, merge)
    }

    fn managed_load(&self, fcx: &FnCtx<'ctx>, object: PointerValue<'ctx>, offset: IntValue<'ctx>, repr: &Repr, name: &str, owner: &CoreExpr) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        // A 128-bit scalar is two 64-bit slots, low half first.
        if let Repr::Scalar(scalar) = repr { if scalar.bits() == 128 {
            let half = Repr::Scalar(ScalarRepr::U64);
            let high_offset = self.builder.build_int_add(offset, offset.get_type().const_int(8, false), "slot.high").unwrap();
            let low = self.managed_load(fcx, object, offset, &half, "slot.low", owner)?.into_int_value();
            let high = self.managed_load(fcx, object, high_offset, &half, "slot.high", owner)?.into_int_value();
            let wide = self.ctx.i128_type();
            let low = self.builder.build_int_z_extend(low, wide, "slot.low.wide").unwrap();
            let high = self.builder.build_int_z_extend(high, wide, "slot.high.wide").unwrap();
            let high = self.builder.build_left_shift(high, wide.const_int(64, false), "slot.high.shifted").unwrap();
            return Ok(self.builder.build_or(high, low, name).unwrap().into());
        } }
        let width = match repr { Repr::Scalar(s) => s.bits().max(8), Repr::Ref(_) => 64,
            _ => return Err(CodegenError("aggregate must use a managed snapshot".into())) };
        let (idle, copying, merge) = self.managed_access_blocks(fcx);
        self.builder.position_at_end(idle);
        let addr = unsafe { self.builder.build_gep(self.ctx.i8_type(), object, &[offset], "fld.slot.idle").unwrap() };
        let fast = self.managed_load_idle(addr, repr, name, owner)?;
        self.builder.build_unconditional_branch(merge).unwrap();
        self.builder.position_at_end(copying);
        let function = self.module.get_function(&format!("ai_slot_bits{width}_load")).unwrap();
        let bits = call_result(self.build_managed_call(fcx, function, &[fcx.thread.into(), object.into(), offset.into()], "slot.scoped").unwrap()).into_int_value();
        let slow: BasicValueEnum<'ctx> = match repr {
            Repr::Scalar(ScalarRepr::Bool) => self.builder.build_int_truncate(bits, self.ctx.bool_type(), "slot.bool").unwrap().into(),
            Repr::Scalar(ScalarRepr::F32 | ScalarRepr::F64) => self.builder.build_bit_cast(bits, self.llvm_ty(repr).unwrap(), "slot.float").unwrap(),
            Repr::Scalar(ScalarRepr::Ptr) | Repr::Ref(_) => self.builder.build_int_to_ptr(bits, self.ctx.ptr_type(AddressSpace::default()), "slot.pointer").unwrap().into(),
            _ => bits.into(),
        };
        self.builder.build_unconditional_branch(merge).unwrap();
        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(fast.get_type(), name).unwrap();
        phi.add_incoming(&[(&fast, idle), (&slow, copying)]);
        Ok(phi.as_basic_value())
    }

    fn managed_store(&self, fcx: &FnCtx<'ctx>, object: PointerValue<'ctx>, offset: IntValue<'ctx>, value: BasicValueEnum<'ctx>, repr: &Repr, traced: bool, owner: &CoreExpr) -> Result<(), CodegenError> {
        if let Repr::Scalar(scalar) = repr { if scalar.bits() == 128 {
            let half = Repr::Scalar(ScalarRepr::U64);
            let value = value.into_int_value();
            let i64t = self.ctx.i64_type();
            let low = self.builder.build_int_truncate(value, i64t, "slot.low").unwrap();
            let high = self.builder.build_right_shift(value, value.get_type().const_int(64, false), false, "slot.high.shifted").unwrap();
            let high = self.builder.build_int_truncate(high, i64t, "slot.high").unwrap();
            let high_offset = self.builder.build_int_add(offset, offset.get_type().const_int(8, false), "slot.high.offset").unwrap();
            self.managed_store(fcx, object, offset, low.into(), &half, false, owner)?;
            return self.managed_store(fcx, object, high_offset, high.into(), &half, false, owner);
        } }
        let width = match repr { Repr::Scalar(s) => s.bits().max(8), Repr::Ref(_) => 64,
            _ => return Err(CodegenError("aggregate must use a managed snapshot".into())) };
        let (idle, copying, merge) = self.managed_access_blocks(fcx);
        self.builder.position_at_end(idle);
        let addr = unsafe { self.builder.build_gep(self.ctx.i8_type(), object, &[offset], "fld.slot.idle").unwrap() };
        self.managed_store_idle(addr, value, repr, owner)?;
        if traced { self.emit_write_barrier(fcx, object, addr, value); }
        self.builder.build_unconditional_branch(merge).unwrap();
        self.builder.position_at_end(copying);
        if traced {
            let function = self.module.get_function("ai_slot_ref_store").unwrap();
            self.build_managed_call(fcx, function, &[fcx.thread.into(), object.into(), offset.into(), value.into()], "").unwrap();
        } else {
            let bits: BasicValueEnum<'ctx> = match repr {
                Repr::Scalar(ScalarRepr::Bool) => self.builder.build_int_z_extend(value.into_int_value(), self.ctx.i8_type(), "slot.bool.bits").unwrap().into(),
                Repr::Scalar(ScalarRepr::F32 | ScalarRepr::F64) => self.builder.build_bit_cast(value, self.ctx.custom_width_int_type(width), "slot.float.bits").unwrap(),
                Repr::Scalar(ScalarRepr::Ptr) => self.builder.build_ptr_to_int(value.into_pointer_value(), self.ctx.i64_type(), "slot.pointer.bits").unwrap().into(),
                _ => value,
            };
            let function = self.module.get_function(&format!("ai_slot_bits{width}_store")).unwrap();
            self.build_managed_call(fcx, function, &[fcx.thread.into(), object.into(), offset.into(), bits.into()], "").unwrap();
        }
        self.builder.build_unconditional_branch(merge).unwrap();
        self.builder.position_at_end(merge);
        Ok(())
    }

    /// Access-scoped provenance: only disjoint allocation-origin sets receive
    /// noalias metadata. Runtime calls, roots and incoming/escaped references
    /// carry no such claim, so collectors may still access and move objects.
    fn set_heap_alias_metadata(&self, instruction: inkwell::values::InstructionValue<'ctx>, owner: &CoreExpr) -> Result<(), CodegenError> {
        let Some(origins) = self.private_heap.origins(owner) else { return Ok(()); };
        let own = self.heap_scopes[origins];
        instruction.set_metadata(self.ctx.metadata_node(&[own.into()]), self.ctx.get_kind_id("alias.scope"))
            .map_err(|error| CodegenError(error.to_string()))?;
        let disjoint: Vec<_> = self.heap_scopes.iter().filter(|(other,_)| origins.is_disjoint(other))
            .map(|(_,scope)| (*scope).into()).collect();
        if !disjoint.is_empty() {
            instruction.set_metadata(self.ctx.metadata_node(&disjoint), self.ctx.get_kind_id("noalias"))
                .map_err(|error| CodegenError(error.to_string()))?;
        }
        Ok(())
    }

    fn managed_lock(&self, fcx: &FnCtx<'ctx>, obj: PointerValue<'ctx>) -> PointerValue<'ctx> {
        let f = self.module.get_function("ai_managed_lock").unwrap();
        call_result(self.build_managed_call(fcx, f, &[fcx.thread.into(), obj.into()], "managed.object").unwrap()).into_pointer_value()
    }

    fn managed_unlock(&self, fcx: &FnCtx<'ctx>) {
        let f = self.module.get_function("ai_managed_unlock").unwrap();
        self.builder.build_call(f, &[fcx.thread.into()], "").unwrap();
    }

    fn declare_fn(&self, id: FuncId, f: &CoreFn) -> FunctionValue<'ctx> {
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        // Foreign `extern "C"` function: a plain C declaration with exactly its
        // written signature — NO leading Thread*, no env pointer, name unmangled
        // (the C symbol). External linkage so the JIT/host or AOT linker resolves
        // it. See `docs/ffi.md`.
        if f.is_extern {
            let mut params: Vec<BasicMetadataTypeEnum> = Vec::new();
            let foreign_args = self.foreign_args(f);
            for (i, p) in f.params.iter().enumerate() {
                match p {
                    // A `#[repr(C)]` value struct crosses either BY VALUE (the
                    // common case — coerced per the C ABI into register-shaped
                    // types) or BY POINTER (a `mut` out-param, or > 16 bytes which
                    // the ABI passes indirectly). See `docs/ffi.md`.
                    Repr::Value(_) => {
                        match foreign_args[i] {
                            ForeignArg::Direct(coerced) => params.push(coerced.into()),
                            _ => params.push(ptr.into()),
                        }
                    }
                    _ => {
                        if let Some(t) = self.llvm_ty(p) {
                            params.push(t.into());
                        }
                    }
                }
            }
            // A value-struct return is coerced too (≤ 16 bytes — `lower` rejects
            // larger). Scalars/unit use their natural LLVM type.
            let fn_ty = match &f.ret {
                Repr::Value(vid) => match self.abi_coerce(*vid, true) {
                    Some(coerced) => coerced.fn_type(&params, false),
                    None => self.ctx.void_type().fn_type(&params, false),
                },
                _ => match self.llvm_ty(&f.ret) {
                    Some(rt) => rt.fn_type(&params, false),
                    None => self.ctx.void_type().fn_type(&params, false),
                },
            };
            let function = self.module.add_function(
                &f.name, fn_ty, Some(inkwell::module::Linkage::External),
            );
            for (index, abi) in foreign_args.iter().enumerate() {
                if matches!(abi, ForeignArg::Indirect { byval: true }) {
                    let Repr::Value(vid) = f.params[index] else { unreachable!() };
                    function.add_attribute(inkwell::attributes::AttributeLoc::Param(index as u32),
                        self.foreign_byval_attribute(vid));
                }
            }
            return function;
        }
        // First param is the Thread*. A lifted closure fn takes the env pointer
        // as its second param (before the value params).
        let mut params: Vec<BasicMetadataTypeEnum> = vec![ptr.into()];
        if f.is_closure {
            params.push(ptr.into());
        }
        for p in &f.params {
            if let Some(t) = self.llvm_ty(p) {
                params.push(t.into());
            }
        }
        let fn_ty = match self.llvm_ty(&f.ret) {
            Some(rt) => rt.fn_type(&params, false),
            None => self.ctx.void_type().fn_type(&params, false),
        };
        // Private-array analysis assumes all helper callers are in this module.
        // Entry parameters and address-taken functions have unknown origins.
        let linkage = if self.closed_world && self.prog.entry != Some(id) {
            Some(inkwell::module::Linkage::Internal)
        } else { None };
        let mut name = f.name.clone();
        let mut suffix = 0u64;
        while self.module.get_function(&name).is_some() {
            name = format!("{}.__managed_{}_{}", f.name, id, suffix);
            suffix += 1;
        }
        self.module.add_function(&name, fn_ty, linkage)
    }

    fn is_closure_fn(f: &CoreFn) -> bool {
        f.is_closure
    }

    /// Get (or synthesize) a C-ABI trampoline for gc-rust function `fid`, used as
    /// an FFI callback. The trampoline has the callback's C signature (the wrapped
    /// function's value params, no leading `Thread*`), recovers the ambient
    /// thread, re-enters managed state, calls the real function, restores native
    /// state, and returns. See `docs/ffi.md`.
    fn callback_trampoline(&mut self, fid: FuncId) -> FunctionValue<'ctx> {
        if let Some(tr) = self.trampolines.get(&fid) {
            return *tr;
        }
        let target = self.funcs[&fid];
        let f = &self.prog.funcs[fid as usize];

        // C signature: the value params (skip Unit), no Thread*.
        let mut c_params: Vec<BasicMetadataTypeEnum> = Vec::new();
        for p in &f.params {
            if let Some(t) = self.llvm_ty(p) {
                c_params.push(t.into());
            }
        }
        let fn_ty = match self.llvm_ty(&f.ret) {
            Some(rt) => rt.fn_type(&c_params, false),
            None => self.ctx.void_type().fn_type(&c_params, false),
        };
        let name = format!("__cb_{}", f.name);
        let tramp = self.module.add_function(&name, fn_ty, Some(inkwell::module::Linkage::Internal));
        self.trampolines.insert(fid, tramp);

        // Build the body. (Save/restore the builder position — we may be mid-call.)
        let saved = self.builder.get_insert_block();
        let entry = self.ctx.append_basic_block(tramp, "entry");
        self.builder.position_at_end(entry);

        // thread = ai_current_thread()
        let cur = self.module.get_function("ai_current_thread").unwrap();
        let thread = call_result(self.builder.build_call(cur, &[], "thr").unwrap()).into_pointer_value();
        // ai_ffi_reenter(thread) — we're back in managed code.
        let reenter = self.module.get_function("ai_ffi_reenter").unwrap();
        self.builder.build_call(reenter, &[thread.into()], "").unwrap();

        // Call the real gc-rust function: (thread, c_params...).
        let mut call_args: Vec<inkwell::values::BasicMetadataValueEnum> = vec![thread.into()];
        for (i, _) in c_params.iter().enumerate() {
            call_args.push(tramp.get_nth_param(i as u32).unwrap().into());
        }
        let cs = self.builder.build_call(target, &call_args, "cbret").unwrap();

        // ai_ffi_exit(thread) — return to foreign code, re-publish frame.
        let exit = self.module.get_function("ai_ffi_exit").unwrap();
        self.builder.build_call(exit, &[thread.into()], "").unwrap();

        match cs.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => { self.builder.build_return(Some(&v)).unwrap(); }
            inkwell::values::ValueKind::Instruction(_) => { self.builder.build_return(None).unwrap(); }
        }

        if let Some(bb) = saved { self.builder.position_at_end(bb); }
        tramp
    }

    /// The function's representative source location — the first spanned node in
    /// its body — as `(SourceId, line, col)`. Defaults to `(0, 1, 1)` (user
    /// source, line 1) when the function has no spanned node, so it still gets a
    /// DIFile + scope. Shallow scan (post-ANF the construction/statement nodes
    /// are at statement position) — enough to locate the function.
    fn fn_debug_loc(&self, f: &CoreFn) -> (crate::lexer::SourceId, u32, u32) {
        // The function's own definition span (robust: names its source even for a
        // monomorphized prelude/`mod` function). Fall back to the first spanned
        // body node, then to (user, line 1).
        if let Some((sid, line, col)) = self.prog.span_source_loc(f.span) {
            return (sid, line as u32, col as u32);
        }
        if let Some(sp) = first_block_span(&f.body) {
            if let Some((sid, line, col)) = self.prog.span_source_loc(sp) {
                return (sid, line as u32, col as u32);
            }
        }
        (0, 1, 1)
    }

    /// Create `f`'s DWARF `DISubprogram` (debugger P2) and attach it to `func`.
    /// `None` when not emitting debug info. The subprogram's file is the source
    /// the function body comes from (so its line table + any backtrace frame name
    /// the right file — prelude/`mod`/user).
    fn create_subprogram(&self, f: &CoreFn, func: FunctionValue<'ctx>) -> Option<DISubprogram<'ctx>> {
        let d = self.debug.as_ref()?;
        let (sid, line, _col) = self.fn_debug_loc(f);
        let file = d.files.get(sid as usize).copied().unwrap_or(d.files[0]);
        // Full debug: give the subprogram a real signature (return + param types)
        // so backtraces show typed prototypes. Line-tables-only keeps the empty
        // `(void)` type (no type DIEs are built there anyway).
        let sub_ty = if d.full {
            let ret_ty = self.di_signature_type(&f.ret);
            let param_tys: Vec<DIType<'ctx>> =
                f.params.iter().filter_map(|p| self.di_signature_type(p)).collect();
            d.di.create_subroutine_type(file, ret_ty, &param_tys, DIFlags::ZERO)
        } else {
            d.di.create_subroutine_type(file, None, &[], DIFlags::ZERO)
        };
        let sp = d.di.create_function(
            d.cu.as_debug_info_scope(),
            &f.name,
            Some(&f.name), // linkage name = the mangled symbol
            file,
            line,
            sub_ty,
            /* is_local_to_unit */ true,
            /* is_definition */ true,
            /* scope_line */ line,
            DIFlags::ZERO,
            /* is_optimized */ false,
        );
        func.set_subprogram(sp);
        Some(sp)
    }

    /// The DWARF `DIType` for a local (debugger P3), so a variable DIE carries a
    /// real type and `frame variable` decodes the bytes. Scalars → a basic type;
    /// `Ref` → a pointer to that layout's native struct DIE (lldb renders the
    /// fields), or a raw address for unmodeled (enum/opaque/varlen) layouts.
    /// `None` for `Value`/`Unit` and the FFI raw pointer, and when not emitting
    /// debug info.
    fn di_type_for_repr(&self, r: &Repr) -> Option<DIType<'ctx>> {
        let d = self.debug.as_ref()?;
        // `Ref` locals are handled by the caller (it needs the GC-frame slot +
        // deref expression to render the struct in place); this returns scalars.
        let s = match r {
            Repr::Scalar(s) => *s,
            _ => return None,
        };
        use crate::core::ScalarRepr::*;
        // DWARF `DW_ATE_*` encodings: boolean=2, float=4, signed=5, unsigned=7.
        let (name, encoding, bits): (&str, u32, u64) = match s {
            I8 => ("i8", 5, 8),
            I16 => ("i16", 5, 16),
            I32 => ("i32", 5, 32),
            I64 => ("i64", 5, 64),
            U8 => ("u8", 7, 8),
            U16 => ("u16", 7, 16),
            U32 => ("u32", 7, 32),
            U64 => ("u64", 7, 64),
            I128 => ("i128", 5, 128),
            U128 => ("u128", 7, 128),
            F32 => ("f32", 4, 32),
            F64 => ("f64", 4, 64),
            // `bool` lowers to `i1` but occupies a byte in its alloca; describe it
            // as an 8-bit boolean so lldb reads the byte.
            Bool => ("bool", 2, 8),
            // `char` is a 32-bit Unicode scalar.
            Char => ("char", 7, 32),
            // Raw FFI pointer — skip for now (no pointee type yet).
            Ptr => return None,
        };
        Some(d.di.create_basic_type(name, bits, encoding, DIFlags::ZERO).ok()?.as_type())
    }

    /// The DWARF type for a value in a function SIGNATURE (return/param), used to
    /// give the `DISubprogram` a real prototype. Unlike a local, a `Ref` here is a
    /// plain pointer to the struct (the passed value is the object pointer);
    /// `Value` is an opaque address; `Unit` is `None` (omitted / void).
    fn di_signature_type(&self, r: &Repr) -> Option<DIType<'ctx>> {
        let d = self.debug.as_ref()?;
        let opaque = |d: &DebugCx<'ctx>| {
            d.di.create_basic_type("ptr", 64, 1, DIFlags::ZERO).ok().map(|t| t.as_type())
        };
        match r {
            Repr::Unit => None,
            Repr::Scalar(_) => self.di_type_for_repr(r),
            Repr::Ref(lid) => match d.layout_types.get(*lid as usize).copied().flatten() {
                Some(st) => Some(
                    d.di.create_pointer_type("", st, 64, 64, AddressSpace::default()).as_type(),
                ),
                None => opaque(d),
            },
            Repr::Value(_) => opaque(d),
        }
    }

    /// Fixed-size temporary storage belongs in the entry block. An alloca at
    /// an expression's insertion point executes again on every loop iteration
    /// and retains that stack space until the function returns.
    fn entry_alloca<T: BasicType<'ctx>>(&self, ty: T, name: &str) -> PointerValue<'ctx> {
        let func = self.builder.get_insert_block().unwrap().get_parent().unwrap();
        let entry = func.get_first_basic_block().unwrap();
        let builder = self.ctx.create_builder();
        match entry.get_first_instruction() {
            Some(first) => builder.position_before(&first),
            None => builder.position_at_end(entry),
        }
        builder.build_alloca(ty, name).unwrap()
    }

    fn define_fn(&mut self, id: FuncId, f: &CoreFn) -> Result<(), CodegenError> {
        // Foreign `extern "C"` functions have no body — they're resolved at
        // link/JIT time, not defined here.
        if f.is_extern {
            return Ok(());
        }
        let func = self.funcs[&id];
        let entry = self.ctx.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);

        // DWARF (P2): create this function's DISubprogram + set its default debug
        // location BEFORE the prologue, so prologue/unspanned instructions carry
        // THIS function's scope (not the previous function's leftover location —
        // which would fail verify with "wrong subprogram"). Always reset the
        // builder's location per function.
        let subprogram = self.create_subprogram(f, func);
        if let (Some(d), Some(sp)) = (&self.debug, subprogram) {
            let (_, line, col) = self.fn_debug_loc(f);
            let loc = d.di.create_debug_location(self.ctx, line, col, sp.as_debug_info_scope(), None);
            self.builder.set_current_debug_location(loc);
        } else {
            self.builder.unset_current_debug_location();
        }

        let ptr = self.ctx.ptr_type(AddressSpace::default());

        // Proven nonrelocating callees never park, so collectors cannot inspect
        // their stack or move their references during execution. Their callers
        // retain the traced roots across the call. Keep full-debug frame storage
        // for editable reference locals and debugger consistency.
        let entry_roots_only = !self.debug.as_ref().is_some_and(|d|d.full)
            && self.bounded_loops.entry_poll(id as usize)
            && f.locals.iter().all(|repr| !matches!(repr,Repr::Value(v) if value_has_ref(&self.prog.values,*v)))
            && crate::codegen_effects::body_nonrelocating(&f.body,&self.nonrelocating,&self.bounded_loops);
        let incoming = f.closure_captures.len()+f.params.len();
        let requires_roots = !self.nonrelocating[id as usize]
            || self.debug.as_ref().is_some_and(|debug| debug.full);
        // Partition locals: Ref-typed locals become GC frame *direct* root slots;
        // all others get plain allocas. Count the refs to size the frame. A
        // local never live across a safepoint needs no root; full-debug
        // frames keep every reference local in the frame.
        let full_debug = self.debug.as_ref().is_some_and(|debug| debug.full);
        let crossing = (requires_roots && !full_debug)
            .then(|| {
                let foreign: Vec<bool> = self.prog.funcs.iter().map(|g| g.is_extern).collect();
                let traced = |r: &Repr| match r {
                    Repr::Ref(_) => true,
                    Repr::Value(v) => value_has_ref(&self.prog.values, *v),
                    _ => false,
                };
                crate::codegen_liveness::safepoint_crossing(f, traced, &self.nonrelocating, &self.diverging, &self.bounded_loops, &foreign)
            });
        let rooted = |i: usize, r: &Repr| requires_roots && matches!(r, Repr::Ref(_))
            && (!entry_roots_only || i < incoming)
            && (i < incoming || crossing.as_ref().is_none_or(|c| c.contains(&(i as LocalId))));
        // Traced locals whose live ranges are disjoint share one slot: one
        // root mirror and working slot per reference, one stack aggregate per
        // value holding references (see `codegen_slots`).
        let value_traced = |i: usize| requires_roots
            && (i < incoming || crossing.as_ref().is_none_or(|c| c.contains(&(i as LocalId))))
            && matches!(&f.locals[i], Repr::Value(v) if value_has_ref(&self.prog.values, *v));
        let shared: Vec<LocalId> = if full_debug { (0..f.locals.len() as LocalId).collect() } else {
            crate::codegen_slots::share_slots(f, incoming, |l| {
                let i = l as usize;
                if rooted(i, &f.locals[i]) { Some(0) }
                else if value_traced(i) { match &f.locals[i] { Repr::Value(v) => Some(1 + *v as u64), _ => None } }
                else { None }
            })
        };
        let owner = |i: usize| shared[i] as usize;
        let mut sharers = vec![0u32; f.locals.len()];
        for i in 0..f.locals.len() { sharers[owner(i)] += 1; }
        let shared_slot: Vec<bool> = (0..f.locals.len()).map(|i| sharers[owner(i)] > 1).collect();
        if std::env::var_os("GCR_CROSSING").is_some_and(|n| f.name.contains(n.to_str().unwrap_or_default())) {
            let mut c: Vec<_> = crossing.iter().flatten().copied().collect();
            c.sort();
            eprintln!("crossing {}: {:?}", f.name, c);
        }
        if std::env::var_os("GCR_SLOT_STATS").is_some() {
            let pairs: Vec<String> = (0..f.locals.len()).filter(|&i| owner(i) != i).map(|i| format!("l{i}->l{}", owner(i))).collect();
            eprintln!("slots {}: {}", f.name, pairs.join(" "));
        }
        // Map: local id -> root index (only for Ref locals).
        let mut root_index: Vec<Option<u32>> = vec![None; f.locals.len()];
        let mut num_roots = 0usize;
        for (i, r) in f.locals.iter().enumerate() {
            if rooted(i, r) && owner(i) == i {
                root_index[i] = Some(num_roots as u32);
                num_roots += 1;
            }
        }
        for (i, r) in f.locals.iter().enumerate() {
            if rooted(i, r) && owner(i) != i { root_index[i] = root_index[owner(i)]; }
        }

        // Value-with-ref locals: a flattened `#[value]` aggregate (in a plain
        // alloca) holding GC refs. Those refs live in untraced stack memory, so a
        // collection while the local is live would dangle them. We register each
        // interior ref's *address* as an INDIRECT frame root: the GC dereferences
        // it and relocates the pointer in place inside the alloca. `(local_idx,
        // value-relative ref offsets)`.
        let value_indirect: Vec<(usize, Vec<u16>)> = f
            .locals
            .iter()
            .enumerate()
            .filter(|_| requires_roots)
            // As for references: only values live across a safepoint.
            .filter(|(i, _)| *i < incoming || crossing.as_ref().is_none_or(|c| c.contains(&(*i as LocalId))))
            .filter(|(i, _)| owner(*i) == *i)
            .filter_map(|(i, r)| match r {
                Repr::Value(vid) if value_has_ref(&self.prog.values, *vid) => {
                    let mut offs = Vec::new();
                    value_interior_offsets(&self.prog.values, *vid, 0, &mut offs);
                    if offs.is_empty() { None } else { Some((i, offs)) }
                }
                _ => None,
            })
            .collect();
        let num_indirect: usize = value_indirect.iter().map(|(_, o)| o.len()).sum();
        if std::env::var_os("GCR_FRAME_STATS").is_some() && num_roots + num_indirect > 0 {
            eprintln!("frame {}: {} roots, {} indirect in {} values, {} locals", f.name, num_roots, num_indirect, value_indirect.len(), f.locals.len());
        }
        // With no incoming roots or indirect-root locals, all mirrors begin
        // null. Register the frame for the first reference assignment, before
        // any subsequent safepoint. Leaf-return paths can then optimize away
        // frame traffic. Other functions retain entry registration.
        //
        // Otherwise (incoming references, value locals holding references),
        // the frame is still deferred: until it is linked the references live
        // only in their working slots, and linking copies them into the traced
        // mirrors. It is linked before every call that may safepoint, so a
        // path that makes no such call never touches it. Linking reads every
        // working slot, keeping them all live at each such call, so only
        // functions with few roots work this way; the others store each
        // reference into its mirror as it is assigned.
        //
        // A *masked* frame (the default) is never zeroed: each call that may
        // reach a safepoint first stores a descriptor whose mask names the slots
        // of the traced locals in scope there, all written on the way, and the
        // collector reads only those. The frame is linked at the first such
        // call; mirrors are written as references are assigned.
        const LATE_ROOTS_LIMIT: usize = 32;
        let masked = !full_debug && !entry_roots_only;
        let few_roots = num_roots + num_indirect <= LATE_ROOTS_LIMIT;
        let defer_frame = if masked { num_roots > 0 || num_indirect > 0 } else {
            (num_roots > 0 && num_indirect == 0
            && (entry_roots_only || (f.closure_captures.is_empty()
            && f.params.iter().all(|repr| !self.repr_relocates(repr))))
            || (!entry_roots_only && (num_roots > 0 || num_indirect > 0) && few_roots))
            && !self.debug.as_ref().is_some_and(|debug| debug.full)
        };
        let late_roots = !masked && defer_frame && !entry_roots_only && few_roots;
        let frame_active = if defer_frame {
            let active = self.builder.build_alloca(self.ctx.bool_type(), "roots.active").unwrap();
            self.builder.build_store(active, self.ctx.bool_type().const_zero()).unwrap();
            Some(active)
        } else { None };

        // Emit the GC frame `{ parent, origin, [num_roots x ptr], [num_indirect x
        // ptr] }` when there are any roots (direct or indirect). A zero-length
        // trailing array adds no bytes, so frames without indirect roots are
        // byte-identical to before.
        let i32t = self.ctx.i32_type();
        let mut deferred_frame_origin = None;
        //
        // A masked frame instead holds the value-with-ref locals themselves
        // after the roots, `{ parent, origin, [num_roots x ptr], V1, V2, .. }`,
        // and its descriptor gives the frame offsets of their references: the
        // frame is linked with no per-call address stores.
        let mut indirect_offsets: Vec<u32> = Vec::new();
        let mut indirect_offsets_global = None;
        let frame = if num_roots > 0 || num_indirect > 0 {
            let roots_arr = ptr.array_type(num_roots as u32);
            let ind_arr = ptr.array_type(if masked { 0 } else { num_indirect as u32 });
            let mut fields: Vec<BasicTypeEnum> = vec![ptr.into(), ptr.into(), roots_arr.into()];
            if masked {
                fields.extend(value_indirect.iter()
                    .map(|(i, _)| self.llvm_ty(&f.locals[*i]).expect("value-with-ref local has a type")));
            } else {
                fields.push(ind_arr.into());
            }
            let frame_ty = self.ctx.struct_type(&fields, false);
            if masked {
                for (k, (_, offs)) in value_indirect.iter().enumerate() {
                    let base = self.target_data.offset_of_element(&frame_ty, 3 + k as u32).unwrap();
                    indirect_offsets.extend(offs.iter().map(|&off| (base + off as u64) as u32));
                }
                if !indirect_offsets.is_empty() {
                    let i32t = self.ctx.i32_type();
                    let words = i32t.const_array(&indirect_offsets.iter().map(|&o| i32t.const_int(o as u64, false)).collect::<Vec<_>>());
                    let g = self.module.add_global(words.get_type(), None, &format!("__indirect_{}", f.name));
                    g.set_constant(true);
                    g.set_initializer(&words);
                    indirect_offsets_global = Some(g.as_pointer_value());
                }
            }
            let frame = self.builder.build_alloca(frame_ty, "gcframe").unwrap();
            let origin = self.frame_origin(&f.name, num_roots as u32, num_indirect as u32, indirect_offsets_global);
            if defer_frame { deferred_frame_origin = Some(origin); }
            else {
                let origin_field = self.builder.build_struct_gep(frame_ty, frame, 1, "origin.f").unwrap();
                self.builder.build_store(origin_field, origin).unwrap();
            }
            // link: parent = thread.top_frame; thread.top_frame = &frame
            let tf_ptr = self.thread_field_ptr(func, crate::runtime::thread_offsets::TOP_FRAME);
            if !defer_frame {
                let prev = self.builder.build_load(ptr, tf_ptr, "prevtop").unwrap();
                let parent_field = self.builder.build_struct_gep(frame_ty, frame, 0, "parent.f").unwrap();
                self.builder.build_store(parent_field, prev).unwrap();
                self.builder.build_store(tf_ptr, frame).unwrap();
            }
            // Zero all direct root slots.
            let roots_field = self.builder.build_struct_gep(frame_ty, frame, 2, "roots.f").unwrap();
            for k in 0..if defer_frame { 0 } else { num_roots } {
                let slot = unsafe {
                    self.builder.build_in_bounds_gep(
                        roots_arr, roots_field,
                        &[i32t.const_zero(), i32t.const_int(k as u64, false)],
                        "rinit",
                    ).unwrap()
                };
                self.builder.build_store(slot, ptr.const_null()).unwrap();
            }
            // Zero all indirect slots (filled with alloca-interior addresses below).
            let ind_field = if masked { frame } else { self.builder.build_struct_gep(frame_ty, frame, 3, "ind.f").unwrap() };
            for k in 0..if late_roots || masked { 0 } else { num_indirect } {
                let slot = unsafe {
                    self.builder.build_in_bounds_gep(
                        ind_arr, ind_field,
                        &[i32t.const_zero(), i32t.const_int(k as u64, false)],
                        "iinit",
                    ).unwrap()
                };
                self.builder.build_store(slot, ptr.const_null()).unwrap();
            }
            Some((frame, frame_ty, roots_arr, roots_field, ind_arr, ind_field, tf_ptr))
        } else {
            None
        };

        // Keep traced mirrors separate from private reference allocas so LLVM
        // can retain references in SSA between relocation boundaries.
        let mut root_slots = vec![None; f.locals.len()];
        let mut slots: Vec<Option<PointerValue<'ctx>>> = Vec::with_capacity(f.locals.len());
        for (i, lr) in f.locals.iter().enumerate() {
            if owner(i) != i {
                // Filled from the owner's slots below.
                slots.push(None);
                continue;
            }
            if let Some(ri) = root_index[i] {
                let (_, _, roots_arr, roots_field, ..) = frame.unwrap();
                let slot = unsafe {
                    self.builder.build_in_bounds_gep(
                        roots_arr, roots_field,
                        &[i32t.const_zero(), i32t.const_int(ri as u64, false)],
                        &format!("root{}", ri),
                    ).unwrap()
                };
                if self.debug.as_ref().is_some_and(|d| d.full) {
                    // Full-debug locals remain editable in their traced slots.
                    slots.push(Some(slot));
                } else {
                    root_slots[i] = Some(slot);
                    let private = self.builder.build_alloca(ptr, &format!("ref.local{i}")).unwrap();
                    self.builder.build_store(private, ptr.const_null()).unwrap();
                    slots.push(Some(private));
                }
            } else if let Some(k) = value_indirect.iter().position(|(l, _)| *l == i).filter(|_| masked) {
                let (frame, frame_ty, ..) = frame.unwrap();
                slots.push(Some(self.builder.build_struct_gep(frame_ty, frame, 3 + k as u32, &format!("l{}", i)).unwrap()));
            } else {
                match self.llvm_ty(lr) {
                    Some(t) => slots.push(Some(self.builder.build_alloca(t, &format!("l{}", i)).unwrap())),
                    None => slots.push(None),
                }
            }
        }

        for i in 0..f.locals.len() {
            if owner(i) != i {
                slots[i] = slots[owner(i)];
                root_slots[i] = root_slots[owner(i)];
            }
        }

        // Wire indirect roots: zero each value-with-ref local's alloca (so its
        // interior refs start null and GC-safe before assignment), then store the
        // address of each interior ref into an indirect slot. The alloca is stable
        // on the stack, so these addresses are set once and the GC updates the
        // refs they point at in place.
        let mut late_indirect: Vec<(PointerValue<'ctx>, PointerValue<'ctx>)> = Vec::new();
        // Each traced local's frame slots, as mask bits.
        let mut local_bits: Vec<Vec<u32>> = vec![Vec::new(); f.locals.len()];
        for (i, ri) in root_index.iter().enumerate() {
            if let Some(ri) = ri { local_bits[i].push(*ri); }
        }
        if let Some((_, _, _, _, ind_arr, ind_field, _)) = frame {
            let mut j = 0u32;
            for (local_idx, offs) in &value_indirect {
                let alloca = slots[*local_idx].expect("value-with-ref local has an alloca");
                if !masked {
                    if let Some(vty) = self.llvm_ty(&f.locals[*local_idx]) {
                        self.builder.build_store(alloca, vty.const_zero()).unwrap();
                    }
                }
                local_bits[*local_idx].extend((0..offs.len() as u32).map(|k| num_roots as u32 + j + k));
                if masked { j += offs.len() as u32; continue; }
                for &off in offs {
                    let addr = self.obj_addr(alloca, off as u64);
                    let slot = unsafe {
                        self.builder.build_in_bounds_gep(
                            ind_arr, ind_field,
                            &[i32t.const_zero(), i32t.const_int(j as u64, false)],
                            "islot",
                        ).unwrap()
                    };
                    // A late frame records these addresses when it is linked.
                    if late_roots || masked { late_indirect.push((slot, addr)); }
                    else { self.builder.build_store(slot, addr).unwrap(); }
                    j += 1;
                }
            }
        }

        // Store incoming params (LLVM param 0 is Thread*; a closure fn has the
        // env pointer as LLVM param 1). For a closure fn, the value params map
        // to locals AFTER the capture locals.
        let is_closure = Self::is_closure_fn(f);
        let ncaptures = f.closure_captures.len();
        let nparams = f.params.len();
        let mut llvm_idx = if is_closure { 2u32 } else { 1u32 };
        // Capture locals (the first `ncaptures` locals) are initialized from the
        // env pointer (LLVM param 1), reading each capture's recorded location.
        if is_closure {
            let env = func.get_nth_param(1).unwrap().into_pointer_value();
            for cap in &f.closure_captures {
                let lty = self.llvm_ty(&f.locals[cap.local as usize]);
                if let (Some(slot), Some(lty)) = (slots[cap.local as usize], lty) {
                    let addr = self.obj_addr(env, cap.offset);
                    let v = self.builder.build_load(lty, addr, "cap").unwrap();
                    self.builder.build_store(slot, v).unwrap();
                    if !entry_roots_only && !late_roots { if let Some(root) = root_slots[cap.local as usize] { self.builder.build_store(root, v).unwrap(); } }
                }
            }
        }
        let param_local_base = if is_closure { ncaptures } else { 0 };
        for i in 0..nparams {
            let local = param_local_base + i;
            if self.llvm_ty(&f.locals[local]).is_some() {
                if let Some(slot) = slots[local] {
                    let arg = func.get_nth_param(llvm_idx).unwrap();
                    self.builder.build_store(slot, arg).unwrap();
                    if !entry_roots_only && !late_roots { if let Some(root) = root_slots[local] { self.builder.build_store(root, arg).unwrap(); } }
                }
                llvm_idx += 1;
            }
        }

        // DWARF local-variable DIEs (debugger P3). For each named local with a
        // DWARF type, emit a `dbg.declare` describing where it lives, so lldb's
        // `frame variable` shows the source name + value. Only in full-debug
        // builds — the AOT path leaves those unoptimized, so the allocas survive
        // (an optimized build would coalesce them away and the declares too).
        // Params get a parameter DIE (lldb lists them as arguments); other named
        // locals get an auto-variable DIE.
        //
        // Location: a plain alloca local points at the alloca (empty expr). A
        // `Ref` local lives in a GC frame ROOT slot — a `gep` into the `gcframe`
        // alloca, which has NO simple DWARF location — so we instead point at the
        // gcframe alloca and append `DW_OP_plus_uconst <slot-offset>`, giving a
        // real `fbreg+offset` location the collector also keeps current (it
        // updates the slot in place on relocation — the §5 moving-GC property).
        if let (Some(d), Some(sp)) = (&self.debug, subprogram) {
            if d.full {
                let (sid, fline, _) = self.fn_debug_loc(f);
                let file = d.files.get(sid as usize).copied().unwrap_or(d.files[0]);
                let scope = sp.as_debug_info_scope();
                let dloc = d.di.create_debug_location(self.ctx, fline, 0, scope, None);
                for (i, repr) in f.locals.iter().enumerate() {
                    let name = match f.local_names.get(i).and_then(|n| n.as_deref()) {
                        Some(n) => n,
                        None => continue,
                    };
                    if slots[i].is_none() {
                        continue;
                    }
                    // Compute (DWARF type, storage, location expression):
                    // `gcframe` = `{ ptr parent, ptr origin, [roots], [ind] }`, so
                    // root `ri` is at byte offset 16 + ri*8.
                    const DW_OP_PLUS_UCONST: i64 = 0x23;
                    const DW_OP_DEREF: i64 = 0x06;
                    let (dty, storage, expr) = if let Repr::Ref(lid) = repr {
                        let frame_ptr = frame.expect("Ref local implies a gcframe").0;
                        let ri = root_index[i].expect("Ref local implies a root slot");
                        let off = 16i64 + ri as i64 * 8;
                        match d.layout_types.get(*lid as usize).copied().flatten() {
                            // Modeled struct: the variable IS the struct living at
                            // `*(frame+off)` (the slot holds the object pointer the
                            // collector keeps current) — `+off` then `deref`. lldb
                            // renders `p = (Point) { x = 3, y = 4 }` directly.
                            Some(st) => (
                                st,
                                frame_ptr,
                                d.di.create_expression(vec![DW_OP_PLUS_UCONST, off, DW_OP_DEREF]),
                            ),
                            // Unmodeled (enum/opaque/varlen): show the slot's
                            // pointer value as a raw address (no deref).
                            None => {
                                let addr = match d.di.create_basic_type("ptr", 64, 1, DIFlags::ZERO) {
                                    Ok(t) => t.as_type(),
                                    Err(_) => continue,
                                };
                                (addr, frame_ptr, d.di.create_expression(vec![DW_OP_PLUS_UCONST, off]))
                            }
                        }
                    } else {
                        match self.di_type_for_repr(repr) {
                            Some(t) => (t, slots[i].unwrap(), d.di.create_expression(vec![])),
                            None => continue,
                        }
                    };
                    let is_param = i >= param_local_base && i < param_local_base + nparams;
                    let var = if is_param {
                        let arg_no = (i - param_local_base + 1) as u32;
                        d.di.create_parameter_variable(
                            scope, name, arg_no, file, fline, dty,
                            /* always_preserve */ true, DIFlags::ZERO,
                        )
                    } else {
                        d.di.create_auto_variable(
                            scope, name, file, fline, dty,
                            /* always_preserve */ true, DIFlags::ZERO, /* align */ 0,
                        )
                    };
                    // Emit the `dbg.declare` record directly via llvm-sys,
                    // bypassing inkwell's `insert_declare_at_end`: under LLVM 21
                    // that wrapper calls the record API but then casts the
                    // returned `DbgRecord` to a value and asserts it is an
                    // instruction — UB that panics nondeterministically. We only
                    // need the side effect (the record attached to `entry`), so
                    // we call the C function and discard its return.
                    unsafe {
                        inkwell::llvm_sys::debuginfo::LLVMDIBuilderInsertDeclareRecordAtEnd(
                            d.di.as_mut_ptr(),
                            storage.as_value_ref(),
                            var.as_mut_ptr(),
                            expr.as_mut_ptr(),
                            dloc.as_mut_ptr(),
                            entry.as_mut_ptr(),
                        );
                    }
                }
            }
        }

        let unlink = frame.map(|(frame_ptr, frame_ty, _, _, _, _, tf_ptr)| (frame_ptr, frame_ty, tf_ptr)
        );
        for i in 0..f.locals.len() {
            if owner(i) != i { local_bits[i] = local_bits[owner(i)].clone(); }
        }
        let scope: Vec<LocalId> = (0..incoming).filter(|&i| !local_bits[i].is_empty()).map(|i| i as LocalId).collect();

        let mut fcx = FnCtx {
            func,
            slots,
            root_slots,
            frame_active,
            deferred_frame_origin,
            local_reprs: f.locals.clone(),
            thread: func.get_nth_param(0).unwrap().into_pointer_value(),
            loops: Vec::new(),
            loop_headers: Vec::new(),
            loop_ffi_bases: Vec::new(),
            onces: Vec::new(),
            ffi_stack_tokens: Vec::new(),
            unlink,
            pending_copy_outs: Vec::new(),
            pending_ffi_buffers: Vec::new(),
            phase: None,
            late_roots,
            late_indirect,
            subprogram,
            masked: masked && frame.is_some(),
            scope,
            loop_scopes: Vec::new(),
            once_scopes: Vec::new(),
            shared_slot,
            local_bits,
            frame_dims: (f.name.clone(), num_roots as u32, num_indirect as u32, indirect_offsets_global),
            site_origins: std::cell::RefCell::new(HashMap::new()),
        };
        let phase = self.entry_alloca(self.ctx.i8_type().as_basic_type_enum(), "copy.phase.local");
        let current = self.load_copy_phase(&fcx);
        self.builder.build_store(phase, current).unwrap();
        fcx.phase = Some(phase);
        if entry_roots_only { self.emit_entry_only_poll(&mut fcx); }
        else if self.bounded_loops.entry_poll(id as usize) { self.emit_safepoint_poll(&fcx); }
        let val = self.gen_block(&mut fcx, &f.body)?;

        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            self.emit_unlink(&fcx);
            match val {
                Some(v) => { self.builder.build_return(Some(&v)).unwrap(); }
                // No tail value. If the function returns non-unit, the body must
                // have diverged (a `loop`/`return` tail, e.g. `swap`'s retry
                // loop) and this fall-through is unreachable — emitting `ret void`
                // against a non-void signature is invalid. Emit `unreachable`.
                None if self.llvm_ty(&f.ret).is_some() => { self.builder.build_unreachable().unwrap(); }
                None => { self.builder.build_return(None).unwrap(); }
            }
        }
        Ok(())
    }

    /// Ref locals have an escaped traced mirror and a private working slot.
    /// Every assignment updates both. A managed call can relocate mirrors;
    /// reload working slots if a world pause occurred during the call. LLVM can
    /// promote private slots to SSA and retain registers on a poll's fast path.
    fn reload_root_mirrors(&self, fcx: &FnCtx<'ctx>) {
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        for (slot, root) in fcx.slots.iter().zip(&fcx.root_slots) {
            if let (Some(slot), Some(root)) = (slot, root) {
                let relocated = self.builder.build_load(ptr, *root, "root.relocated").unwrap();
                self.builder.build_store(*slot, relocated).unwrap();
            }
        }
    }

    fn load_relocation_epoch(&self, fcx: &FnCtx<'ctx>) -> IntValue<'ctx> {
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let address = self.obj_addr(fcx.thread, crate::runtime::thread_offsets::RELOCATION_EPOCH as u64);
        let epoch = self.builder.build_load(ptr, address, "gc.epoch_ptr").unwrap().into_pointer_value();
        let value = self.builder.build_load(self.ctx.i64_type(), epoch, "gc.epoch").unwrap();
        value.as_instruction_value().unwrap().set_alignment(8).unwrap();
        // The v3 runtime updates this owner-local word only while the owner
        // is quiescent. The call/poll that resumes execution orders relocation.
        value.into_int_value()
    }

    /// Deferred frames have no live references until registration. Their
    /// mirrors are initialized on registration, never read while inactive.
    fn reload_active_root_mirrors(&self, fcx: &FnCtx<'ctx>) {
        let ready = if let Some(flag) = fcx.frame_active {
            let active = self.builder.build_load(self.ctx.bool_type(), flag, "roots.active_ffi").unwrap().into_int_value();
            let reload = self.ctx.append_basic_block(fcx.func, "roots.ffi.reload");
            let ready = self.ctx.append_basic_block(fcx.func, "roots.ffi.ready");
            self.builder.build_conditional_branch(active, reload, ready).unwrap();
            self.builder.position_at_end(reload);
            Some(ready)
        } else { None };
        self.reload_root_mirrors(fcx);
        if let Some(ready) = ready {
            self.builder.build_unconditional_branch(ready).unwrap();
            self.builder.position_at_end(ready);
        }
    }

    /// Inactive deferred frames have no live references. Avoid even acquire
    /// epoch loads on those paths; these loads otherwise survive LLVM DCE.
    fn prepare_root_reload(&self, fcx: &FnCtx<'ctx>) -> Option<(IntValue<'ctx>, Option<IntValue<'ctx>>)> {
        if !fcx.root_slots.iter().any(Option::is_some) { return None; }
        let active = fcx.frame_active.map(|flag| {
            self.builder.build_load(self.ctx.bool_type(), flag, "roots.were_active").unwrap().into_int_value()});
        let before = if let Some(active) = active {
            let inactive = self.builder.get_insert_block().unwrap();
            let read = self.ctx.append_basic_block(fcx.func, "roots.epoch.read");
            let ready = self.ctx.append_basic_block(fcx.func, "roots.epoch.ready");
            self.builder.build_conditional_branch(active, read, ready).unwrap();
            self.builder.position_at_end(read);
            let epoch = self.load_relocation_epoch(fcx);
            self.builder.build_unconditional_branch(ready).unwrap();
            self.builder.position_at_end(ready);
            let before = self.builder.build_phi(self.ctx.i64_type(), "roots.epoch.before").unwrap();
            before.add_incoming(&[(&self.ctx.i64_type().const_zero(), inactive), (&epoch, read)]);
            before.as_basic_value().into_int_value()
        } else { self.load_relocation_epoch(fcx) };
        Some((before, active))
    }

    /// Branch to `then` when `condition` holds, which is rare.
    fn build_unlikely_branch(&self, condition: IntValue<'ctx>, then: inkwell::basic_block::BasicBlock<'ctx>, otherwise: inkwell::basic_block::BasicBlock<'ctx>) {
        let branch = self.builder.build_conditional_branch(condition, then, otherwise).unwrap();
        let weights = self.ctx.metadata_node(&[
            self.ctx.metadata_string("branch_weights").into(),
            self.ctx.i32_type().const_int(1, false).into(),
            self.ctx.i32_type().const_int(1024, false).into(),
        ]);
        branch.set_metadata(weights, self.ctx.get_kind_id("prof")).unwrap();
    }

    fn reload_if_relocated(&self, fcx: &FnCtx<'ctx>, before: IntValue<'ctx>, active: Option<IntValue<'ctx>>) {
        let ready = self.ctx.append_basic_block(fcx.func, "roots.ready");
        if let Some(active) = active {
            let check = self.ctx.append_basic_block(fcx.func, "roots.epoch.check");
            self.builder.build_conditional_branch(active, check, ready).unwrap();
            self.builder.position_at_end(check);
        }
        let after = self.load_relocation_epoch(fcx);
        let changed = self.builder.build_int_compare(IntPredicate::NE, before, after, "gc.relocated").unwrap();
        let reload = self.ctx.append_basic_block(fcx.func, "roots.reload");
        self.build_unlikely_branch(changed, reload, ready);
        self.builder.position_at_end(reload);
        self.reload_root_mirrors(fcx);
        self.builder.build_unconditional_branch(ready).unwrap();
        self.builder.position_at_end(ready);
    }

    /// A runtime length guard proves the legacy call cannot do unbounded work.
    /// Only the versioned polling call can relocate; keep epoch/root reloads on
    /// that branch. Distinct symbols preserve old callers' noncollecting ABI.
    fn build_string_scan_call(&self, fcx: &FnCtx<'ctx>, legacy: &str,
        args: &[inkwell::values::BasicMetadataValueEnum<'ctx>], name: &str) -> BasicValueEnum<'ctx>
    {
        let source = args[1].into_pointer_value();
        let address = self.obj_addr(source, <crate::gc::Full as crate::gc::ObjHeader>::SIZE as u64);
        let len = self.builder.build_load(self.ctx.i64_type(), address, "scan.length").unwrap().into_int_value();
        let small = self.builder.build_int_compare(IntPredicate::ULE, len,
            self.ctx.i64_type().const_int(crate::runtime::STRING_SCAN_LEAF_BYTES as u64, false), "scan.is_small").unwrap();
        let leaf = self.ctx.append_basic_block(fcx.func, "scan.leaf");
        let polling = self.ctx.append_basic_block(fcx.func, "scan.polling");
        let ready = self.ctx.append_basic_block(fcx.func, "scan.ready");
        self.builder.build_conditional_branch(small, leaf, polling).unwrap();
        self.builder.position_at_end(leaf);
        let fast = call_result(self.builder.build_call(self.module.get_function(legacy).unwrap(), args, name).unwrap());
        self.builder.build_unconditional_branch(ready).unwrap();
        self.builder.position_at_end(polling);
        let slow = call_result(self.build_managed_call(fcx, self.module.get_function(&format!("{legacy}_v2")).unwrap(), args, name).unwrap());
        let polled = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(ready).unwrap();
        self.builder.position_at_end(ready);
        let result = self.builder.build_phi(fast.get_type(), name).unwrap();
        result.add_incoming(&[(&fast, leaf), (&slow, polled)]);
        result.as_basic_value()
    }

    fn build_managed_call(&self, fcx: &FnCtx<'ctx>, callee: FunctionValue<'ctx>, args: &[inkwell::values::BasicMetadataValueEnum<'ctx>], name: &str)
        -> Result<inkwell::values::CallSiteValue<'ctx>, inkwell::builder::BuilderError>
    {
        let symbol = callee.get_name();
        let symbol = symbol.to_str().unwrap_or("");
        // Returning paths of these routines cannot relocate, or deliberately
        // leave the thread BLOCKED. Terminal failures may park for diagnostics
        // but never return to read derived pointers. Never read root slots
        // between ffi_enter and leave.
        let managed_id = self.funcs.iter().find_map(|(&id, &function)| (function == callee).then_some(id));
        let no_relocation = if let Some(id) = managed_id { self.nonrelocating[id as usize] } else { symbol.starts_with("llvm.") || crate::codegen_effects::native_atomic_is_nonrelocating(symbol) || matches!(symbol,
            "ai_ffi_buffer_alloc_v1" | "ai_ffi_buffer_free_v1" | "ai_ffi_buffer_data_v2" |
            "ai_ffi_enter" | "ai_ffi_exit" | "ai_gc_record_alloc" | "ai_gc_write_barrier" |
            "ai_object_type_id" | "ai_closure_code_ptr" | "ai_managed_try_write" |
            "ai_slot_bits8_load" | "ai_slot_bits8_store" | "ai_slot_bits16_load" | "ai_slot_bits16_store" |
            "ai_slot_bits32_load" | "ai_slot_bits32_store" | "ai_slot_bits64_load" | "ai_slot_bits64_store" | "ai_slot_ref_store" |
            "ai_atom_ref_load" | "ai_atom_ref_cas" |
            "ai_atom_bits8_load" | "ai_atom_bits8_cas" | "ai_atom_bits16_load" | "ai_atom_bits16_cas" |
            "ai_atom_bits32_load" | "ai_atom_bits32_cas" | "ai_atom_bits64_load" | "ai_atom_bits64_cas" |
            "ai_str_len" | "ai_str_eq" | "ai_str_get" | "ai_str_hash" | "ai_str_to_float" |
            "ai_bounds_fail" | "ai_uninitialized_array_fail" | "ai_arithmetic_fail" | "ai_panic"
        ) };
        let has_roots = fcx.root_slots.iter().any(Option::is_some);
        // A collection may run while this thread is in foreign code, so the
        // frame must be visible before `ai_ffi_enter`, not only before calls
        // that may relocate on return.
        if (fcx.late_roots || fcx.masked) && (!no_relocation || symbol == "ai_ffi_enter") { self.ensure_root_frame_linked(fcx); }
        if fcx.masked && (!no_relocation || symbol == "ai_ffi_enter") { self.store_site_origin(fcx); }
        // ffi_leave begins BLOCKED, after native callbacks may already have
        // moved roots. Reload unconditionally after it returns RUNNING.
        let before = if has_roots && !no_relocation && symbol != "ai_ffi_leave" {
            self.prepare_root_reload(fcx)
        } else { None };
        let call = self.builder.build_call(callee, args, name)?;
        if !no_relocation { self.refresh_copy_phase(fcx); }
        if let Some((before, active)) = before { self.reload_if_relocated(fcx, before, active); }
        else if has_roots && symbol == "ai_ffi_leave" { self.reload_active_root_mirrors(fcx); }
        Ok(call)
    }

    fn ensure_root_frame_linked(&self, fcx: &FnCtx<'ctx>) {
        if let (Some(flag), Some((frame, frame_ty, tf_ptr))) = (fcx.frame_active, fcx.unlink) {
            let active = self.builder.build_load(self.ctx.bool_type(), flag, "roots.active_now").unwrap().into_int_value();
            let link = self.ctx.append_basic_block(fcx.func, "roots.link");
            let ready = self.ctx.append_basic_block(fcx.func, "roots.linked");
            self.builder.build_conditional_branch(active, ready, link).unwrap();
            self.builder.position_at_end(link);
            let origin_field = self.builder.build_struct_gep(frame_ty, frame, 1, "origin.f").unwrap();
            self.builder.build_store(origin_field, fcx.deferred_frame_origin.unwrap()).unwrap();
            let roots = frame_ty.get_field_type_at_index(2).unwrap().into_array_type();
            let roots_field = self.builder.build_struct_gep(frame_ty, frame, 2, "roots.f").unwrap();
            if fcx.masked {
                // Mirrors are already current; only the interior addresses.
                for (slot, addr) in &fcx.late_indirect {
                    self.builder.build_store(*slot, *addr).unwrap();
                }
            } else if fcx.late_roots {
                // No safepoint has run yet: the working slots are current.
                let ptr = self.ctx.ptr_type(AddressSpace::default());
                for (working, mirror) in fcx.slots.iter().zip(&fcx.root_slots) {
                    if let (Some(working), Some(mirror)) = (working, mirror) {
                        let value = self.builder.build_load(ptr, *working, "root.current").unwrap();
                        self.builder.build_store(*mirror, value).unwrap();
                    }
                }
                for (slot, addr) in &fcx.late_indirect {
                    self.builder.build_store(*slot, *addr).unwrap();
                }
            } else {
                for index in 0..roots.len() {
                    let slot = unsafe { self.builder.build_in_bounds_gep(roots, roots_field, &[self.ctx.i32_type().const_zero(), self.ctx.i32_type().const_int(index as u64, false)], "root.init").unwrap() };
                    self.builder.build_store(slot, self.ctx.ptr_type(AddressSpace::default()).const_null()).unwrap();
                }
            }
            let parent_field = self.builder.build_struct_gep(frame_ty, frame, 0, "parent.f").unwrap();
            let parent = self.builder.build_load(self.ctx.ptr_type(AddressSpace::default()), tf_ptr, "prevtop").unwrap();
            self.builder.build_store(parent_field, parent).unwrap();
            self.builder.build_store(tf_ptr, frame).unwrap();
            self.builder.build_store(flag, self.ctx.bool_type().const_int(1, false)).unwrap();
            self.builder.build_unconditional_branch(ready).unwrap();
            self.builder.position_at_end(ready);
        }
    }

    /// Record reference `v`'s new value in its traced mirror.
    fn store_mirror(&self, fcx: &FnCtx<'ctx>, root: PointerValue<'ctx>, v: BasicValueEnum<'ctx>) {
        if !fcx.masked { self.ensure_root_frame_linked(fcx); }
        self.builder.build_store(root, v).unwrap();
    }

    /// Before a call that may reach a safepoint: point the frame at the
    /// descriptor naming the slots of the traced locals now in scope.
    fn store_site_origin(&self, fcx: &FnCtx<'ctx>) {
        let Some((frame, frame_ty, _)) = fcx.unlink else { return };
        let (name, roots, indirect, offsets) = &fcx.frame_dims;
        let mut mask = vec![0u64; (*roots as usize + *indirect as usize).div_ceil(64).max(1)];
        for local in &fcx.scope {
            for &bit in &fcx.local_bits[*local as usize] { mask[bit as usize / 64] |= 1 << (bit % 64); }
        }
        let origin = *fcx.site_origins.borrow_mut().entry(mask.clone())
            .or_insert_with(|| self.site_origin(name, *roots, *indirect, &mask, *offsets));
        let field = self.builder.build_struct_gep(frame_ty, frame, 1, "origin.site").unwrap();
        self.builder.build_store(field, origin).unwrap();
    }

    /// A `FrameOrigin` whose `live` mask is `mask`.
    fn site_origin(&self, fn_name: &str, num_roots: u32, num_indirect: u32, mask: &[u64], offsets: Option<PointerValue<'ctx>>) -> PointerValue<'ctx> {
        let i32t = self.ctx.i32_type();
        let i64t = self.ctx.i64_type();
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let words = i64t.const_array(&mask.iter().map(|w| i64t.const_int(*w, false)).collect::<Vec<_>>());
        let mask_global = self.module.add_global(words.get_type(), None, &format!("__live_{}", fn_name));
        mask_global.set_constant(true);
        mask_global.set_initializer(&words);
        let origin_ty = self.ctx.struct_type(&[i32t.into(), i32t.into(), ptr.into(), ptr.into(), ptr.into()], false);
        let g = self.module.add_global(origin_ty, None, &format!("__origin_{}", fn_name));
        g.set_constant(true);
        g.set_initializer(&origin_ty.const_named_struct(&[
            i32t.const_int(num_roots as u64, false).into(),
            i32t.const_int(num_indirect as u64, false).into(),
            ptr.const_null().into(),
            mask_global.as_pointer_value().into(),
            offsets.unwrap_or(ptr.const_null()).into(),
        ]));
        g.as_pointer_value()
    }

    /// Restore the parent only if this function registered its frame.
    fn emit_unlink(&self, fcx: &FnCtx<'ctx>) {
        if let Some((frame, frame_ty, tf_ptr)) = fcx.unlink {
            let ready = if let Some(flag) = fcx.frame_active {
                let active = self.builder.build_load(self.ctx.bool_type(), flag, "roots.active_return").unwrap().into_int_value();
                let unlink = self.ctx.append_basic_block(fcx.func, "roots.unlink");
                let ready = self.ctx.append_basic_block(fcx.func, "roots.return");
                self.builder.build_conditional_branch(active, unlink, ready).unwrap();
                self.builder.position_at_end(unlink);
                Some(ready)
            } else { None };
            let parent_field = self.builder.build_struct_gep(frame_ty, frame, 0, "parent.r").unwrap();
            let parent = self.builder.build_load(self.ctx.ptr_type(AddressSpace::default()), parent_field, "parent.v").unwrap();
            self.builder.build_store(tf_ptr, parent).unwrap();
            if let Some(ready) = ready {
                self.builder.build_unconditional_branch(ready).unwrap();
                self.builder.position_at_end(ready);
            }
        }
    }

    /// Pointer to a field of the Thread struct at `offset` (via ptr arithmetic).
    fn thread_field_ptr(&self, func: FunctionValue<'ctx>, offset: usize) -> PointerValue<'ctx> {
        self.obj_addr(func.get_nth_param(0).unwrap().into_pointer_value(), offset as u64)
    }

    /// A private constant `FrameOrigin { num_roots, pad, name }` global.
    fn frame_origin(&self, fn_name: &str, num_roots: u32, num_indirect: u32, offsets: Option<PointerValue<'ctx>>) -> PointerValue<'ctx> {
        let i32t = self.ctx.i32_type();
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        // FrameOrigin (runtime, #[repr(C)]):
        // { u32 num_roots, u32 num_indirect, ptr name, ptr live, ptr indirect_offsets }.
        let origin_ty = self.ctx.struct_type(&[i32t.into(), i32t.into(), ptr.into(), ptr.into(), ptr.into()], false);
        let g = self.module.add_global(origin_ty, None, &format!("__origin_{}", fn_name));
        g.set_constant(true);
        g.set_initializer(&origin_ty.const_named_struct(&[
            i32t.const_int(num_roots as u64, false).into(),
            i32t.const_int(num_indirect as u64, false).into(),
            ptr.const_null().into(),
            ptr.const_null().into(),
            offsets.unwrap_or(ptr.const_null()).into(),
        ]));
        g.as_pointer_value()
    }

    fn clear_dead_roots(&self, fcx: &FnCtx<'ctx>, statement: &CoreStmt) {
        let Some(dead) = self.root_liveness.dead(statement) else { return; };
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        for local in dead {
            // A shared slot may already hold a later local: a compound
            // statement's dead set names locals defined inside it.
            if fcx.shared_slot[*local as usize] { continue; }
            if let Some(root) = fcx.root_slots[*local as usize] {
                self.builder.build_store(root, ptr.const_null()).unwrap();
                // A late frame copies working slots when it is linked.
                if fcx.late_roots {
                    if let Some(working) = fcx.slots[*local as usize] { self.builder.build_store(working, ptr.const_null()).unwrap(); }
                }
            }
        }
        // Indirect value roots retain their original lifetime for now. Clearing
        // their inline bytes would also affect the private working aggregate.
    }

    fn gen_block(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        b: &CoreBlock,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // A `let` stays in scope past its block: later code may reuse it
        // (field-read reuse makes Core uses non-lexical). Joins meet scopes.
        self.gen_block_stmts(fcx, b)
    }

    fn gen_block_stmts(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        b: &CoreBlock,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        for s in &b.stmts {
            match s {
                CoreStmt::Let(local, e) => {
                    let v = self.gen_expr(fcx, e)?;
                    if self.builder.get_insert_block().unwrap().get_terminator().is_some() { return Ok(None); }
                    if let (Some(slot), Some(v)) = (fcx.slots[*local as usize], v) {
                        self.builder.build_store(slot, v).unwrap();
                        if let Some(root) = fcx.root_slots[*local as usize] { self.store_mirror(fcx, root, v); }
                    }
                    if !fcx.local_bits[*local as usize].is_empty() { fcx.scope.push(*local); }
                }
                CoreStmt::Expr(e) => {
                    self.gen_expr(fcx, e)?;
                }
            }
            if self.builder.get_insert_block().unwrap().get_terminator().is_some() {
                return Ok(None);
            }
            self.clear_dead_roots(fcx, s);
        }
        match &b.tail {
            Some(e) => self.gen_expr(fcx, e),
            None => Ok(None),
        }
    }

    fn gen_expr(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        e: &CoreExpr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // DWARF (P2): stamp this node's instructions with its source location.
        // One function = one source, so the scope is the function's DISubprogram
        // and the location's file stays consistent. Nodes without a span keep the
        // last location (the function default at minimum) — no fabrication.
        if let (Some(d), Some(sp)) = (&self.debug, fcx.subprogram) {
            if let Some((_, line, col)) = self.prog.span_source_loc(e.span) {
                let loc =
                    d.di.create_debug_location(self.ctx, line as u32, col as u32, sp.as_debug_info_scope(), None);
                self.builder.set_current_debug_location(loc);
            }
        }
        match &*e.kind {
            CoreExprKind::ConstInt(n, sr) => {
                let t = self.scalar_ty(*sr).into_int_type();
                if sr.bits() == 128 {
                    return Ok(Some(t.const_int_arbitrary_precision(&[*n as u64, (*n >> 64) as u64]).into()));
                }
                Ok(Some(t.const_int(*n as u64, sr.is_signed()).into()))
            }
            CoreExprKind::ConstFloat(f, sr) => {
                let t = self.scalar_ty(*sr).into_float_type();
                Ok(Some(t.const_float(*f).into()))
            }
            CoreExprKind::ConstBool(b) => Ok(Some(self.ctx.bool_type().const_int(*b as u64, false).into()))
            ,
            CoreExprKind::ConstChar(c) => {
                Ok(Some(self.ctx.i32_type().const_int(*c as u64, false).into()))
            }
            CoreExprKind::ConstStr(s) => self.gen_const_str(fcx, s, &e.repr, e.span),
            CoreExprKind::ConstZero(repr) => {
                // Zero/null of the repr: scalars → 0, refs → null pointer.
                match self.llvm_ty(repr) {
                    Some(ty) => Ok(Some(ty.const_zero())),
                    None => Ok(None),
                }
            }
            CoreExprKind::Unit => Ok(None),
            CoreExprKind::Local(id) => {
                if let Repr::Scalar(scalar) = e.repr {
                    if scalar.is_int() {
                        if let Some(value) = self.private_heap.integer(e) {
                            return Ok(Some(self.scalar_ty(scalar).into_int_type().const_int(value, scalar.is_signed()).into()));
                        }
                    }
                }
                let slot = fcx.slots[*id as usize];
                match slot {
                    Some(slot) => {
                        let ty = self.llvm_ty(&fcx.local_reprs[*id as usize]).unwrap();
                        let v = self.builder.build_load(ty, slot, "ld").unwrap();
                        Ok(Some(v))
                    }
                    None => Ok(None),
                }
            }
            CoreExprKind::Bin(op, l, r) => self.gen_bin(fcx, *op, l, r),
            CoreExprKind::Un(op, inner) => self.gen_un(fcx, *op, inner),
            CoreExprKind::FloatIntrinsic(intr, inner) => {
                self.gen_float_intrinsic(fcx, *intr, inner)}
            CoreExprKind::Print(inner) => {
                let v = self.gen_expr(fcx, inner)?.unwrap();
                let is_float = matches!(&inner.repr, Repr::Scalar(s) if s.is_float());
                let fname = if is_float { "ai_print_float" } else { "ai_print_int" };
                // Widen integer prints to i64.
                let arg: BasicValueEnum = if is_float {
                    if matches!(&inner.repr, Repr::Scalar(ScalarRepr::F32)) {
                        self.builder.build_float_ext(v.into_float_value(), self.ctx.f64_type(), "pw").unwrap().into()
                    } else { v }
                } else {
                    let iv = v.into_int_value();
                    let signed = matches!(&inner.repr, Repr::Scalar(s) if s.is_signed());
                    if iv.get_type().get_bit_width() < 64 {
                        if signed {
                            self.builder.build_int_s_extend(iv, self.ctx.i64_type(), "pw").unwrap().into()
                        } else {
                            self.builder.build_int_z_extend(iv, self.ctx.i64_type(), "pw").unwrap().into()
                        }
                    } else { iv.into() }
                };
                let f = self.module.get_function(fname).unwrap();
                let r = call_result(self.build_managed_call(fcx, f, &[fcx.thread.into(), arg.into()], "print").unwrap());
                Ok(Some(r))
            }
            CoreExprKind::PrintStr(s) => {
                let sv = self.gen_expr(fcx, s)?.unwrap().into_pointer_value();
                let f = self.module.get_function("ai_print_str").unwrap();
                let r = call_result(self.build_managed_call(fcx, f, &[fcx.thread.into(), sv.into()], "prints").unwrap());
                Ok(Some(r))
            }
            CoreExprKind::PrintStrRaw(s) => {
                let sv = self.gen_expr(fcx, s)?.unwrap().into_pointer_value();
                let f = self.module.get_function("ai_print_str_raw").unwrap();
                let r = call_result(self.build_managed_call(fcx, f, &[fcx.thread.into(), sv.into()], "printsr").unwrap());
                Ok(Some(r))
            }
            CoreExprKind::StrLen(s) => {
                // Strings are immutable: the byte count is readable in any GC phase.
                let sv = self.gen_expr(fcx, s)?.unwrap().into_pointer_value();
                Ok(Some(self.builder.build_load(self.ctx.i64_type(), self.obj_addr(sv, Self::HEADER), "strlen").unwrap()))
            }
            CoreExprKind::StrEqLit(a, lit) => {
                let av = self.gen_expr(fcx, a)?.unwrap().into_pointer_value();
                let bytes = self.ctx.const_string(lit.as_bytes(), false);
                let global = self.module.add_global(bytes.get_type(), None, "str.lit");
                global.set_initializer(&bytes);
                global.set_constant(true);
                global.set_linkage(inkwell::module::Linkage::Private);
                global.set_unnamed_addr(true);
                // Strings are immutable flat byte arrays, readable in any
                // GC phase: compare the length, then the bytes.
                let i64t = self.ctx.i64_type();
                let len = i64t.const_int(lit.len() as u64, false);
                let count = self.builder.build_load(i64t, self.obj_addr(av, Self::HEADER), "str.len").unwrap().into_int_value();
                let same_len = self.builder.build_int_compare(IntPredicate::EQ, count, len, "str.same_len").unwrap();
                if lit.is_empty() { return Ok(Some(same_len.into())); }
                let entry = self.builder.get_insert_block().unwrap();
                let compare = self.ctx.append_basic_block(fcx.func, "str.compare");
                let done = self.ctx.append_basic_block(fcx.func, "str.compared");
                self.builder.build_conditional_branch(same_len, compare, done).unwrap();
                self.builder.position_at_end(compare);
                let memcmp = self.module.get_function("memcmp").unwrap_or_else(|| {
                    let ptr = self.ctx.ptr_type(AddressSpace::default());
                    self.module.add_function("memcmp", self.ctx.i32_type().fn_type(&[ptr.into(), ptr.into(), i64t.into()], false), Some(inkwell::module::Linkage::External))
                });
                let data = self.obj_addr(av, Self::HEADER + 8);
                let order = call_result(self.builder.build_call(memcmp, &[data.into(), global.as_pointer_value().into(), len.into()], "str.memcmp").unwrap()).into_int_value();
                let equal = self.builder.build_int_compare(IntPredicate::EQ, order, self.ctx.i32_type().const_zero(), "str.same_bytes").unwrap();
                let compared = self.builder.get_insert_block().unwrap();
                self.builder.build_unconditional_branch(done).unwrap();
                self.builder.position_at_end(done);
                let result = self.builder.build_phi(self.ctx.bool_type(), "streqlit").unwrap();
                result.add_incoming(&[(&self.ctx.bool_type().const_zero(), entry), (&equal, compared)]);
                Ok(Some(result.as_basic_value()))
            }
            CoreExprKind::StrEq(a, b) => {
                let av = self.gen_expr(fcx, a)?.unwrap().into_pointer_value();
                let bv = self.gen_expr(fcx, b)?.unwrap().into_pointer_value();
                let r = self.build_string_scan_call(fcx, "ai_str_eq", &[fcx.thread.into(), av.into(), bv.into()], "streq");
                // ai_str_eq returns i64 (0/1); narrow to i1 for the bool repr.
                let iv = r.into_int_value();
                let b1 = self.builder.build_int_truncate(iv, self.ctx.bool_type(), "streqb").unwrap();
                Ok(Some(b1.into()))
            }
            CoreExprKind::StrConcat { layout, a, b } => {
                // `a`/`b` are live GC roots that the allocating call may move, so
                // they must already be spilled to frame slots by the root
                // discipline (same as any other runtime call that allocates).
                let av = self.gen_expr(fcx, a)?.unwrap().into_pointer_value();
                let bv = self.gen_expr(fcx, b)?.unwrap().into_pointer_value();
                let i32t = self.ctx.i32_type();
                let f = self.module.get_function("ai_str_concat").unwrap();
                let r = call_result(self.build_managed_call(fcx,
                    f,
                    &[fcx.thread.into(), i32t.const_int(*layout as u64, false).into(), av.into(), bv.into()],
                    "strcat",
                ).unwrap());
                Ok(Some(r))
            }
            CoreExprKind::StrGet(s, i) => {
                let sv = self.gen_expr(fcx, s)?.unwrap().into_pointer_value();
                let iv = self.gen_expr(fcx, i)?.unwrap().into_int_value();
                let f = self.module.get_function("ai_str_get").unwrap();
                let r = call_result(self.build_managed_call(fcx, f, &[fcx.thread.into(), sv.into(), iv.into()], "strget").unwrap());
                Ok(Some(r))
            }
            CoreExprKind::StrSubstring { layout, s, start, end } => {
                let sv = self.gen_expr(fcx, s)?.unwrap().into_pointer_value();
                let st = self.gen_expr(fcx, start)?.unwrap().into_int_value();
                let en = self.gen_expr(fcx, end)?.unwrap().into_int_value();
                let i32t = self.ctx.i32_type();
                let f = self.module.get_function("ai_str_substring").unwrap();
                let r = call_result(self.build_managed_call(fcx,
                    f,
                    &[fcx.thread.into(), i32t.const_int(*layout as u64, false).into(), sv.into(), st.into(), en.into()],
                    "strsub",
                ).unwrap());
                Ok(Some(r))
            }
            CoreExprKind::StrJoin { layout, args } => {
                let mut cargs = vec![fcx.thread.into(), self.ctx.i32_type().const_int(*layout as u64, false).into()];
                for arg in args { cargs.push(self.gen_expr(fcx, arg)?.unwrap().into()); }
                let func = self.module.get_function("ai_str_join").unwrap();
                Ok(Some(call_result(self.build_managed_call(fcx, func, &cargs, "str.join").unwrap())))
            }
            CoreExprKind::Panic(message) => {
                let message = self.gen_expr(fcx, message)?.unwrap();
                let func = self.module.get_function("ai_panic").unwrap();
                self.build_managed_call(fcx, func, &[fcx.thread.into(), message.into()], "").unwrap();
                self.builder.build_unreachable().unwrap();
                Ok(None)
            }
            CoreExprKind::HostCall { op, response, string, entries, args } => {
                let mut cargs = vec![fcx.thread.into()];
                for value in [*op, *response, *string, *entries] {
                    cargs.push(self.ctx.i32_type().const_int(value as u64, false).into());
                }
                for arg in args { cargs.push(self.gen_expr(fcx, arg)?.unwrap().into()); }
                let func = self.module.get_function("ai_host_call").unwrap();
                Ok(Some(call_result(self.build_managed_call(fcx, func, &cargs, "host.io").unwrap())))
            }
            CoreExprKind::ReadFile { layout, path } => {
                let pv = self.gen_expr(fcx, path)?.unwrap().into_pointer_value();
                let i32t = self.ctx.i32_type();
                let f = self.module.get_function("ai_read_file").unwrap();
                let r = call_result(self.build_managed_call(fcx,
                    f,
                    &[fcx.thread.into(), i32t.const_int(*layout as u64, false).into(), pv.into()],
                    "readfile",
                ).unwrap());
                Ok(Some(r))
            }
            CoreExprKind::StrToFloat(s) => {
                let sv = self.gen_expr(fcx, s)?.unwrap().into_pointer_value();
                let r = self.build_string_scan_call(fcx, "ai_str_to_float", &[fcx.thread.into(), sv.into()], "strtof");
                Ok(Some(r))
            }
            CoreExprKind::FloatBits(f) => {
                let fv = self.gen_expr(fcx, f)?.unwrap().into_float_value();
                let r = self.builder.build_bit_cast(fv, self.ctx.i64_type(), "fbits").unwrap();
                Ok(Some(r))
            }
            CoreExprKind::StrHash(s) => {
                let sv = self.gen_expr(fcx, s)?.unwrap().into_pointer_value();
                let r = self.build_string_scan_call(fcx, "ai_str_hash", &[fcx.thread.into(), sv.into()], "strhash");
                Ok(Some(r))
            }
            CoreExprKind::TypeIdOf(obj) => {
                use crate::gc::{Full, ObjHeader};
                let ov = self.gen_expr(fcx, obj)?.unwrap().into_pointer_value();
                let i16t = self.ctx.i16_type();
                let i64t = self.ctx.i64_type();
                let (idle, copying, merge) = self.managed_access_blocks(fcx);
                self.builder.position_at_end(idle);
                let addr = self.obj_addr(ov, Full::TYPE_ID_OFFSET as u64);
                let direct = self.builder.build_load(i16t, addr, "tid").unwrap();
                self.builder.build_unconditional_branch(merge).unwrap();
                self.builder.position_at_end(copying);
                let function = self.module.get_function("ai_object_type_id").unwrap();
                let resolved = call_result(self.build_managed_call(fcx, function,
                    &[fcx.thread.into(), ov.into()], "tid.scoped").unwrap());
                self.builder.build_unconditional_branch(merge).unwrap();
                self.builder.position_at_end(merge);
                let physical = self.builder.build_phi(i16t, "tid.physical").unwrap();
                physical.add_incoming(&[(&direct, idle), (&resolved, copying)]);
                let mut tid = physical.as_basic_value().into_int_value();
                // Collector shape IDs may differ between variants of one source
                // enum; the language exposes its stable nominal LayoutId.
                if let Repr::Ref(logical) = obj.repr {
                    if let Some(physical) = self.physical_layouts.empty_variants.get(&logical) {
                        let is_empty = self.builder.build_int_compare(IntPredicate::EQ,
                            tid, i16t.const_int(*physical as u64, false), "tid.compact").unwrap();
                        tid = self.builder.build_select(is_empty,
                            i16t.const_int(logical as u64, false), tid, "tid.nominal").unwrap().into_int_value();
                    }
                }
                let r = self.builder.build_int_z_extend(tid, i64t, "tid64").unwrap();
                Ok(Some(r.into()))
            }
            CoreExprKind::TypeNameOf { layout, obj } => {
                let ov = self.gen_expr(fcx, obj)?.unwrap().into_pointer_value();
                let i32t = self.ctx.i32_type();
                let f = self.module.get_function("ai_type_name").unwrap();
                let r = call_result(self.build_managed_call(fcx,
                    f,
                    &[fcx.thread.into(), i32t.const_int(*layout as u64, false).into(), ov.into()],
                    "typename",
                ).unwrap());
                Ok(Some(r))
            }
            CoreExprKind::AsCBytes { src, elem, is_string, copy_out } => {
                // Copy a String/scalar-Array's heap bytes into a scoped
                // native buffer and yield its pointer. Native storage never moves, so
                // the pointer is stable for the enclosing extern call with no
                // pinning. A `copy_out` (mut array) buffer is written back into
                // the heap object after the call (queued for `gen_call` to drain).
                // See `docs/ffi.md`.
                let sv = self.gen_expr(fcx, src)?.unwrap().into_pointer_value();
                let i64t = self.ctx.i64_type();
                let stride = (elem.bits().max(8) / 8) as u64;

                if *is_string {
                    // byte_len = ai_str_len(s); buffer = byte_len + 1 (NUL).
                    let lenf = self.module.get_function("ai_str_len").unwrap();
                    let len = call_result(
                        self.build_managed_call(fcx, lenf, &[fcx.thread.into(), sv.into()], "ascb.len").unwrap()
                    ).into_int_value();
                    let cap = self.builder.build_int_add(len, i64t.const_int(1, false), "ascb.cap").unwrap();
                    let alloc = self.module.get_function("ai_ffi_buffer_alloc_v2").unwrap();
                    let token = call_result(self.build_managed_call(fcx, alloc, &[fcx.thread.into(), cap.into()], "ascb.owner").unwrap()).into_pointer_value();
                    fcx.pending_ffi_buffers.push(token);
                    let data = self.module.get_function("ai_ffi_buffer_data_v2").unwrap();
                    let buf = call_result(self.build_managed_call(fcx, data, &[token.into()], "ascb.buf").unwrap()).into_pointer_value();
                    // Allocation may park or collect; ANF captured this exact
                    // source into a rooted local before argument evaluation.
                    let sv = self.gen_expr(fcx, src)?.unwrap().into_pointer_value();
                    let cpf = self.module.get_function("ai_str_copy_to_buf_v2").unwrap();
                    self.build_managed_call(fcx, cpf, &[fcx.thread.into(), sv.into(), buf.into()], "").unwrap();
                    return Ok(Some(buf.into()));
                }

                // Scalar array: byte_len from the varlen count word at HEADER.
                let lid = match &src.repr { Repr::Ref(l) => *l, _ => return Err(CodegenError("as_c_bytes on non-array".into())) };
                let is_values = matches!(self.prog.layouts[lid as usize].varlen, crate::core::VarLen::Values);
                let count_addr = self.obj_addr(sv, Self::HEADER);
                let count = self.builder.build_load(i64t, count_addr, "ascb.cnt").unwrap().into_int_value();
                let byte_len = if is_values {
                    self.builder.build_int_mul(count, i64t.const_int(stride, false), "ascb.bytes").unwrap()
                } else {
                    count // Bytes array: count word already holds the byte length.
                };
                let alloc = self.module.get_function("ai_ffi_buffer_alloc_v2").unwrap();
                let token = call_result(self.build_managed_call(fcx, alloc, &[fcx.thread.into(), byte_len.into()], "ascb.owner").unwrap()).into_pointer_value();
                fcx.pending_ffi_buffers.push(token);
                let data = self.module.get_function("ai_ffi_buffer_data_v2").unwrap();
                let buf = call_result(self.build_managed_call(fcx, data, &[token.into()], "ascb.buf").unwrap()).into_pointer_value();
                let sv = self.gen_expr(fcx, src)?.unwrap().into_pointer_value();
                let cin = self.module.get_function("ai_buf_copy_in_v2").unwrap();
                self.build_managed_call(fcx, cin, &[fcx.thread.into(), sv.into(), buf.into(), byte_len.into(), i64t.const_int(elem.bits() as u64, false).into()], "").unwrap();
                if *copy_out {
                    // Queue a write-back; gen_call replays it after the extern call.
                    fcx.pending_copy_outs.push((src.as_ref().clone(), buf, byte_len, elem.bits()));
                }
                Ok(Some(buf.into()))
            }
            CoreExprKind::StrFromNum { layout, is_float, v } => {
                let vv = self.gen_expr(fcx, v)?.unwrap();
                let i32t = self.ctx.i32_type();
                let fname = if *is_float { "ai_str_from_float" } else { "ai_str_from_int" };
                let f = self.module.get_function(fname).unwrap();
                let r = call_result(self.build_managed_call(fcx,
                    f,
                    &[fcx.thread.into(), i32t.const_int(*layout as u64, false).into(), vv.into()],
                    "strfromnum",
                ).unwrap());
                Ok(Some(r))
            }
            CoreExprKind::StrFromChar { layout, cp } => {
                let cpv = self.gen_expr(fcx, cp)?.unwrap();
                let i32t = self.ctx.i32_type();
                let f = self.module.get_function("ai_char_to_str").unwrap();
                let r = call_result(self.build_managed_call(fcx,
                    f,
                    &[fcx.thread.into(), i32t.const_int(*layout as u64, false).into(), cpv.into()],
                    "strfromchar",
                ).unwrap());
                Ok(Some(r))
            }
            CoreExprKind::Cast { value, from, to } => self.gen_cast(fcx, value, from, to),
            CoreExprKind::Call(fid, args) => {
                let result = self.gen_call(fcx, *fid, args)?;
                // A call that never returns ends the path, like a panic.
                if self.diverging[*fid as usize] && self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    self.builder.build_unreachable().unwrap();
                    return Ok(None);
                }
                Ok(result)
            }
            CoreExprKind::PtrReadI64(p) => {
                let pv = self.gen_expr(fcx, p)?.unwrap().into_pointer_value();
                let v = self.builder.build_load(self.ctx.i64_type(), pv, "ptrd").unwrap();
                Ok(Some(v))
            }
            CoreExprKind::CallbackPtr(fid) => {
                if self.prog.funcs[*fid as usize].is_extern {
                    // This target already has the C ABI. A managed trampoline
                    // would incorrectly prepend Thread* to its arguments.
                    return Ok(Some(self.funcs[fid].as_global_value().as_pointer_value().into()));
                }
                // The address of the synthesized C-ABI trampoline for `fid`.
                let tramp = self.callback_trampoline(*fid);
                Ok(Some(tramp.as_global_value().as_pointer_value().into()))
            }
            CoreExprKind::ThreadSpawn(closure) => {
                // Evaluate the closure env, read its code pointer (first word of
                // the raw section, like gen_call_closure), and hand both to the
                // runtime. ai_thread_spawn registers a child mutator + runs it.
                let env = self.gen_expr(fcx, closure)?.unwrap().into_pointer_value();
                // Recover the code pointer at runtime from the env's header
                // type_id — the static repr here may be the placeholder closure
                // layout (ptr_fields=0) which gives the WRONG offset for a closure
                // that captured GC pointers. See `ai_closure_code_ptr`.
                let codef = self.module.get_function("ai_closure_code_ptr").unwrap();
                let code = call_result(self.build_managed_call(fcx,
                    codef, &[fcx.thread.into(), env.into()], "code",
                ).unwrap()).into_pointer_value();
                let f = self.module.get_function("ai_thread_spawn").unwrap();
                let r = call_result(self.build_managed_call(fcx,
                    f, &[fcx.thread.into(), env.into(), code.into()], "spawn",
                ).unwrap());
                Ok(Some(r))
            }
            CoreExprKind::ThreadJoin(handle) => {
                let h = self.gen_expr(fcx, handle)?.unwrap().into_pointer_value();
                let f = self.module.get_function("ai_thread_join").unwrap();
                let r = call_result(self.build_managed_call(fcx, f, &[fcx.thread.into(), h.into()], "join").unwrap());
                Ok(Some(r))
            }
            CoreExprKind::AtomLoad { atom, elem } => {
                // Atomic load of the atom's value field (first pointer slot).
                let a = self.gen_expr(fcx, atom)?.unwrap().into_pointer_value();
                if matches!(elem, Repr::Ref(_)) {
                    let f = self.module.get_function("ai_atom_ref_load").unwrap();
                    return Ok(Some(call_result(self.build_managed_call(
                        fcx, f, &[fcx.thread.into(), a.into()], "atom.ref.load",
                    ).unwrap())));
                }
                let Repr::Scalar(scalar) = elem else {
                    return Err(CodegenError("unsupported atom representation".into()));
                };
                let width = scalar.bits().max(8);
                let f = self.module.get_function(&format!("ai_atom_bits{width}_load")).unwrap();
                let bits = call_result(self.build_managed_call(
                    fcx, f, &[fcx.thread.into(), a.into()], "atom.bits.load",
                ).unwrap()).into_int_value();
                let value = match scalar {
                    ScalarRepr::Bool => self.builder.build_int_truncate(bits, self.ctx.bool_type(), "atom.bool").unwrap().into(),
                    ScalarRepr::F32 | ScalarRepr::F64 => self.builder.build_bit_cast(bits, self.llvm_ty(elem).unwrap(), "atom.float").unwrap(),
                    ScalarRepr::Ptr => self.builder.build_int_to_ptr(bits, self.ctx.ptr_type(AddressSpace::default()), "atom.rawptr").unwrap().into(),
                    _ => bits.into(),
                };
                Ok(Some(value))
            }
            CoreExprKind::AtomCas { atom, old, new } => {
                // Atomic compare-and-swap the value field; barrier on success.
                let a = self.gen_expr(fcx, atom)?.unwrap().into_pointer_value();
                let oldv = self.gen_expr(fcx, old)?.unwrap();
                let newv = self.gen_expr(fcx, new)?.unwrap();
                if matches!(&old.repr, Repr::Ref(_)) {
                    let f = self.module.get_function("ai_atom_ref_cas").unwrap();
                    let result = call_result(self.build_managed_call(
                        fcx, f, &[fcx.thread.into(), a.into(), oldv.into(), newv.into()], "atom.ref.cas",
                    ).unwrap()).into_int_value();
                    let ok = self.builder.build_int_compare(
                        IntPredicate::NE, result, self.ctx.i8_type().const_zero(), "cas.ok",
                    ).unwrap();
                    return Ok(Some(ok.into()));
                }
                let Repr::Scalar(scalar) = &old.repr else {
                    return Err(CodegenError("unsupported atom representation".into()));
                };
                let width = scalar.bits().max(8);
                let encode = |v: BasicValueEnum<'ctx>| -> BasicValueEnum<'ctx> {
                    match scalar {
                        ScalarRepr::Bool => self.builder.build_int_z_extend(v.into_int_value(), self.ctx.i8_type(), "atom.bool.bits").unwrap().into(),
                        ScalarRepr::F32 | ScalarRepr::F64 => self.builder.build_bit_cast(v, self.ctx.custom_width_int_type(width), "atom.float.bits").unwrap(),
                        ScalarRepr::Ptr => self.builder.build_ptr_to_int(v.into_pointer_value(), self.ctx.i64_type(), "atom.rawptr.bits").unwrap().into(),
                        _ => v,
                    }
                };
                let f = self.module.get_function(&format!("ai_atom_bits{width}_cas")).unwrap();
                let result = call_result(self.build_managed_call(
                    fcx, f, &[fcx.thread.into(), a.into(), encode(oldv).into(), encode(newv).into()], "atom.bits.cas",
                ).unwrap()).into_int_value();
                let ok = self.builder.build_int_compare(
                    IntPredicate::NE, result, self.ctx.i8_type().const_zero(), "cas.ok",
                ).unwrap();
                Ok(Some(ok.into()))
            }
            CoreExprKind::ChanSend { buf, ctrl, value } => {
                let b = self.gen_expr(fcx, buf)?.unwrap().into_pointer_value();
                let c = self.gen_expr(fcx, ctrl)?.unwrap().into_pointer_value();
                let v = self.gen_expr(fcx, value)?.unwrap().into_pointer_value();
                let f = self.module.get_function("ai_chan_send").unwrap();
                let r = call_result(self.build_managed_call(fcx,
                    f, &[fcx.thread.into(), b.into(), c.into(), v.into()], "chsend").unwrap());
                Ok(Some(r))
            }
            CoreExprKind::ChanRecv { buf, ctrl, elem } => {
                let b = self.gen_expr(fcx, buf)?.unwrap().into_pointer_value();
                let c = self.gen_expr(fcx, ctrl)?.unwrap().into_pointer_value();
                let f = self.module.get_function("ai_chan_recv").unwrap();
                let value = call_result(self.build_managed_call(fcx,
                    f, &[fcx.thread.into(), b.into(), c.into()], "chrecv").unwrap()).into_pointer_value();
                let Repr::Value(option) = e.repr else { return Err(CodegenError("channel receive requires Option representation".into())); };
                let some_bb = self.ctx.append_basic_block(fcx.func, "channel.some");
                let none_bb = self.ctx.append_basic_block(fcx.func, "channel.none");
                let merge_bb = self.ctx.append_basic_block(fcx.func, "channel.result");
                let empty = self.builder.build_is_null(value, "channel.closed").unwrap();
                self.builder.build_conditional_branch(empty, none_bb, some_bb).unwrap();
                self.builder.position_at_end(some_bb);
                let some = self.build_value_variant(option, 1, &[(value.into(), elem.clone())]);
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(none_bb);
                let none = self.build_value_variant(option, 0, &[]);
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                let phi = self.builder.build_phi(self.value_struct_ty(option), "channel.option").unwrap();
                phi.add_incoming(&[(&some, some_bb), (&none, none_bb)]);
                Ok(Some(phi.as_basic_value()))
            }
            CoreExprKind::RuntimeCall { func: "ai_str_from_utf8_prefix", args, .. } => self.gen_str_from_utf8_prefix(fcx, args, e.span),
            CoreExprKind::RuntimeCall { func, args, ret } => {
                let f = self.module.get_function(func)
                    .ok_or_else(|| CodegenError(format!("unknown runtime extern `{}`", func)))?;
                let mut cargs: Vec<inkwell::values::BasicMetadataValueEnum> = vec![fcx.thread.into()];
                for a in args {
                    if let Some(v) = self.gen_expr(fcx, a)? {
                        cargs.push(v.into());
                    }
                }
                let cs = self.build_managed_call(fcx, f, &cargs, "rtcall").unwrap();
                match (cs.try_as_basic_value(), self.llvm_ty(ret)) {
                    (inkwell::values::ValueKind::Basic(v), _) => Ok(Some(v)),
                    (inkwell::values::ValueKind::Instruction(_), _) => Ok(None),
                }
            }
            CoreExprKind::ThreadSleep(ms) => {
                let m = self.gen_expr(fcx, ms)?.unwrap().into_int_value();
                let f = self.module.get_function("ai_thread_sleep").unwrap();
                self.build_managed_call(fcx, f, &[fcx.thread.into(), m.into()], "").unwrap();
                Ok(Some(self.ctx.i64_type().const_zero().into()))
            }
            CoreExprKind::ThreadYield => {
                let f = self.module.get_function("ai_thread_yield").unwrap();
                self.build_managed_call(fcx, f, &[fcx.thread.into()], "").unwrap();
                Ok(Some(self.ctx.i64_type().const_zero().into()))
            }
            CoreExprKind::ThreadCurrentId => {
                let f = self.module.get_function("ai_thread_current_id").unwrap();
                let r = call_result(self.build_managed_call(fcx, f, &[fcx.thread.into()], "tid").unwrap());
                Ok(Some(r))
            }
            CoreExprKind::If(cond, then_b, else_b) => {
                self.gen_if(fcx, cond, then_b, else_b, &e.repr)}
            CoreExprKind::Block(b) => self.gen_block(fcx, b),
            CoreExprKind::Loop(body) => self.gen_loop(fcx, body, &e.repr),
            CoreExprKind::Break(v) => {
                let mut bv = match v {
                    Some(e) => self.gen_expr(fcx, e)?,
                    None => None,
                };
                if self.builder.get_insert_block().unwrap().get_terminator().is_some() { return Ok(None); }
                let (buffer_base, stack_base) = *fcx.loop_ffi_bases.last()
                    .ok_or_else(|| CodegenError("break outside loop".into()))?;
                self.emit_ffi_cleanup(fcx, buffer_base, stack_base);
                if let Some(e) = v { self.reload_after_cleanup(fcx, e, buffer_base, &mut bv)?; }
                let (cont_bb, phi_in) = *fcx.loops.last()
                    .ok_or_else(|| CodegenError("break outside loop".into()))?;
                if let (Some(slot), Some(bv)) = (phi_in, bv) {
                    self.builder.build_store(slot, bv).unwrap();
                }
                let scope = std::mem::take(&mut fcx.scope);
                meet_scope(fcx.loop_scopes.last_mut().unwrap(), &scope);
                fcx.scope = scope;
                self.builder.build_unconditional_branch(cont_bb).unwrap();
                Ok(None)
            }
            CoreExprKind::Once { label, body } => self.gen_once(fcx, *label, body, &e.repr),
            CoreExprKind::Leave { label, value } => {
                let mut v = match value {
                    Some(e) => self.gen_expr(fcx, e)?,
                    None => None,
                };
                if self.builder.get_insert_block().unwrap().get_terminator().is_some() { return Ok(None); }
                let at = fcx.onces.iter().rposition(|o| o.0 == *label)
                    .ok_or_else(|| CodegenError("leave outside its once".into()))?;
                let (_, exit, slot, (buffer_base, stack_base)) = fcx.onces[at];
                let scope = std::mem::take(&mut fcx.scope);
                meet_scope(&mut fcx.once_scopes[at], &scope);
                fcx.scope = scope;
                self.emit_ffi_cleanup(fcx, buffer_base, stack_base);
                if let Some(e) = value { self.reload_after_cleanup(fcx, e, buffer_base, &mut v)?; }
                if let (Some(slot), Some(v)) = (slot, v) {
                    self.builder.build_store(slot, v).unwrap();
                }
                self.builder.build_unconditional_branch(exit).unwrap();
                Ok(None)
            }
            CoreExprKind::Continue => {
                let (buffer_base, stack_base) = *fcx.loop_ffi_bases.last()
                    .ok_or_else(|| CodegenError("continue outside loop".into()))?;
                self.emit_ffi_cleanup(fcx, buffer_base, stack_base);
                let header_bb = fcx.loop_headers.last()
                    .ok_or_else(|| CodegenError("continue outside loop".into()))?;
                self.builder.build_unconditional_branch(*header_bb).unwrap();
                Ok(None)
            }
            CoreExprKind::Assign { local, value } => {
                let v = self.gen_expr(fcx, value)?;
                if let (Some(slot), Some(v)) = (fcx.slots[*local as usize], v) {
                    self.builder.build_store(slot, v).unwrap();
                    if let Some(root) = fcx.root_slots[*local as usize] { self.store_mirror(fcx, root, v); }
                }
                Ok(None)
            }
            CoreExprKind::Return(v) => {
                match v {
                    Some(e) => {
                        let mut rv = self.gen_expr(fcx, e)?;
                        if self.builder.get_insert_block().unwrap().get_terminator().is_some() { return Ok(None); }
                        let may_move = !fcx.pending_ffi_buffers.is_empty();
                        self.emit_ffi_cleanup(fcx, 0, 0);
                        let traced = matches!(e.repr, Repr::Ref(_)) || matches!(e.repr, Repr::Value(id) if value_has_ref(&self.prog.values, id));
                        if may_move && traced {
                            match e.kind.as_ref() {
                                CoreExprKind::Local(_) => rv = self.gen_expr(fcx, e)?,
                                CoreExprKind::ConstZero(_) => {},
                                _ => return Err(CodegenError("return cleanup requires a rooted ANF value".into())),
                            }
                        }
                        self.emit_unlink(fcx);
                        match rv {
                            Some(rv) => { self.builder.build_return(Some(&rv)).unwrap(); }
                            None => { self.builder.build_return(None).unwrap(); }
                        }
                    }
                    None => { self.emit_ffi_cleanup(fcx, 0, 0); self.emit_unlink(fcx); self.builder.build_return(None).unwrap(); }
                }
                Ok(None)
            }
            CoreExprKind::MakeValue { value, fields } => self.gen_make_value(fcx, *value, fields),
            CoreExprKind::MakeValueVariant { value, tag, fields } => {
                self.gen_make_value_variant(fcx, *value, *tag, fields)
            }
            CoreExprKind::ValueMatch { scrutinee, arms } => {
                self.gen_value_match(fcx, scrutinee, arms, &e.repr)}
            CoreExprKind::New { layout, fields } => {
                self.gen_alloc(fcx, *layout, None, fields, e.span)}
            CoreExprKind::MakeVariant { layout, tag, fields } => self.gen_alloc(fcx, *layout, Some(*tag), fields, e.span)
            ,
            CoreExprKind::Field { base, loc } => self.gen_field(fcx, base, loc),
            CoreExprKind::SetField { base, loc, value } => {
                let mut obj = self.gen_expr(fcx, base)?.unwrap().into_pointer_value();
                if matches!(loc, FieldLoc::Unit) {
                    self.gen_expr(fcx, value)?;
                    return Ok(None);
                }
                let aggregate = matches!(loc, FieldLoc::ValueAt { .. });
                let off = match loc {
                    FieldLoc::Ptr { idx } => Self::HEADER + (*idx as u64) * 8,
                    FieldLoc::Raw { offset, .. } | FieldLoc::ValueAt { offset, .. } => {
                        let lid = match &base.repr { Repr::Ref(l) => *l, _ => return Err(CodegenError("setfield on non-ref".into())) };
                        let lay = &self.prog.layouts[lid as usize];
                        Self::HEADER + (lay.ptr_fields as u64) * 8 + *offset as u64
                    }
                    FieldLoc::Unit => unreachable!("unit fields do not have an address"),
                    FieldLoc::ValueField { .. } => {
                        return Err(CodegenError("value-struct field is immutable".into()));
                    }
                };
                if let FieldLoc::ValueAt { value: vid, .. } = loc {
                    // The references a stored aggregate embeds: each may be an
                    // old→young edge, so each is barriered after the store.
                    let lid = match &base.repr { Repr::Ref(l) => *l, _ => return Err(CodegenError("setfield on non-ref".into())) };
                    let vsize = self.prog.values[*vid as usize].size as u64;
                    let interior: Vec<u64> = self.prog.layouts[lid as usize].interior_ptrs.iter()
                        .map(|&p| p as u64).filter(|&p| p >= off && p < off + vsize).collect();
                    let store_and_barrier = |this: &mut Self, fcx: &mut FnCtx<'ctx>, target: PointerValue<'ctx>| -> Result<(), CodegenError> {
                        let v = this.gen_expr(fcx, value)?.unwrap();
                        this.builder.build_store(this.obj_addr(target, off), v).unwrap();
                        let ptrty = this.ctx.ptr_type(AddressSpace::default());
                        for p in &interior {
                            let slot = this.obj_addr(target, *p);
                            let refv = this.builder.build_load(ptrty, slot, "iref").unwrap();
                            this.emit_write_barrier(fcx, target, slot, refv);
                        }
                        Ok(())
                    };
                    // Direct when nothing can race the store; otherwise the
                    // runtime's tear-free write, or a locked store if contended.
                    let direct = self.aggregate_direct(fcx);
                    let fast = self.ctx.append_basic_block(fcx.func, "aggregate.write.direct");
                    let slow = self.ctx.append_basic_block(fcx.func, "aggregate.write.runtime");
                    let done = self.ctx.append_basic_block(fcx.func, "aggregate.done");
                    self.builder.build_conditional_branch(direct, fast, slow).unwrap();
                    self.builder.position_at_end(fast);
                    store_and_barrier(self, fcx, obj)?;
                    self.builder.build_unconditional_branch(done).unwrap();
                    self.builder.position_at_end(slow);
                    // ANF value locals already have stable native storage, with
                    // reference interiors rooted in the caller's frame. The
                    // nonparking attempt only reads it and cannot relocate it.
                    let input = if let CoreExprKind::Local(local) = value.kind.as_ref() {
                        fcx.slots[*local as usize].ok_or_else(|| CodegenError("aggregate RHS has no native local storage".into()))?
                    } else {
                        let candidate = self.gen_expr(fcx, value)?.unwrap();
                        let input = self.entry_alloca(candidate.get_type(), "aggregate.input");
                        self.builder.build_store(input, candidate).unwrap();
                        input
                    };
                    let name = format!("__aggregate_refs_{vid}");
                    let mut references = Vec::new();
                    value_interior_offsets(&self.prog.values, *vid, 0, &mut references);
                    references.sort_unstable();
                    references.dedup();
                    let reference_count = references.len();
                    let offsets = if let Some(global) = self.module.get_global(&name) { global } else {
                        let ty = self.ctx.i16_type();
                        let values: Vec<_> = references.iter().map(|&at| ty.const_int(at as u64, false)).collect();
                        let initializer = ty.const_array(&values);
                        let global = self.module.add_global(initializer.get_type(), None, &name);
                        global.set_initializer(&initializer);
                        global.set_constant(true);
                        global.set_linkage(inkwell::module::Linkage::Private);
                        global
                    };
                    let write = self.module.get_function("ai_managed_try_write").unwrap();
                    let result = call_result(self.build_managed_call(fcx, write, &[
                        fcx.thread.into(), obj.into(), self.ctx.i64_type().const_int(off, false).into(), input.into(),
                        self.ctx.i64_type().const_int(vsize, false).into(),
                        offsets.as_pointer_value().into(), self.ctx.i64_type().const_int(reference_count as u64, false).into(),
                    ], "aggregate.stored").unwrap()).into_int_value();
                    let fallback = self.ctx.append_basic_block(fcx.func, "aggregate.contended");
                    let success = self.builder.build_int_compare(IntPredicate::NE, result, self.ctx.i8_type().const_zero(), "aggregate.success").unwrap();
                    self.builder.build_conditional_branch(success, done, fallback).unwrap();
                    self.builder.position_at_end(fallback);
                    // A contended acquisition can relocate both base and RHS.
                    obj = self.managed_lock(fcx, obj);
                    store_and_barrier(self, fcx, obj)?;
                    self.managed_unlock(fcx);
                    self.builder.build_unconditional_branch(done).unwrap();
                    self.builder.position_at_end(done);
                    return Ok(None);
                }
                let _ = aggregate;
                let v = self.gen_expr(fcx, value)?.unwrap();
                self.managed_store(fcx, obj, self.ctx.i64_type().const_int(off, false), v, &value.repr, matches!(loc, FieldLoc::Ptr { .. }), base)?;
                Ok(None)
            }
            CoreExprKind::Match { scrutinee, arms } => {
                self.gen_match(fcx, scrutinee, arms, &e.repr)}
            CoreExprKind::EnumTag(scrut) => {
                let value = self.gen_expr(fcx, scrut)?.unwrap();
                let tag = match &scrut.repr {
                    Repr::Value(id) => {
                        let variants = self.prog.values[*id as usize].variants.as_ref()
                            .ok_or_else(|| CodegenError("EnumTag on non-enum value".into()))?;
                        let tag_index = if crate::core::value_enum_max_ptrs(variants, &self.prog.values) == 0 { 0 } else { 1 };
                        self.builder.build_extract_value(value.into_struct_value(), tag_index, "vetag").unwrap()
                    }
                    Repr::Ref(lid) => {
                        let tag_off = self.enum_tag_offset(*lid);
                        self.builder.build_load(self.ctx.i32_type(), self.obj_addr(value.into_pointer_value(), tag_off), "etag").unwrap()
                    }
                    _ => return Err(CodegenError("EnumTag on non-enum".into())),
                };
                Ok(Some(tag))
            }
            CoreExprKind::EnumPayload { scrutinee, field, repr, payload_reprs } => {
                let value = self.gen_expr(fcx, scrutinee)?.unwrap();
                if matches!(repr, Repr::Unit) { return Ok(None); }
                let val = match &scrutinee.repr {
                    Repr::Value(id) => {
                        self.load_value_enum_payload(value, *id, *field as usize, payload_reprs)?}
                    Repr::Ref(lid) => {
                        let raw_base = self.enum_raw_base(*lid);
                        self.load_enum_payload(value.into_pointer_value(), raw_base, *field as usize, payload_reprs)?
                    }
                    _ => return Err(CodegenError("EnumPayload on non-enum".into())),
                };
                Ok(Some(val))
            }
            CoreExprKind::ArrayNew { layout, len, elem } => {
                self.gen_array_new(fcx, *layout, len, elem, e.span)}
            CoreExprKind::ArrayLen(arr) => self.gen_array_len(fcx, arr),
            CoreExprKind::ArrayGet { array, index, elem } => {
                self.gen_array_get(fcx, array, index, elem, &e.repr)}
            CoreExprKind::ArrayGetUnchecked { array, index, elem } => {
                self.gen_array_get_unchecked(fcx, array, index, elem)}
            CoreExprKind::ArrayGetChecked { array, index, elem } => {
                self.gen_array_get_checked(fcx, array, index, elem)}
            CoreExprKind::ArraySet { array, index, value, elem } => self.gen_array_set(fcx, array, index, value, elem),
            CoreExprKind::MakeClosure { code, env, captures } => self.gen_make_closure(fcx, *code, *env, captures, e.span),
            CoreExprKind::CallClosure { callee, args } => {
                self.gen_call_closure(fcx, callee, args, &e.repr)}
        }
    }

    /// Return the allocation-site id (Target-1b) for `(function, type_id)`,
    /// assigning a fresh id and recording the site on first sight. Dedups by
    /// pair so the v1 function+type granularity stays honest: every emitted id
    /// maps to exactly one labelable `(function, type)`, never a distinct id we
    /// couldn't tell apart at profile time.
    fn alloc_site_id(&mut self, function: &str, type_id: u16, span: crate::core::SpanId) -> u32 {
        // Resolve the construction node's span to `file:line:col` if source is
        // available (debugger P1 span-threading); empty otherwise. The location
        // is part of the dedup key, so two allocations of the same type in the
        // same function at DIFFERENT lines become DISTINCT sites — the precise
        // upgrade over the old (function, type)-only key.
        let location = self.prog.span_label(span).unwrap_or_default();
        let key = (function.to_string(), type_id, location.clone());
        if let Some(&id) = self.alloc_site_ids.get(&key) {
            return id;
        }
        let id = self.alloc_sites.len() as u32;
        self.alloc_sites.push(crate::gc::AllocSite {
            function: function.to_string(),
            type_id,
            location,
        });
        self.alloc_site_ids.insert(key, id);
        id
    }

    /// Keep source allocation labels when a branch has a synthesized helper.
    fn current_fn_name(&self, fcx: &FnCtx<'ctx>) -> String {
        self.funcs
            .iter()
            .find_map(|(id, function)| {
                if *function == fcx.func {
                    self.outlined_helpers.get(id).cloned()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| fcx.func.get_name().to_string_lossy().into_owned())
    }

    fn declare_runtime_externs(&self) {
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let i32t = self.ctx.i32_type();
        let i64t = self.ctx.i64_type();
        self.module.add_function(
            "ai_managed_lock", ptr.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_managed_unlock", self.ctx.void_type().fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_managed_read", self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), i64t.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_managed_try_write", self.ctx.i8_type().fn_type(&[ptr.into(), ptr.into(), i64t.into(), ptr.into(), i64t.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_gc_alloc_fixed(ptr thread, i32 type_id, i32 site_id)
        self.module.add_function(
            "ai_gc_alloc_fixed",
            ptr.fn_type(&[ptr.into(), i32t.into(), i32t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_gc_record_alloc",
            self.ctx.void_type().fn_type(&[ptr.into(), i32t.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_gc_alloc_varlen(ptr thread, i32 type_id, i64 n, i32 site_id)
        self.module.add_function(
            "ai_gc_alloc_varlen",
            ptr.fn_type(&[ptr.into(), i32t.into(), i64t.into(), i32t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // void ai_gc_write_barrier(ptr thread, ptr obj, ptr new_val)
        self.module.add_function(
            "ai_gc_write_barrier",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_atom_ref_load", ptr.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_atom_ref_cas",
            self.ctx.i8_type().fn_type(&[ptr.into(), ptr.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        for width in [8, 16, 32, 64] {
            let bits = self.ctx.custom_width_int_type(width);
            self.module.add_function(
                &format!("ai_atom_bits{width}_load"), bits.fn_type(&[ptr.into(), ptr.into()], false),
                Some(inkwell::module::Linkage::External),
            );
            self.module.add_function(
                &format!("ai_atom_bits{width}_cas"),
                self.ctx.i8_type().fn_type(&[ptr.into(), ptr.into(), bits.into(), bits.into()], false),
                Some(inkwell::module::Linkage::External),
            );
        }
        for width in [8, 16, 32, 64] {
            let bits = self.ctx.custom_width_int_type(width);
            self.module.add_function(
                &format!("ai_slot_bits{width}_load"), bits.fn_type(&[ptr.into(), ptr.into(), i64t.into()], false),
                Some(inkwell::module::Linkage::External),
            );
            self.module.add_function(
                &format!("ai_slot_bits{width}_store"), self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), i64t.into(), bits.into()], false),
                Some(inkwell::module::Linkage::External),
            );
        }
        self.module.add_function("ai_slot_ref_store",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), i64t.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External));
        // void ai_gc_pollcheck_slow(ptr thread)
        self.module.add_function(
            "ai_gc_pollcheck_slow",
            self.ctx.void_type().fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        {
            let f = self.module.add_function("ai_uninitialized_array_fail",
                self.ctx.void_type().fn_type(&[ptr.into(), i64t.into()], false),
                Some(inkwell::module::Linkage::External));
            for name in ["noreturn", "cold"] {
                let kind = inkwell::attributes::Attribute::get_named_enum_kind_id(name);
                f.add_attribute(inkwell::attributes::AttributeLoc::Function, self.ctx.create_enum_attribute(kind, 0));
            }
        }
        // void ai_bounds_fail(ptr thread, i64 index, i64 len) -- aborts, never
        // returns. Marked noreturn + cold so the inlined array bounds check keeps
        // the in-bounds path straight-line and the trap path out of line.
        {
            self.module.add_function(
                "ai_array_allocation_len",
                i64t.fn_type(&[ptr.into(), i64t.into(), i64t.into(), i32t.into()], false),
                Some(inkwell::module::Linkage::External),
            );
            let f = self.module.add_function(
                "ai_bounds_fail",
                self.ctx.void_type().fn_type(&[ptr.into(), i64t.into(), i64t.into()], false),
                Some(inkwell::module::Linkage::External),
            );
            use inkwell::attributes::AttributeLoc;
            for name in ["noreturn", "cold"] {
                let kind = inkwell::attributes::Attribute::get_named_enum_kind_id(name);
                f.add_attribute(AttributeLoc::Function, self.ctx.create_enum_attribute(kind, 0));
            }
        }
        {
            let f = self.module.add_function(
                "ai_arithmetic_fail",
                self.ctx.void_type().fn_type(&[ptr.into(), i32t.into()], false),
                Some(inkwell::module::Linkage::External),
            );
            use inkwell::attributes::AttributeLoc;
            for name in ["noreturn", "cold"] {
                let kind = inkwell::attributes::Attribute::get_named_enum_kind_id(name);
                f.add_attribute(AttributeLoc::Function, self.ctx.create_enum_attribute(kind, 0));
            }
        }
        // void ai_ffi_enter(ptr thread) / void ai_ffi_leave(ptr thread)
        // The managed↔native transition wrapped around every extern "C" call.
        self.module.add_function(
            "ai_ffi_enter",
            self.ctx.void_type().fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_ffi_leave",
            self.ctx.void_type().fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // Callback support: ptr ai_current_thread(void); reenter/exit transitions.
        self.module.add_function(
            "ai_current_thread",
            ptr.fn_type(&[], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_ffi_reenter",
            self.ctx.void_type().fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_ffi_exit",
            self.ctx.void_type().fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_print_int(ptr thread, i64 v)
        self.module.add_function(
            "ai_print_int",
            i64t.fn_type(&[ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_print_float(ptr thread, f64 v)
        self.module.add_function(
            "ai_print_float",
            i64t.fn_type(&[ptr.into(), self.ctx.f64_type().into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // String primitives (see crates/gcrust-rt/src/runtime.rs).
        // i64 ai_print_str(ptr thread, ptr s)
        self.module.add_function(
            "ai_print_str",
            i64t.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_print_str_raw(ptr thread, ptr s) — no trailing newline
        self.module.add_function(
            "ai_print_str_raw",
            i64t.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_str_len(ptr thread, ptr s)
        self.module.add_function(
            "ai_str_len",
            i64t.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_str_eq(ptr thread, ptr a, ptr b)
        self.module.add_function(
            "ai_str_eq",
            i64t.fn_type(&[ptr.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_str_eq_v2",
            i64t.fn_type(&[ptr.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_str_concat(ptr thread, i32 type_id, ptr a, ptr b)
        self.module.add_function(
            "ai_str_concat",
            ptr.fn_type(&[ptr.into(), i32t.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_str_get(ptr thread, ptr s, i64 i)
        self.module.add_function(
            "ai_str_get",
            i64t.fn_type(&[ptr.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_str_substring(ptr thread, i32 type_id, ptr s, i64 start, i64 end)
        self.module.add_function(
            "ai_str_substring",
            ptr.fn_type(&[ptr.into(), i32t.into(), ptr.into(), i64t.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_host_call",
            ptr.fn_type(&[ptr.into(), i32t.into(), i32t.into(), i32t.into(), i32t.into(), ptr.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function("ai_str_join", ptr.fn_type(&[ptr.into(), i32t.into(), ptr.into(), i64t.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        let panic = self.module.add_function("ai_panic", self.ctx.void_type().fn_type(&[ptr.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        panic.add_attribute(inkwell::attributes::AttributeLoc::Function, self.ctx.create_enum_attribute(inkwell::attributes::Attribute::get_named_enum_kind_id("noreturn"), 0));
        // ptr ai_read_file(ptr thread, i32 type_id, ptr path)
        self.module.add_function(
            "ai_read_file",
            ptr.fn_type(&[ptr.into(), i32t.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_str_from_int(ptr thread, i32 type_id, i64 v)
        self.module.add_function(
            "ai_str_from_int",
            ptr.fn_type(&[ptr.into(), i32t.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_char_to_str(ptr thread, i32 type_id, i64 cp)
        self.module.add_function(
            "ai_char_to_str",
            ptr.fn_type(&[ptr.into(), i32t.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_type_name(ptr thread, i32 str_type_id, ptr obj)
        self.module.add_function(
            "ai_type_name",
            ptr.fn_type(&[ptr.into(), i32t.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // void ai_heap_snapshot(ptr thread) — heap-explorer P2 on-demand snapshot.
        self.module.add_function(
            "ai_heap_snapshot",
            self.ctx.void_type().fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // void ai_thread_mark_daemon(ptr thread, ptr handle) — `std::thread::spawn`.
        self.module.add_function(
            "ai_thread_mark_daemon",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_monotonic_nanos(ptr thread) — `Instant::now()`.
        self.module.add_function(
            "ai_monotonic_nanos",
            i64t.fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // In-language field reflection (all thread-first; RuntimeCall-dispatched).
        // i64 ai_reflect_field_count(ptr thread, ptr obj)
        self.module.add_function(
            "ai_reflect_field_count",
            i64t.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_reflect_field_kind(ptr thread, ptr obj, i64 i)
        self.module.add_function(
            "ai_reflect_field_kind",
            i64t.fn_type(&[ptr.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_reflect_field_i64(ptr thread, ptr obj, i64 i)
        self.module.add_function(
            "ai_reflect_field_i64",
            i64t.fn_type(&[ptr.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_reflect_field_name(ptr thread, i64 str_type_id, ptr obj, i64 i)
        self.module.add_function(
            "ai_reflect_field_name",
            ptr.fn_type(&[ptr.into(), i64t.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // void ai_array_copy(ptr thread, ptr src, i64 src_pos, ptr dst, i64 dst_pos, i64 n, i64 stride)
        self.module.add_function(
            "ai_array_copy",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), i64t.into(), ptr.into(), i64t.into(), i64t.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // void ai_str_copy_bytes(ptr thread, ptr s, ptr array)
        self.module.add_function(
            "ai_str_copy_bytes",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_str_eq_bytes(ptr thread, ptr s, ptr bytes, i64 len)
        self.module.add_function(
            "ai_str_eq_bytes",
            i64t.fn_type(&[ptr.into(), ptr.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_str_from_utf8_prefix(ptr thread, i64 str_type_id, ptr array, i64 len)
        self.module.add_function(
            "ai_str_from_utf8_prefix",
            ptr.fn_type(&[ptr.into(), i64t.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_str_from_bytes(ptr thread, i64 str_type_id, ptr array, i64 len)
        self.module.add_function(
            "ai_str_from_bytes",
            ptr.fn_type(&[ptr.into(), i64t.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_fmt_f64(ptr thread, i64 str_type_id, f64 v, i64 prec, i64 mode)
        self.module.add_function(
            "ai_fmt_f64",
            ptr.fn_type(&[ptr.into(), i64t.into(), self.ctx.f64_type().into(), i64t.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // ptr ai_str_from_float(ptr thread, i32 type_id, f64 v)
        self.module.add_function(
            "ai_str_from_float",
            ptr.fn_type(&[ptr.into(), i32t.into(), self.ctx.f64_type().into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // f64 ai_str_to_float(ptr thread, ptr s)
        self.module.add_function(
            "ai_str_to_float",
            self.ctx.f64_type().fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_str_to_float_v2",
            self.ctx.f64_type().fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_str_hash(ptr thread, ptr s)
        self.module.add_function(
            "ai_str_hash",
            i64t.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_str_hash_v2",
            i64t.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function("ai_ffi_buffer_alloc_v1", ptr.fn_type(&[i64t.into()], false), Some(inkwell::module::Linkage::External));
        self.module.add_function("ai_ffi_buffer_free_v1", self.ctx.void_type().fn_type(&[ptr.into(), i64t.into()], false), Some(inkwell::module::Linkage::External));
        self.module.add_function("ai_ffi_buffer_alloc_v2", ptr.fn_type(&[ptr.into(), i64t.into()], false), Some(inkwell::module::Linkage::External));
        self.module.add_function("ai_ffi_buffer_data_v2", ptr.fn_type(&[ptr.into()], false), Some(inkwell::module::Linkage::External));
        self.module.add_function("ai_ffi_buffer_free_v2", self.ctx.void_type().fn_type(&[ptr.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        // i64 ai_str_copy_to_buf(ptr thread, ptr s, ptr dst)
        self.module.add_function(
            "ai_str_copy_to_buf",
            i64t.fn_type(&[ptr.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // void ai_buf_copy_in(ptr thread, ptr obj, ptr dst, i64 byte_len, i64 bits)
        self.module.add_function(
            "ai_buf_copy_in",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), ptr.into(), i64t.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // void ai_buf_copy_out(ptr thread, ptr obj, ptr src, i64 byte_len, i64 bits)
        self.module.add_function(
            "ai_buf_copy_out",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), ptr.into(), i64t.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        for legacy in ["ai_str_copy_to_buf", "ai_buf_copy_in", "ai_buf_copy_out"] {
            let ty = self.module.get_function(legacy).unwrap().get_type();
            self.module.add_function(&format!("{legacy}_v2"), ty, Some(inkwell::module::Linkage::External));
        }
        // Threads. ptr ai_thread_spawn(ptr parent, ptr env, ptr code)
        self.module.add_function(
            "ai_thread_spawn",
            ptr.fn_type(&[ptr.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_thread_bind_owner",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_atomic_i64_bind_owner",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_thread_join(ptr thread, ptr handle)
        self.module.add_function(
            "ai_thread_join",
            i64t.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // void ai_thread_sleep(ptr thread, i64 ms) / void ai_thread_yield(ptr thread)
        self.module.add_function(
            "ai_thread_sleep",
            self.ctx.void_type().fn_type(&[ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_thread_yield",
            self.ctx.void_type().fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // void ai_statics_install(ptr thread, ptr table); ptr ai_statics_table(ptr thread)
        self.module.add_function(
            "ai_statics_install",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function(
            "ai_statics_table",
            ptr.fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_thread_current_id(ptr thread)
        self.module.add_function(
            "ai_thread_current_id",
            i64t.fn_type(&[ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        self.module.add_function("ai_object_type_id",
            self.ctx.i16_type().fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External));
        // ptr ai_closure_code_ptr(ptr thread, ptr env) — recover a closure's code
        // pointer from its env header type_id (correct for any capture count).
        self.module.add_function(
            "ai_closure_code_ptr",
            ptr.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        for name in ["ai_chan_waiting_senders", "ai_chan_waiting_receivers"] {
            self.module.add_function(name, i64t.fn_type(&[ptr.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        }
        self.module.add_function("ai_chan_bind_owner",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        // Channels.
        self.module.add_function("ai_chan_new",
            ptr.fn_type(&[ptr.into(), i64t.into(), ptr.into(), i32t.into()], false), Some(inkwell::module::Linkage::External));
        self.module.add_function("ai_chan_send",
            i64t.fn_type(&[ptr.into(), ptr.into(), ptr.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        self.module.add_function("ai_chan_recv",
            ptr.fn_type(&[ptr.into(), ptr.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        self.module.add_function("ai_chan_close",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        self.module.add_function("ai_chan_sender_clone",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        self.module.add_function("ai_chan_sender_drop",
            self.ctx.void_type().fn_type(&[ptr.into(), ptr.into()], false), Some(inkwell::module::Linkage::External));
        // AtomicI64. ptr ai_atomic_i64_new(ptr thread, i64 v)
        self.module.add_function(
            "ai_atomic_i64_new",
            ptr.fn_type(&[ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_atomic_i64_load(ptr thread, ptr a)
        self.module.add_function(
            "ai_atomic_i64_load",
            i64t.fn_type(&[ptr.into(), ptr.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_atomic_i64_store(ptr thread, ptr a, i64 v)
        self.module.add_function(
            "ai_atomic_i64_store",
            i64t.fn_type(&[ptr.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_atomic_i64_fetch_add(ptr thread, ptr a, i64 delta)
        self.module.add_function(
            "ai_atomic_i64_fetch_add",
            i64t.fn_type(&[ptr.into(), ptr.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
        // i64 ai_atomic_i64_compare_and_set(ptr thread, ptr a, i64 expected, i64 new)
        self.module.add_function(
            "ai_atomic_i64_compare_and_set",
            i64t.fn_type(&[ptr.into(), ptr.into(), i64t.into(), i64t.into()], false),
            Some(inkwell::module::Linkage::External),
        );
    }

    /// The byte offset of a heap object's data start (the `Full` header size).
    const HEADER: u64 = 16;

    /// Whether a value of this repr can be moved by the GC — a heap reference, or
    /// a flattened `#[value]` aggregate holding one. Such a value cached in a
    /// register before a safepoint goes stale and must be reloaded after.
    fn repr_relocates(&self, r: &Repr) -> bool {
        match r {
            Repr::Ref(_) => true,
            Repr::Value(v) => value_has_ref(&self.prog.values, *v),
            _ => false,
        }
    }

    /// Reserve a fixed object in the owning mutator's TLAB. Epoch validation
    /// rejects buffers invalidated by nursery reset. Stress always takes the
    /// collecting runtime path. The prefix is published before any safepoint;
    /// construction stores below initialize fields before the object escapes.
    fn gen_fixed_allocation(&mut self, fcx: &mut FnCtx<'ctx>, layout: LayoutId, site: u32, init: FixedInit,
        fields: &[CoreExpr], vals: &[(Option<BasicValueEnum<'ctx>>, Repr)]) -> Result<PointerValue<'ctx>, CodegenError> {
        use crate::gc::{Full, ObjHeader, TypeInfo};
        let header_tag = match init {
            FixedInit::Fields { logical, tag } => tag.filter(|_| self.enum_tag_offset(logical) == Full::ENUM_TAG_OFFSET as u64),
            FixedInit::ClosureEnv { .. } => None,
        };
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let i64t = self.ctx.i64_type();
        let i32t = self.ctx.i32_type();
        let lay = &self.physical_layouts.layouts[layout as usize];
        let size = TypeInfo::for_header(Full::SIZE).with_fields(lay.ptr_fields).with_raw_bytes(lay.raw_bytes)
            .checked_allocation_size(0).ok_or_else(|| CodegenError("invalid fixed allocation layout".into()))? as u64;
        let alloc = self.module.get_function("ai_gc_alloc_fixed").unwrap();
        let args = [fcx.thread.into(), i32t.const_int(layout as u64, false).into(), i32t.const_int(site as u64, false).into()];
        if size > 8192 || !matches!(lay.varlen, crate::core::VarLen::None) {
            let object = call_result(self.build_managed_call(fcx, alloc, &args, "obj").unwrap()).into_pointer_value();
            let restored = self.reload_allocation_values(fcx, fields, vals)?;
            self.init_fixed_object(object, layout, init, &restored, init.nonzero_tag());
            return Ok(object);
        }
        let tlab = self.tlab_reserve(fcx, i64t.const_int(size, false), self.ctx.bool_type().const_int(1, false), site);
        let (object, slow, merge) = (tlab.object, tlab.slow, tlab.merge);
        // Initialize the Full header once, including a known enum tag. A
        // pair of numeric words preserves the low-16-bit ID and forwarding
        // marker invariant on both byte orders, matching runtime Full::new.
        let header_type = self.ctx.struct_type(&[i64t.into(), i64t.into()], false);
        let header = header_type.const_named_struct(&[
            i64t.const_int(header_tag.unwrap_or(0) as u64, false).into(),
            i64t.const_int(layout as u64, false).into(),
        ]);
        self.builder.build_store(object, header).unwrap().set_alignment(8).unwrap();
        let mut initialized = Full::SIZE as u64;
        let ranges = match init {
            FixedInit::Fields { logical, tag } => self.constructor_ranges(logical, tag, vals),
            // A closure env is zeroed whole, then its code pointer and
            // captures are stored.
            FixedInit::ClosureEnv { .. } => Vec::new(),
        };
        for (start, stop) in ranges {
            if start > initialized {
                self.builder.build_memset(self.obj_addr(object, initialized), 1,
                    self.ctx.i8_type().const_zero(), i64t.const_int(start - initialized, false)).unwrap();
            }
            initialized = initialized.max(stop);
        }
        if initialized < size {
            self.builder.build_memset(self.obj_addr(object, initialized), 1,
                self.ctx.i8_type().const_zero(), i64t.const_int(size - initialized, false)).unwrap();
        }
        // Complete constructor stores before publishing the initialized extent.
        // This path cannot collect; pre-allocation reference values remain valid.
        self.init_fixed_object(object, layout, init, vals, header_tag.is_none());
        self.tlab_publish(&tlab, site);
        let count_done = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(merge).unwrap();
        self.builder.position_at_end(slow);
        let allocated = call_result(self.build_managed_call(fcx, alloc, &args, "obj.slow").unwrap()).into_pointer_value();
        let restored = self.reload_allocation_values(fcx, fields, vals)?;
        self.init_fixed_object(allocated, layout, init, &restored, init.nonzero_tag());
        let slow_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(merge).unwrap();
        self.builder.position_at_end(merge);
        let object_phi = self.builder.build_phi(ptr, "obj").unwrap();
        object_phi.add_incoming(&[(&object, count_done), (&allocated, slow_end)]);
        Ok(object_phi.as_basic_value().into_pointer_value())
    }

    /// Try to reserve `size` bytes (at most 8192) in the owning mutator's
    /// TLAB when `eligible`. Leaves the builder in the fast block, where
    /// `object` is the reserved storage; `slow` must call the runtime
    /// allocator and both must branch to `merge`. Stress always takes the
    /// collecting runtime path.
    fn tlab_reserve(&mut self, fcx: &FnCtx<'ctx>, size: IntValue<'ctx>, eligible: IntValue<'ctx>, site: u32) -> TlabReservation<'ctx> {
        use crate::gc::inline_tlab_offsets as off;
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let i64t = self.ctx.i64_type();
        let window_addr = self.obj_addr(fcx.thread, crate::runtime::thread_offsets::TLAB_WINDOW as u64);
        let window = self.builder.build_load(ptr, window_addr, "tlab.window").unwrap().into_pointer_value();
        let cursor_addr = self.obj_addr(window, off::CURSOR as u64);
        let cursor = self.builder.build_load(i64t, cursor_addr, "tlab.cursor").unwrap().into_int_value();
        let limit_addr = self.obj_addr(window, off::LIMIT as u64);
        let limit = self.builder.build_load(i64t, limit_addr, "tlab.limit").unwrap().into_int_value();
        // Native reservations fit a Rust allocation (<= isize::MAX), and size
        // is <= 8192, so adding size to the cached offset cannot wrap usize.
        let end = self.builder.build_int_add(cursor, size, "tlab.end").unwrap();
        let within = self.builder.build_int_compare(IntPredicate::ULE, end, limit, "tlab.within").unwrap();
        let capacity = self.builder.build_and(within, eligible, "tlab.capacity").unwrap();
        let count_len_addr = self.obj_addr(window, off::COUNTERS_LEN as u64);
        let count_len = self.builder.build_load(i64t, count_len_addr, "alloc.counter_len").unwrap().into_int_value();
        let known_site = self.builder.build_int_compare(IntPredicate::ULT,
            i64t.const_int(site as u64, false), count_len, "alloc.known_site").unwrap();
        let available = self.builder.build_and(capacity, known_site, "tlab.available").unwrap();
        let check = self.ctx.append_basic_block(fcx.func, "alloc.check");
        let fast = self.ctx.append_basic_block(fcx.func, "alloc.fast");
        let slow = self.ctx.append_basic_block(fcx.func, "alloc.slow");
        let merge = self.ctx.append_basic_block(fcx.func, "alloc.merge");
        self.builder.build_conditional_branch(available, check, slow).unwrap();
        self.builder.position_at_end(check);
        let stress_addr = self.obj_addr(window, off::STRESS as u64);
        let stress_ptr = self.builder.build_load(ptr, stress_addr, "tlab.stress_ptr").unwrap().into_pointer_value();
        let stress = self.builder.build_load(self.ctx.i8_type(), stress_ptr, "tlab.stress").unwrap();
        stress.as_instruction_value().unwrap().set_alignment(1).unwrap();
        stress.as_instruction_value().unwrap().set_atomic_ordering(AtomicOrdering::Acquire).unwrap();
        let normal = self.builder.build_int_compare(IntPredicate::EQ, stress.into_int_value(), self.ctx.i8_type().const_zero(), "tlab.normal").unwrap();
        self.builder.build_conditional_branch(normal, fast, slow).unwrap();
        self.builder.position_at_end(fast);
        let base_addr = self.obj_addr(window, off::BASE as u64);
        let base = self.builder.build_load(ptr, base_addr, "tlab.base").unwrap().into_pointer_value();
        let object = unsafe { self.builder.build_in_bounds_gep(self.ctx.i8_type(), base, &[cursor], "tlab.object").unwrap() };
        TlabReservation { window, cursor_addr, end, size, object, slow, merge }
    }

    /// Publish a TLAB reservation whose object is fully initialized, and
    /// count it at its allocation site.
    fn tlab_publish(&mut self, tlab: &TlabReservation<'ctx>, site: u32) {
        use crate::gc::inline_tlab_offsets as off;
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let i64t = self.ctx.i64_type();
        self.builder.build_store(tlab.cursor_addr, tlab.end).unwrap();
        let initialized_addr = self.obj_addr(tlab.window, off::INITIALIZED as u64);
        let initialized = self.builder.build_load(ptr, initialized_addr, "tlab.initialized").unwrap().into_pointer_value();
        let publication = self.builder.build_store(initialized, tlab.end).unwrap();
        publication.set_alignment(8).unwrap();
        publication.set_atomic_ordering(AtomicOrdering::Monotonic).unwrap();
        // First use goes through the native allocator, which grows the
        // counter vector before returning. The initialized fast path contains
        // no native calls after constructing its object.
        let counters_addr = self.obj_addr(tlab.window, off::COUNTERS as u64);
        let counters = self.builder.build_load(ptr, counters_addr, "alloc.counters").unwrap().into_pointer_value();
        let count_addr = self.obj_addr(counters, site as u64 * 16);
        let bytes_addr = self.obj_addr(counters, site as u64 * 16 + 8);
        for (addr, increment) in [(count_addr, i64t.const_int(1, false)), (bytes_addr, tlab.size)] {
            let old = self.builder.build_load(i64t, addr, "alloc.counter").unwrap().into_int_value();
            let next = self.builder.build_int_add(old, increment, "alloc.counter_next").unwrap();
            // Match the native allocator and profile merges: overflow must
            // retain a lower bound rather than silently reset a busy site.
            let headroom = self.builder.build_int_sub(i64t.const_all_ones(), increment, "alloc.counter_headroom").unwrap();
            let over = self.builder.build_int_compare(IntPredicate::UGT, old, headroom, "alloc.counter_overflow").unwrap();
            let saturated = self.builder.build_select(over, i64t.const_all_ones(), next,
                "alloc.counter_saturated").unwrap();
            self.builder.build_store(addr, saturated).unwrap();
        }
    }

    /// `str_from_utf8_prefix(array, len)` (see the runtime function). A short
    /// all-ASCII prefix, read while no copy is in progress, becomes a String
    /// in the thread-local buffer inline; anything else calls the runtime.
    fn gen_str_from_utf8_prefix(&mut self, fcx: &mut FnCtx<'ctx>, args: &[CoreExpr], span: crate::core::SpanId) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        const MAX_INLINE: u64 = 64;
        const DATA: u64 = 24;
        let i64t = self.ctx.i64_type();
        let i8t = self.ctx.i8_type();
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let CoreExprKind::ConstInt(layout, _) = args[0].kind.as_ref() else {
            return Err(CodegenError("str_from_utf8_prefix needs the String layout".into()));
        };
        let layout = *layout as u64;
        let tid = self.gen_expr(fcx, &args[0])?.unwrap();
        let array = self.gen_expr(fcx, &args[1])?.unwrap().into_pointer_value();
        let len = self.gen_expr(fcx, &args[2])?.unwrap().into_int_value();
        let site = self.alloc_site_id(&self.current_fn_name(fcx), layout as u16, span);
        // Only while no copy is in progress may the array be read directly.
        let mode = self.access_mode(fcx);
        let copying = self.builder.build_and(mode, i8t.const_int(1, false), "str.copying").unwrap();
        let idle = self.builder.build_int_compare(IntPredicate::EQ, copying, i8t.const_zero(), "str.idle").unwrap();
        let short = self.builder.build_int_compare(IntPredicate::ULE, len, i64t.const_int(MAX_INLINE, false), "str.short").unwrap();
        let eligible = self.builder.build_and(idle, short, "str.inline").unwrap();
        let unrounded = self.builder.build_int_add(len, i64t.const_int(DATA + 7, false), "str.size.raw").unwrap();
        let size = self.builder.build_and(unrounded, i64t.const_int(!7u64, false), "str.size").unwrap();
        let tlab = self.tlab_reserve(fcx, size, eligible, site);
        // Header, count, a zeroed last word (so whole words can be read),
        // then the bytes.
        let header_type = self.ctx.struct_type(&[i64t.into(), i64t.into(), i64t.into()], false);
        let header = header_type.get_undef();
        let header = self.builder.build_insert_value(header, i64t.const_zero(), 0, "str.header").unwrap();
        let header = self.builder.build_insert_value(header, i64t.const_int(layout, false), 1, "str.header").unwrap();
        let header = self.builder.build_insert_value(header, len, 2, "str.header").unwrap();
        self.builder.build_store(tlab.object, header).unwrap().set_alignment(8).unwrap();
        let last_word = self.builder.build_int_sub(size, i64t.const_int(8, false), "str.last").unwrap();
        let last = unsafe { self.builder.build_gep(i8t, tlab.object, &[last_word], "str.last.word").unwrap() };
        self.builder.build_store(last, i64t.const_zero()).unwrap().set_alignment(8).unwrap();
        let data = self.obj_addr(tlab.object, DATA);
        let source = self.obj_addr(array, DATA);
        self.builder.build_memcpy(data, 8, source, 8, len).unwrap();
        // All ASCII? Or together the words.
        let check = self.ctx.append_basic_block(fcx.func, "str.ascii");
        let checked = self.ctx.append_basic_block(fcx.func, "str.checked");
        let entry = self.builder.get_insert_block().unwrap();
        let any = self.builder.build_int_compare(IntPredicate::NE, len, i64t.const_zero(), "str.nonempty").unwrap();
        self.builder.build_conditional_branch(any, check, checked).unwrap();
        self.builder.position_at_end(check);
        let offset = self.builder.build_phi(i64t, "str.offset").unwrap();
        let acc = self.builder.build_phi(i64t, "str.bits").unwrap();
        let word = self.builder.build_load(i64t, unsafe { self.builder.build_gep(i8t, data, &[offset.as_basic_value().into_int_value()], "str.word.at").unwrap() }, "str.word").unwrap();
        word.as_instruction_value().unwrap().set_alignment(8).unwrap();
        let bits = self.builder.build_or(acc.as_basic_value().into_int_value(), word.into_int_value(), "str.bits.next").unwrap();
        let next = self.builder.build_int_add(offset.as_basic_value().into_int_value(), i64t.const_int(8, false), "str.offset.next").unwrap();
        let more = self.builder.build_int_compare(IntPredicate::ULT, next, len, "str.more").unwrap();
        offset.add_incoming(&[(&i64t.const_zero(), entry), (&next, check)]);
        acc.add_incoming(&[(&i64t.const_zero(), entry), (&bits, check)]);
        self.builder.build_conditional_branch(more, check, checked).unwrap();
        self.builder.position_at_end(checked);
        let all_bits = self.builder.build_phi(i64t, "str.bits.all").unwrap();
        all_bits.add_incoming(&[(&i64t.const_zero(), entry), (&bits, check)]);
        let high = self.builder.build_and(all_bits.as_basic_value().into_int_value(), i64t.const_int(0x8080_8080_8080_8080, false), "str.high").unwrap();
        let ascii = self.builder.build_int_compare(IntPredicate::EQ, high, i64t.const_zero(), "str.ascii.all").unwrap();
        let publish = self.ctx.append_basic_block(fcx.func, "str.publish");
        // Not ASCII: leave the space unreserved and ask the runtime.
        self.builder.build_conditional_branch(ascii, publish, tlab.slow).unwrap();
        self.builder.position_at_end(publish);
        self.tlab_publish(&tlab, site);
        let fast_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(tlab.merge).unwrap();
        self.builder.position_at_end(tlab.slow);
        let f = self.module.get_function("ai_str_from_utf8_prefix").unwrap();
        let slow = call_result(self.build_managed_call(fcx, f, &[fcx.thread.into(), tid.into(), array.into(), len.into()], "str.slow").unwrap()).into_pointer_value();
        let slow_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(tlab.merge).unwrap();
        self.builder.position_at_end(tlab.merge);
        let result = self.builder.build_phi(ptr, "str.prefix").unwrap();
        result.add_incoming(&[(&tlab.object, fast_end), (&slow, slow_end)]);
        Ok(Some(result.as_basic_value()))
    }

    fn enum_tag_offset(&self, layout: LayoutId) -> u64 {
        match &self.prog.layouts[layout as usize].meta.kind {
            crate::gc::TypeKind::Enum { tag_offset, .. } => *tag_offset as u64,
            // Hand-written Core IR may omit reflection metadata and retains
            // the historical payload tag representation.
            _ => Self::HEADER + self.prog.layouts[layout as usize].ptr_fields as u64 * 8,
        }
    }

    fn enum_raw_base(&self, layout: LayoutId) -> u64 {
        Self::HEADER + self.prog.layouts[layout as usize].ptr_fields as u64 * 8
            + if self.enum_tag_offset(layout) >= Self::HEADER { 8 } else { 0 }
    }

    /// Allocate a heap object and initialize its tag and payload, with pointer
    /// fields in leading slots and scalar/value fields in the raw region.
    /// Store a new fixed object's contents. `store_tag`: whether a `Fields`
    /// enum variant's tag still has to be written (the fast path writes it in
    /// the header when the tag lives there).
    fn init_fixed_object(&mut self, obj: PointerValue<'ctx>, layout: LayoutId, init: FixedInit,
        vals: &[(Option<BasicValueEnum<'ctx>>, Repr)], store_tag: bool) {
        match init {
            FixedInit::Fields { logical, tag } => self.store_allocation_fields(obj, logical, tag, vals, store_tag),
            FixedInit::ClosureEnv { code } => self.store_closure_env(obj, layout, code, vals),
        }
    }

    /// Store a closure env: the code pointer at raw offset 0, pointer captures
    /// in pointer slots, and scalar/value captures after the code pointer
    /// (mirrors `lower_lifted`'s offsets).
    fn store_closure_env(&mut self, obj: PointerValue<'ctx>, env: LayoutId, code: FuncId,
        vals: &[(Option<BasicValueEnum<'ctx>>, Repr)]) {
        let lay = &self.prog.layouts[env as usize];
        let raw_base = Self::HEADER + lay.ptr_fields as u64 * 8;
        let code_ptr = self.funcs[&code].as_global_value().as_pointer_value();
        let addr = self.obj_addr(obj, raw_base);
        self.builder.build_store(addr, code_ptr).unwrap();
        let mut ptr_slot = 0u64;
        let mut raw_off = 8u64;
        for (v, repr) in vals {
            match repr {
                Repr::Ref(_) => {
                    let a = self.obj_addr(obj, Self::HEADER + ptr_slot * 8);
                    ptr_slot += 1;
                    if let Some(v) = v { self.builder.build_store(a, *v).unwrap(); }
                }
                Repr::Scalar(s) => {
                    let sz = (s.bits().max(8) / 8) as u64;
                    raw_off = align_up64(raw_off, sz);
                    let a = self.obj_addr(obj, raw_base + raw_off);
                    raw_off += sz;
                    if let Some(v) = v { self.builder.build_store(a, *v).unwrap(); }
                }
                // Inline value-aggregate capture: its bytes at an 8-aligned
                // offset in the raw section.
                Repr::Value(vid) => {
                    let sz = self.prog.values[*vid as usize].size as u64;
                    raw_off = align_up64(raw_off, 8);
                    let a = self.obj_addr(obj, raw_base + raw_off);
                    raw_off += sz;
                    if let Some(v) = v { self.builder.build_store(a, *v).unwrap(); }
                }
                Repr::Unit => {}
            }
        }
    }

    fn store_allocation_fields(&mut self, obj: PointerValue<'ctx>, layout: LayoutId,
        tag: Option<u32>, vals: &[(Option<BasicValueEnum<'ctx>>, Repr)], write_tag: bool) {
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let i32t = self.ctx.i32_type();
        let lay = &self.prog.layouts[layout as usize];
        // The metadata selects the physical tag location.
        if let Some(t) = tag.filter(|_| write_tag) {
            let addr = self.obj_addr(obj, self.enum_tag_offset(layout));
            self.builder.build_store(addr, i32t.const_int(t as u64, false)).unwrap();
        }

        // Pointer payloads occupy the leading pointer region; scalar/value
        // payloads follow it. The enum tag normally lives in the header.
        let mut ptr_slot = 0u64;
        let mut raw_cursor = if tag.is_some() { self.enum_raw_base(layout) } else { Self::HEADER + lay.ptr_fields as u64 * 8 };
        for (v, repr) in vals {
            match repr {
                Repr::Ref(_) => {
                    let off = Self::HEADER + ptr_slot * 8;
                    ptr_slot += 1;
                    let addr = self.obj_addr(obj, off);
                    if let Some(v) = v { self.builder.build_store(addr, *v).unwrap(); }
                }
                Repr::Scalar(s) => {
                    let sz = (s.bits().max(8) / 8) as u64;
                    raw_cursor = align_up64(raw_cursor, sz);
                    let addr = self.obj_addr(obj, raw_cursor);
                    raw_cursor += sz;
                    if let Some(v) = v {
                        // Initialize the entire bool byte, including its upper
                        // bits: Atom<bool> CAS compares the byte representation.
                        let stored = if *s == ScalarRepr::Bool {
                            self.builder.build_int_z_extend(v.into_int_value(), self.ctx.i8_type(), "bool.byte").unwrap().into()
                        } else { *v };
                        self.builder.build_store(addr, stored).unwrap();
                    }
                }
                Repr::Value(vid) => {
                    // Structs keep nested references in their inline storage;
                    // enums move them to leading slots shared by every variant.
                    let sz = self.prog.values[*vid as usize].size as u64;
                    raw_cursor = align_up64(raw_cursor, 8);
                    let addr = self.obj_addr(obj, raw_cursor);
                    raw_cursor += sz;
                    if let Some(v) = v {
                        self.builder.build_store(addr, *v).unwrap();
                        if tag.is_some() {
                            let mut offsets = Vec::new();
                            value_interior_offsets(&self.prog.values, *vid, 0, &mut offsets);
                            for offset in offsets {
                                let source = self.obj_addr(obj, raw_cursor - sz + offset as u64);
                                let reference = self.builder.build_load(ptr, source, "enum.nested.ref").unwrap();
                                let target = self.obj_addr(obj, Self::HEADER + ptr_slot * 8);
                                self.builder.build_store(target, reference).unwrap();
                                self.builder.build_store(source, ptr.const_null()).unwrap();
                                ptr_slot += 1;
                            }
                        }
                    }
                }
                Repr::Unit => {
                    // Zero-size: no storage.
                }
            }
        }
    }

    fn reload_allocation_values(&mut self, fcx: &mut FnCtx<'ctx>, fields: &[CoreExpr],
        vals: &[(Option<BasicValueEnum<'ctx>>, Repr)]) -> Result<Vec<(Option<BasicValueEnum<'ctx>>, Repr)>, CodegenError> {
        let mut vals = vals.to_vec();
        for (field, value) in fields.iter().zip(&mut vals) {
            if self.repr_relocates(&field.repr) { value.0 = self.gen_expr(fcx, field)?; }
        }
        Ok(vals)
    }

    /// Bytes overwritten completely by non-value constructor stores. Inline
    /// aggregates retain conservative clearing because their padding and nested
    /// pointer regions require a separate initialization proof.
    fn constructor_ranges(&self, layout: LayoutId, tag: Option<u32>,
        vals: &[(Option<BasicValueEnum<'ctx>>, Repr)]) -> Vec<(u64, u64)> {
        let mut ranges = Vec::new();
        let mut pointer_slot = 0;
        let mut raw = if tag.is_some() { self.enum_raw_base(layout) }
            else { Self::HEADER + self.prog.layouts[layout as usize].ptr_fields as u64 * 8 };
        if let Some(_) = tag {
            let offset = self.enum_tag_offset(layout);
            if offset >= Self::HEADER { ranges.push((offset, offset + 4)); }
        }
        for (value, repr) in vals {
            let (start, size) = match repr {
                Repr::Ref(_) => {
                    let start = Self::HEADER + pointer_slot * 8; pointer_slot += 1; (start, 8)
                }
                Repr::Scalar(s) => {
                    let size = s.bits().max(8) as u64 / 8;
                    raw = align_up64(raw, size); let start = raw; raw += size; (start, size)
                }
                Repr::Value(_) => return Vec::new(),
                Repr::Unit => continue,
            };
            if value.is_some() { ranges.push((start, start + size)); }
        }
        ranges.sort_unstable();
        ranges
    }

    fn gen_alloc(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        layout: LayoutId,
        tag: Option<u32>,
        fields: &[CoreExpr],
        span: crate::core::SpanId,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // Evaluate field values FIRST. Then allocate, then store. The allocation
        // is a SAFEPOINT: a GC there relocates every rooted local, so any GC value
        // we cached in a register before the alloc is now STALE (it points at the
        // pre-relocation address). ANF guarantees every GC field is an atomic
        // local (or null), so after the alloc we RELOAD those fields from their
        // (now-relocated) slots — a side-effect-free load — before storing them.
        // Scalar/POD-value fields don't relocate, so their cached values stand.
        let mut vals = Vec::with_capacity(fields.len());
        for fe in fields {
            vals.push((self.gen_expr(fcx, fe)?, fe.repr.clone()));
        }

        let site = self.alloc_site_id(&self.current_fn_name(fcx), layout as u16, span);
        let physical = if fields.is_empty() && tag.is_some_and(|tag| {
            matches!(&self.prog.layouts[layout as usize].meta.kind,
                crate::gc::TypeKind::Enum { variants, .. }
                    if variants.iter().any(|v| v.tag == tag && v.fields.is_empty()))
        }) {
            self.physical_layouts.empty_variants.get(&layout).copied().unwrap_or(layout)
        } else { layout };
        let obj = self.gen_fixed_allocation(fcx, physical, site, FixedInit::Fields { logical: layout, tag }, fields, &vals)?;
        Ok(Some(obj.into()))
    }

    /// Build an inline value-struct aggregate from its field values.
    fn gen_make_value(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        value: ValueId,
        fields: &[CoreExpr],
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let sty = self.value_struct_ty(value);
        let mut agg = sty.get_undef();
        let mut idx = 0u32;
        for fe in fields {
            let v = self.gen_expr(fcx, fe)?;
            if let Some(v) = v {
                agg = self.builder
                    .build_insert_value(agg, v, idx, "vf")
                    .unwrap()
                    .into_struct_value();
                idx += 1;
            }
        }
        Ok(Some(agg.into()))
    }

    /// Build an inline value-enum variant `{ i32 tag, [N x i8] payload }`. The
    /// payload fields are stored into the byte region via an alloca (typed GEPs
    /// + a final load); mem2reg keeps this in registers after O2.
    fn gen_make_value_variant(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        value: ValueId,
        tag: u32,
        fields: &[CoreExpr],
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // Evaluate payload field values first, then build the aggregate.
        let mut vals = Vec::with_capacity(fields.len());
        for f in fields {
            if let Some(v) = self.gen_expr(fcx, f)? {
                vals.push((v, f.repr.clone()));
            }
        }
        Ok(Some(self.build_value_variant(value, tag, &vals)))
    }

    /// Build a value-enum variant aggregate from already-computed payload values
    /// (the guts of `gen_make_value_variant`, factored out so codegen-internal
    /// constructions — e.g. wrapping an array element in `Option::Some` — can
    /// reuse the exact layout logic without re-evaluating field expressions).
    fn build_value_variant(
        &self,
        value: ValueId,
        tag: u32,
        vals: &[(BasicValueEnum<'ctx>, Repr)],
    ) -> BasicValueEnum<'ctx> {
        let sty = self.value_struct_ty(value);
        let i32t = self.ctx.i32_type();
        let max_ptrs = crate::core::value_enum_max_ptrs(
            self.prog.values[value as usize].variants.as_ref().unwrap(), &self.prog.values,
        ) as u32;
        let slot = self.entry_alloca(sty, "ve");
        self.builder.build_store(slot, sty.const_zero()).unwrap();

        if max_ptrs == 0 {
            // Compact `{ i32 tag, [payload bytes] }`: tag field 0, payload field 1.
            let tag_addr = self.builder.build_struct_gep(sty, slot, 0, "ve.tag").unwrap();
            self.builder.build_store(tag_addr, i32t.const_int(tag as u64, false)).unwrap();
            if sty.count_fields() > 1 {
                let payload_addr = self.builder.build_struct_gep(sty, slot, 1, "ve.pl").unwrap();
                let mut off = 0u64;
                for (v, repr) in vals {
                    let (sz, align) = self.repr_size_align(repr);
                    off = align_up64(off, align);
                    let field_addr = self.payload_field_addr(payload_addr, off);
                    self.builder.build_store(field_addr, *v).unwrap();
                    off += sz;
                }
            }
        } else {
            // Pointers-first `{ [max_ptrs x ptr], i32 tag, [raw] }`: ref payloads
            // into the shared leading slots (field 0), tag (field 1), scalars/
            // POD-values into the raw region (field 2). Mirrors the reference-enum
            // heap layout so embedded refs sit at fixed, GC-traceable offsets.
            let ptr = self.ctx.ptr_type(AddressSpace::default());
            let ptr_arr_ty = ptr.array_type(max_ptrs);
            let ptr_arr = self.builder.build_struct_gep(sty, slot, 0, "ve.ptrs").unwrap();
            let tag_addr = self.builder.build_struct_gep(sty, slot, 1, "ve.tag").unwrap();
            self.builder.build_store(tag_addr, i32t.const_int(tag as u64, false)).unwrap();
            let raw_addr = self.builder.build_struct_gep(sty, slot, 2, "ve.raw").unwrap();
            let mut ptr_slot = 0u64;
            let mut raw_off = 0u64;
            for (v, repr) in vals {
                match repr {
                    Repr::Ref(_) => {
                        let elem = unsafe {
                            self.builder.build_in_bounds_gep(
                                ptr_arr_ty, ptr_arr,
                                &[i32t.const_zero(), i32t.const_int(ptr_slot, false)],
                                "ve.pslot",
                            ).unwrap()
                        };
                        ptr_slot += 1;
                        self.builder.build_store(elem, *v).unwrap();
                    }
                    _ => {
                        let (sz, align) = self.repr_size_align(repr);
                        raw_off = align_up64(raw_off, align);
                        let field_addr = self.payload_field_addr(raw_addr, raw_off);
                        raw_off += sz;
                        self.builder.build_store(field_addr, *v).unwrap();
                        if let Repr::Value(id) = repr {
                            let mut offsets = Vec::new();
                            value_interior_offsets(&self.prog.values, *id, 0, &mut offsets);
                            for offset in offsets {
                                let source = self.payload_field_addr(field_addr, offset as u64);
                                let reference = self.builder.build_load(ptr, source, "ve.nested.ref").unwrap();
                                let target = self.payload_field_addr(ptr_arr, ptr_slot * 8);
                                self.builder.build_store(target, reference).unwrap();
                                self.builder.build_store(source, ptr.const_null()).unwrap();
                                ptr_slot += 1;
                            }
                        }
                    }
                }
            }
        }
        self.builder.build_load(sty, slot, "ve.val").unwrap()
    }

    /// Address of a payload field at `byte_off` within the value-enum's byte
    /// region (an `[N x i8]` base pointer).
    fn payload_field_addr(&self, payload_base: PointerValue<'ctx>, byte_off: u64) -> PointerValue<'ctx> {
        self.obj_addr(payload_base, byte_off)
    }

    /// Byte size + alignment of a repr (for laying out value-enum payloads).
    fn repr_size_align(&self, repr: &Repr) -> (u64, u64) {
        match repr {
            Repr::Unit => (0, 1),
            Repr::Scalar(s) => { let b = (s.bits().max(8) / 8) as u64; (b, b) }
            Repr::Ref(_) => (8, 8),
            Repr::Value(id) => { let layout = &self.prog.values[*id as usize]; (layout.size as u64, layout.align as u64) }
        }
    }

    fn obj_addr(&self, obj: PointerValue<'ctx>, byte_off: u64) -> PointerValue<'ctx> {
        // Preserve the base pointer's provenance and expose byte offsets to
        // LLVM alias analysis. Plain GEP adds no inbounds/nonnull assumption.
        unsafe { self.builder.build_gep(self.ctx.i8_type(), obj,
            &[self.ctx.i64_type().const_int(byte_off, false)], "object.field").unwrap() }
    }

    /// Emit a generational write barrier: `ai_gc_write_barrier(thread, obj, new)`
    /// after storing pointer `new` into a field/element of heap object `obj`.
    /// Called unconditionally on every pointer store; the runtime fast-paths the
    /// non-generational / non-old→young cases. Keeps the minor GC's old→young
    /// root set correct — a missed barrier would be a use-after-free.
    fn emit_write_barrier(&self, fcx: &FnCtx<'ctx>, obj: PointerValue<'ctx>, slot: PointerValue<'ctx>, new_val: BasicValueEnum<'ctx>) {
        if !new_val.is_pointer_value() {
            return; // only pointer stores can create an old→young edge
        }
        // Only a young value stored into an old object needs its card marked.
        // The nursery range is fixed; without a nursery its size is 0.
        let i64t = self.ctx.i64_type();
        let field = |offset: usize, name: &str| {
            let address = self.obj_addr(fcx.thread, offset as u64);
            self.builder.build_load(i64t, address, name).unwrap().into_int_value()
        };
        let base = field(crate::runtime::thread_offsets::NURSERY_BASE, "nursery.base");
        let size = field(crate::runtime::thread_offsets::NURSERY_SIZE, "nursery.size");
        let young = |pointer: PointerValue<'ctx>, name: &str| {
            let address = self.builder.build_ptr_to_int(pointer, i64t, "barrier.addr").unwrap();
            let offset = self.builder.build_int_sub(address, base, "barrier.offset").unwrap();
            self.builder.build_int_compare(IntPredicate::ULT, offset, size, name).unwrap()
        };
        let young_value = young(new_val.into_pointer_value(), "barrier.young.value");
        let young_object = young(obj, "barrier.young.object");
        let old_object = self.builder.build_not(young_object, "barrier.old.object").unwrap();
        let needed = self.builder.build_and(young_value, old_object, "barrier.needed").unwrap();
        let slow = self.ctx.append_basic_block(fcx.func, "barrier.mark");
        let done = self.ctx.append_basic_block(fcx.func, "barrier.done");
        self.build_unlikely_branch(needed, slow, done);
        self.builder.position_at_end(slow);
        // The runtime marks the card holding the slot.
        let f = self.module.get_function("ai_gc_write_barrier").unwrap();
        self.builder
            .build_call(f, &[fcx.thread.into(), slot.into(), new_val.into()], "")
            .unwrap();
        self.builder.build_unconditional_branch(done).unwrap();
        self.builder.position_at_end(done);
    }

    /// The byte stride of an array element repr, and whether it's a traced
    /// (pointer/Values) array.
    fn elem_stride_of(&self, elem: &Repr) -> (u64, bool) {
        match elem {
            Repr::Value(vid) => (crate::core::record_shape(&self.prog.values, *vid).0 as u64, false),
            _ => Self::elem_stride(elem),
        }
    }

    /// The LLVM type of one array record of value `vid`: the value, after an
    /// initialized word when it holds references.
    fn record_ty(&self, vid: crate::core::ValueId) -> (inkwell::types::BasicTypeEnum<'ctx>, bool) {
        let value = self.value_struct_ty(vid).as_basic_type_enum();
        let (_, flagged) = crate::core::record_shape(&self.prog.values, vid);
        if flagged { (self.ctx.struct_type(&[self.ctx.i64_type().into(), value], false).as_basic_type_enum(), true) }
        else { (value, false) }
    }

    fn elem_stride(elem: &Repr) -> (u64, bool) {
        match elem {
            Repr::Ref(_) | Repr::Value(_) => (8, true),
            Repr::Scalar(s) => ((s.bits().max(8) / 8) as u64, false),
            Repr::Unit => (1, false),
        }
    }

    /// Sign-extend an array index to i64 (indices are i64-typed for addressing
    /// and the bounds check).
    fn idx_to_i64(&self, index: IntValue<'ctx>) -> IntValue<'ctx> {
        let i64t = self.ctx.i64_type();
        if index.get_type().get_bit_width() < 64 {
            self.builder.build_int_s_extend(index, i64t, "a.ix").unwrap()
        } else {
            index
        }
    }

    /// Element address: HEADER + 8 (count word) + index*stride, as an `inbounds`
    /// GEP off the array pointer. Using a GEP (rather than ptrtoint/inttoptr
    /// arithmetic) preserves pointer provenance, so LLVM's alias analysis and
    /// strength reduction can optimize loops of array accesses. `idx64` must
    /// already be i64 (see `idx_to_i64`).
    fn array_elem_offset(&self, idx64: IntValue<'ctx>, stride: u64) -> IntValue<'ctx> {
        let i64t = self.ctx.i64_type();
        let off = self.builder.build_int_mul(idx64, i64t.const_int(stride, false), "a.off").unwrap();
        self.builder.build_int_add(off, i64t.const_int(Self::HEADER + 8, false), "a.h").unwrap()
    }

    /// Load an array's logical element count for a bounds check. The count word
    /// (at `HEADER`) holds the element count for Values (traced) arrays and the
    /// byte length for Bytes (scalar) arrays; the latter is divided by the
    /// element stride. `arr_repr` is the array object's repr (`Repr::Ref(lid)`).
    fn array_logical_len(&self, obj: PointerValue<'ctx>, arr_repr: &Repr) -> Result<IntValue<'ctx>, CodegenError> {
        let i64t = self.ctx.i64_type();
        let count_addr = self.obj_addr(obj, Self::HEADER);
        let count_load = self.builder.build_load(i64t, count_addr, "cnt").unwrap();
        let count = count_load.into_int_value();
        let lid = match arr_repr {
            Repr::Ref(l) => *l,
            _ => return Err(CodegenError("array bounds check on non-array".into())),
        };
        let lay = &self.prog.layouts[lid as usize];
        let len = if matches!(lay.varlen, crate::core::VarLen::Values | crate::core::VarLen::Records) {
            count
        } else {
            let stride = lay.elem_stride.max(1) as u64;
            self.builder.build_int_unsigned_div(count, i64t.const_int(stride, false), "alen").unwrap()
        };
        Ok(len)
    }

    fn known_array_len(&self, obj: PointerValue<'ctx>, array: &CoreExpr) -> Result<IntValue<'ctx>, CodegenError> {
        if let Some(length) = self.private_heap.length(array) {
            Ok(self.ctx.i64_type().const_int(length, false))
        } else { self.array_logical_len(obj, &array.repr) }
    }

    /// Emit an inlined bounds check: if `idx64 >=u len` (unsigned, so negative
    /// indices wrap large and also trap) call `ai_bounds_fail` (noreturn) on a
    /// cold out-of-line path; otherwise fall through to the in-bounds block.
    fn emit_bounds_check(&self, fcx: &FnCtx<'ctx>, idx64: IntValue<'ctx>, len: IntValue<'ctx>) {
        let oob = self.builder.build_int_compare(IntPredicate::UGE, idx64, len, "oob").unwrap();
        let fail_bb = self.ctx.append_basic_block(fcx.func, "bounds.fail");
        let ok_bb = self.ctx.append_basic_block(fcx.func, "bounds.ok");
        self.builder.build_conditional_branch(oob, fail_bb, ok_bb).unwrap();
        self.builder.position_at_end(fail_bb);
        let f = self.module.get_function("ai_bounds_fail").unwrap();
        self.build_managed_call(fcx, f, &[fcx.thread.into(), idx64.into(), len.into()], "").unwrap();
        self.builder.build_unreachable().unwrap();
        self.builder.position_at_end(ok_bb);
    }

    /// Allocate a `String` varlen object and copy the literal's UTF-8 bytes in.
    fn gen_const_str(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        s: &str,
        repr: &Repr,
        span: crate::core::SpanId,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let i32t = self.ctx.i32_type();
        let i64t = self.ctx.i64_type();
        let lid = match repr { Repr::Ref(l) => *l, _ => return Err(CodegenError("string repr".into())) };
        let bytes = s.as_bytes();
        let n = bytes.len() as u64;
        let site = self.alloc_site_id(&self.current_fn_name(fcx), lid as u16, span);
        let alloc = self.module.get_function("ai_gc_alloc_varlen").unwrap();
        let obj = call_result(self.build_managed_call(fcx,
            alloc,
            &[
                fcx.thread.into(),
                i32t.const_int(lid as u64, false).into(),
                i64t.const_int(n, false).into(),
                i32t.const_int(site as u64, false).into(),
            ],
            "str",
        ).unwrap()).into_pointer_value();
        // Store bytes at HEADER + 8 (after the count word). Byte-by-byte; small
        // literals so this is fine, and it's GC-safe (no allocation between).
        let i8t = self.ctx.i8_type();
        for (i, b) in bytes.iter().enumerate() {
            let addr = self.obj_addr(obj, Self::HEADER + 8 + i as u64);
            self.builder.build_store(addr, i8t.const_int(*b as u64, false)).unwrap();
        }
        Ok(Some(obj.into()))
    }

    fn gen_array_new(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        layout: LayoutId,
        len: &CoreExpr,
        elem: &Repr,
        span: crate::core::SpanId,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let i32t = self.ctx.i32_type();
        let i64t = self.ctx.i64_type();
        let (stride, traced) = self.elem_stride_of(elem);
        let n = self.gen_expr(fcx, len)?.unwrap().into_int_value();
        let n64 = if n.get_type().get_bit_width() < 64 {
            self.builder.build_int_s_extend(n, i64t, "n64").unwrap()
        } else { n };
        let site = self.alloc_site_id(&self.current_fn_name(fcx), layout as u16, span);
        // A small array of a plain layout (no fixed fields) is reserved
        // inline: header, then its count word, then zeroed elements.
        let lay = &self.physical_layouts.layouts[layout as usize];
        let values = matches!(lay.varlen, crate::core::VarLen::Values);
        // Values and record arrays count elements; byte arrays count bytes.
        let counted = values || matches!(lay.varlen, crate::core::VarLen::Records);
        // Records holding references start zeroed: null references, and an
        // unset initialized word where a record has one.
        let inline = lay.ptr_fields == 0 && lay.raw_bytes == 0 && !matches!(lay.varlen, crate::core::VarLen::None);
        let tlab = if inline {
            const DATA: u64 = 24;
            let max = (8192 - DATA) / stride.max(1);
            let small = self.builder.build_int_compare(IntPredicate::ULE, n64, i64t.const_int(max, false), "array.small").unwrap();
            // Bounded by `small` before use, so these cannot wrap.
            let bytes = self.builder.build_int_mul(n64, i64t.const_int(stride, false), "array.bytes").unwrap();
            let varlen_len = if counted { n64 } else { bytes };
            let data_bytes = if values { self.builder.build_int_mul(n64, i64t.const_int(8, false), "array.words").unwrap() } else { bytes };
            let unrounded = self.builder.build_int_add(data_bytes, i64t.const_int(DATA + 7, false), "array.size.raw").unwrap();
            let size = self.builder.build_and(unrounded, i64t.const_int(!7u64, false), "array.size").unwrap();
            let tlab = self.tlab_reserve(fcx, size, small, site);
            let header_type = self.ctx.struct_type(&[i64t.into(), i64t.into(), i64t.into()], false);
            let header = header_type.get_undef();
            let header = self.builder.build_insert_value(header, i64t.const_zero(), 0, "array.header").unwrap();
            let header = self.builder.build_insert_value(header, i64t.const_int(layout as u64, false), 1, "array.header").unwrap();
            let header = self.builder.build_insert_value(header, varlen_len, 2, "array.header").unwrap();
            self.builder.build_store(tlab.object, header).unwrap().set_alignment(8).unwrap();
            let fill = self.builder.build_int_sub(size, i64t.const_int(DATA, false), "array.fill").unwrap();
            self.builder.build_memset(self.obj_addr(tlab.object, DATA), 8, self.ctx.i8_type().const_zero(), fill).unwrap();
            self.tlab_publish(&tlab, site);
            let done = self.builder.get_insert_block().unwrap();
            self.builder.build_unconditional_branch(tlab.merge).unwrap();
            self.builder.position_at_end(tlab.slow);
            Some((tlab.object, done, tlab.merge))
        } else { None };
        // Validate before multiplying: a wrapped byte count would allocate a
        // smaller object than its source-language array length requires.
        let validate = self.module.get_function("ai_array_allocation_len").unwrap();
        let varlen_len = call_result(self.build_managed_call(fcx, validate, &[
            fcx.thread.into(), n64.into(), i64t.const_int(stride, false).into(),
            i32t.const_int((traced || counted) as u64, false).into(),
        ], "array.allocation.len").unwrap()).into_int_value();
        let alloc = self.module.get_function("ai_gc_alloc_varlen").unwrap();
        let obj = call_result(self.build_managed_call(fcx,
            alloc,
            &[
                fcx.thread.into(),
                i32t.const_int(layout as u64, false).into(),
                varlen_len.into(),
                i32t.const_int(site as u64, false).into(),
            ],
            "arr",
        ).unwrap()).into_pointer_value();
        let Some((fast, fast_end, merge)) = tlab else { return Ok(Some(obj.into())) };
        let slow_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(merge).unwrap();
        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(self.ctx.ptr_type(AddressSpace::default()), "arr").unwrap();
        phi.add_incoming(&[(&fast, fast_end), (&obj, slow_end)]);
        Ok(Some(phi.as_basic_value()))
    }

    fn gen_array_len(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        arr: &CoreExpr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let i64t = self.ctx.i64_type();
        let obj = self.gen_expr(fcx, arr)?.unwrap().into_pointer_value();
        if let Some(length) = self.private_heap.length(arr) {
            return Ok(Some(i64t.const_int(length, false).into()));
        }
        // Count word at HEADER. For Bytes arrays it's byte-length; divide by the
        // element stride (from the array's layout via its element repr).
        let count_addr = self.obj_addr(obj, Self::HEADER);
        let count = self.builder.build_load(i64t, count_addr, "cnt").unwrap().into_int_value();
        let lid = match &arr.repr { Repr::Ref(l) => *l, _ => return Err(CodegenError("array_len on non-array".into())) };
        let lay = &self.prog.layouts[lid as usize];
        let logical = if matches!(lay.varlen, crate::core::VarLen::Values | crate::core::VarLen::Records) {
            // Values and record arrays store the element count directly.
            count
        } else {
            // Bytes arrays store the byte length; divide by the element stride.
            let stride = lay.elem_stride.max(1) as u64;
            self.builder.build_int_unsigned_div(count, i64t.const_int(stride, false), "alen").unwrap()
        };
        Ok(Some(logical.into()))
    }

    fn load_array_element(&mut self, fcx: &FnCtx<'ctx>, obj: PointerValue<'ctx>, idx: IntValue<'ctx>, elem: &Repr, array: &CoreExpr) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let (stride, _) = self.elem_stride_of(elem);
        let offset = self.array_elem_offset(idx, stride);
        let ty = self.llvm_ty(elem).ok_or_else(|| CodegenError("array of unit".into()))?;
        if let Repr::Value(vid) = elem {
            // An inline record: read directly when nothing can race it,
            // otherwise as a tear-free snapshot.
            let (rty, flagged) = self.record_ty(*vid);
            let direct = self.aggregate_direct(fcx);
            let fast = self.ctx.append_basic_block(fcx.func, "record.read.direct");
            let slow = self.ctx.append_basic_block(fcx.func, "record.read.snapshot");
            let merge = self.ctx.append_basic_block(fcx.func, "record.read.merge");
            self.builder.build_conditional_branch(direct, fast, slow).unwrap();
            self.builder.position_at_end(fast);
            let at = unsafe { self.builder.build_gep(self.ctx.i8_type(), obj, &[offset], "record.at").unwrap() };
            let quick = self.builder.build_load(rty, at, "record.direct").unwrap();
            quick.as_instruction_value().unwrap().set_alignment(8).unwrap();
            let fast_end = self.builder.get_insert_block().unwrap();
            self.builder.build_unconditional_branch(merge).unwrap();
            self.builder.position_at_end(slow);
            let snapshot = self.entry_alloca(rty, "record.snapshot.storage");
            let read = self.module.get_function("ai_managed_read").unwrap();
            self.build_managed_call(fcx, read, &[
                fcx.thread.into(), obj.into(), offset.into(), snapshot.into(),
                self.ctx.i64_type().const_int(stride, false).into(),
            ], "").unwrap();
            let snap = self.builder.build_load(rty, snapshot, "record.snapshot").unwrap();
            let slow_end = self.builder.get_insert_block().unwrap();
            self.builder.build_unconditional_branch(merge).unwrap();
            self.builder.position_at_end(merge);
            let phi = self.builder.build_phi(rty, "record").unwrap();
            phi.add_incoming(&[(&quick, fast_end), (&snap, slow_end)]);
            let record = phi.as_basic_value();
            if !flagged { return Ok(record); }
            let flag = self.builder.build_extract_value(record.into_struct_value(), 0, "record.initialized").unwrap().into_int_value();
            let empty = self.ctx.append_basic_block(fcx.func, "element.uninitialized");
            let present = self.ctx.append_basic_block(fcx.func, "element.present");
            let unset = self.builder.build_int_compare(IntPredicate::EQ, flag, self.ctx.i64_type().const_zero(), "element.unset").unwrap();
            self.builder.build_conditional_branch(unset, empty, present).unwrap();
            self.builder.position_at_end(empty);
            let fail = self.module.get_function("ai_uninitialized_array_fail").unwrap();
            self.build_managed_call(fcx, fail, &[fcx.thread.into(), idx.into()], "").unwrap();
            self.builder.build_unreachable().unwrap();
            self.builder.position_at_end(present);
            let _ = ty;
            return Ok(self.builder.build_extract_value(record.into_struct_value(), 1, "element.value").unwrap());
        }
        if matches!(elem, Repr::Ref(_)) {
            let pointer = self.managed_load(fcx, obj, offset, &Repr::Scalar(ScalarRepr::Ptr), "element.pointer", array)?.into_pointer_value();
            let empty = self.ctx.append_basic_block(fcx.func, "element.uninitialized");
            let present = self.ctx.append_basic_block(fcx.func, "element.present");
            let is_null = self.builder.build_is_null(pointer, "element.unset").unwrap();
            self.builder.build_conditional_branch(is_null, empty, present).unwrap();
            self.builder.position_at_end(empty);
            let fail = self.module.get_function("ai_uninitialized_array_fail").unwrap();
            self.build_managed_call(fcx, fail, &[fcx.thread.into(), idx.into()], "").unwrap();
            self.builder.build_unreachable().unwrap();
            self.builder.position_at_end(present);
            if matches!(elem, Repr::Value(_)) {
                Ok(self.builder.build_load(ty, self.obj_addr(pointer, Self::HEADER), "element.value").unwrap())
            } else { Ok(pointer.into()) }
        } else { self.managed_load(fcx, obj, offset, elem, "element", array) }
    }

    /// `array_get(a, i)` yields `Option<T>`: `Some(a[i])` when in bounds, `None`
    /// when out of bounds (no abort — the absence of a value is signalled in the
    /// type). `result_repr` is the `Option<T>` value-enum repr assigned in
    /// lowering. The in-bounds load is unchecked (it only runs after the bounds
    /// compare), and when a caller immediately unwraps the result LLVM's SROA
    /// collapses the Option away, leaving just the compare + load.
    fn gen_array_get(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        array: &CoreExpr,
        index: &CoreExpr,
        elem: &Repr,
        result_repr: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // Option<T> from the prelude `enum Option<T> { None, Some(T) }`:
        // declaration order fixes None = tag 0, Some = tag 1.
        const OPTION_NONE_TAG: u32 = 0;
        const OPTION_SOME_TAG: u32 = 1;
        let option_vid = match result_repr {
            Repr::Value(v) => *v,
            _ => {
                return Err(CodegenError("array_get result is not an Option value enum".into()));
            }
        };
        let obj = self.gen_expr(fcx, array)?.unwrap().into_pointer_value();
        let idx = self.gen_expr(fcx, index)?.unwrap().into_int_value();
        let idx64 = self.idx_to_i64(idx);
        let len = self.known_array_len(obj, array)?;
        let sty = self.value_struct_ty(option_vid);

        let oob = self.builder.build_int_compare(IntPredicate::UGE, idx64, len, "oob").unwrap();
        let some_bb = self.ctx.append_basic_block(fcx.func, "aget.some");
        let none_bb = self.ctx.append_basic_block(fcx.func, "aget.none");
        let merge_bb = self.ctx.append_basic_block(fcx.func, "aget.merge");
        self.builder.build_conditional_branch(oob, none_bb, some_bb).unwrap();

        // In bounds: load the element and wrap it in Some.
        self.builder.position_at_end(some_bb);
        let values = if matches!(elem, Repr::Unit) { vec![] } else {
            vec![(self.load_array_element(fcx, obj, idx64, elem, array)?, elem.clone())]
        };
        let some = self.build_value_variant(option_vid, OPTION_SOME_TAG, &values);
        let some_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(merge_bb).unwrap();

        // Out of bounds: None.
        self.builder.position_at_end(none_bb);
        let none = self.build_value_variant(option_vid, OPTION_NONE_TAG, &[]);
        let none_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(merge_bb).unwrap();

        // Merge the two Option values.
        self.builder.position_at_end(merge_bb);
        let phi = self.builder.build_phi(sty, "aget.opt").unwrap();
        phi.add_incoming(&[(&some, some_end), (&none, none_end)]);
        Ok(Some(phi.as_basic_value()))
    }

    /// Legacy direct-return indexing validates the actual backing array. A
    /// caller's earlier metadata check may be invalidated by a racing mutation.
    fn gen_array_get_unchecked(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        array: &CoreExpr,
        index: &CoreExpr,
        elem: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let obj = self.gen_expr(fcx, array)?.unwrap().into_pointer_value();
        let idx = self.gen_expr(fcx, index)?.unwrap().into_int_value();
        let idx64 = self.idx_to_i64(idx);
        let len = self.known_array_len(obj, array)?;
        self.emit_bounds_check(fcx, idx64, len);
        if matches!(elem, Repr::Unit) { return Ok(None); }
        let v = self.load_array_element(fcx, obj, idx64, elem, array)?;
        Ok(Some(v))
    }

    /// `a[i]` index operator: a bounds-checked element load that yields `T`
    /// directly and aborts (clear error) on out-of-bounds — Rust-like indexing.
    fn gen_array_get_checked(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        array: &CoreExpr,
        index: &CoreExpr,
        elem: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let obj = self.gen_expr(fcx, array)?.unwrap().into_pointer_value();
        let idx = self.gen_expr(fcx, index)?.unwrap().into_int_value();
        let idx64 = self.idx_to_i64(idx);
        let len = self.known_array_len(obj, array)?;
        self.emit_bounds_check(fcx, idx64, len);
        if matches!(elem, Repr::Unit) { return Ok(None); }
        let v = self.load_array_element(fcx, obj, idx64, elem, array)?;
        Ok(Some(v))
    }

    fn gen_array_set(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        array: &CoreExpr,
        index: &CoreExpr,
        value: &CoreExpr,
        elem: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let idx = self.gen_expr(fcx, index)?.unwrap().into_int_value();
        if let Repr::Value(vid) = elem {
            return self.gen_record_set(fcx, array, idx, value, *vid);
        }
        // ANF has evaluated operands in source order.
        let val = if matches!(elem, Repr::Unit) {
            self.gen_expr(fcx, value)?;
            let obj = self.gen_expr(fcx, array)?.unwrap().into_pointer_value();
            let idx64 = self.idx_to_i64(idx);
            let len = self.known_array_len(obj, array)?;
            self.emit_bounds_check(fcx, idx64, len);
            return Ok(None);
        } else { self.gen_expr(fcx, value)?.unwrap() };
        let obj = self.gen_expr(fcx, array)?.unwrap().into_pointer_value();
        let idx64 = self.idx_to_i64(idx);
        let len = self.known_array_len(obj, array)?;
        self.emit_bounds_check(fcx, idx64, len);
        let (stride, _) = Self::elem_stride(elem);
        let offset = self.array_elem_offset(idx64, stride);
        self.managed_store(fcx, obj, offset, val, elem, matches!(elem, Repr::Ref(_)), array)?;
        Ok(None)
    }

    /// Store an inline record: directly when nothing can race it, otherwise
    /// with the runtime's tear-free write (or a locked store if contended).
    /// Each embedded reference is barriered.
    fn gen_record_set(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        array: &CoreExpr,
        idx: IntValue<'ctx>,
        value: &CoreExpr,
        vid: crate::core::ValueId,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let (stride, _) = self.elem_stride_of(&Repr::Value(vid));
        let (rty, flagged) = self.record_ty(vid);
        let obj = self.gen_expr(fcx, array)?.unwrap().into_pointer_value();
        let idx64 = self.idx_to_i64(idx);
        let len = self.known_array_len(obj, array)?;
        self.emit_bounds_check(fcx, idx64, len);
        let offset = self.array_elem_offset(idx64, stride);
        let mut refs = Vec::new();
        value_interior_offsets(&self.prog.values, vid, if flagged { 8 } else { 0 }, &mut refs);
        // The record for the (current) value.
        let record = |this: &mut Self, fcx: &mut FnCtx<'ctx>| -> Result<BasicValueEnum<'ctx>, CodegenError> {
            let v = this.gen_expr(fcx, value)?.unwrap();
            if !flagged { return Ok(v); }
            let st = rty.into_struct_type();
            let r = this.builder.build_insert_value(st.get_undef(), this.ctx.i64_type().const_int(1, false), 0, "record.flag").unwrap();
            Ok(this.builder.build_insert_value(r, v, 1, "record.value").unwrap().into_struct_value().into())
        };
        let store_and_barrier = |this: &mut Self, fcx: &mut FnCtx<'ctx>, target: PointerValue<'ctx>| -> Result<(), CodegenError> {
            let r = record(this, fcx)?;
            let at = unsafe { this.builder.build_gep(this.ctx.i8_type(), target, &[offset], "record.at").unwrap() };
            this.builder.build_store(at, r).unwrap().set_alignment(8).unwrap();
            let ptrty = this.ctx.ptr_type(AddressSpace::default());
            for p in &refs {
                let slot = unsafe { this.builder.build_gep(this.ctx.i8_type(), at, &[this.ctx.i64_type().const_int(*p as u64, false)], "record.ref").unwrap() };
                let refv = this.builder.build_load(ptrty, slot, "record.iref").unwrap();
                this.emit_write_barrier(fcx, target, slot, refv);
            }
            Ok(())
        };
        let direct = self.aggregate_direct(fcx);
        let fast = self.ctx.append_basic_block(fcx.func, "record.write.direct");
        let slow = self.ctx.append_basic_block(fcx.func, "record.write.runtime");
        let done = self.ctx.append_basic_block(fcx.func, "record.done");
        self.builder.build_conditional_branch(direct, fast, slow).unwrap();
        self.builder.position_at_end(fast);
        store_and_barrier(self, fcx, obj)?;
        self.builder.build_unconditional_branch(done).unwrap();
        self.builder.position_at_end(slow);
        // The nonparking attempt reads the record from native storage built
        // just before it; nothing between can relocate its references.
        let input = self.entry_alloca(rty, "record.input");
        let r = record(self, fcx)?;
        self.builder.build_store(input, r).unwrap();
        let name = format!("__record_refs_{vid}");
        let offsets = if let Some(global) = self.module.get_global(&name) { global } else {
            let ty = self.ctx.i16_type();
            let values: Vec<_> = refs.iter().map(|&at| ty.const_int(at as u64, false)).collect();
            let initializer = ty.const_array(&values);
            let global = self.module.add_global(initializer.get_type(), None, &name);
            global.set_initializer(&initializer);
            global.set_constant(true);
            global.set_linkage(inkwell::module::Linkage::Private);
            global
        };
        let write = self.module.get_function("ai_managed_try_write").unwrap();
        let result = call_result(self.build_managed_call(fcx, write, &[
            fcx.thread.into(), obj.into(), offset.into(), input.into(),
            self.ctx.i64_type().const_int(stride, false).into(),
            offsets.as_pointer_value().into(), self.ctx.i64_type().const_int(refs.len() as u64, false).into(),
        ], "record.stored").unwrap()).into_int_value();
        let fallback = self.ctx.append_basic_block(fcx.func, "record.contended");
        let success = self.builder.build_int_compare(IntPredicate::NE, result, self.ctx.i8_type().const_zero(), "record.success").unwrap();
        self.builder.build_conditional_branch(success, done, fallback).unwrap();
        self.builder.position_at_end(fallback);
        // A contended acquisition can relocate both the array and the value.
        let locked = self.managed_lock(fcx, obj);
        store_and_barrier(self, fcx, locked)?;
        self.managed_unlock(fcx);
        self.builder.build_unconditional_branch(done).unwrap();
        self.builder.position_at_end(done);
        Ok(None)
    }

    /// Build a closure: allocate the env, store the code pointer (the lifted
    /// function's address) at the start of the raw section, then the captures.
    fn gen_make_closure(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        code: FuncId,
        env: LayoutId,
        captures: &[CoreExpr],
        span: crate::core::SpanId,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // Evaluate captures first. The env allocation is a SAFEPOINT on its slow
        // path, which reloads GC-valued captures from their relocated slots (ANF
        // makes every GC capture an atomic local); the inline fast path cannot
        // collect.
        let mut vals = Vec::with_capacity(captures.len());
        for c in captures {
            vals.push((self.gen_expr(fcx, c)?, c.repr.clone()));
        }
        let site = self.alloc_site_id(&self.current_fn_name(fcx), env as u16, span);
        let obj = self.gen_fixed_allocation(fcx, env, site, FixedInit::ClosureEnv { code }, captures, &vals)?;
        Ok(Some(obj.into()))
    }

    /// Call a closure value: load the code pointer from the env's raw section
    /// and indirect-call `(thread, env, args...)`.
    fn gen_call_closure(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        callee: &CoreExpr,
        args: &[CoreExpr],
        ret: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let ptr = self.ctx.ptr_type(AddressSpace::default());
        let env = self.gen_expr(fcx, callee)?.unwrap().into_pointer_value();
        // Recover the code pointer at runtime from the env header's type_id. The
        // static `callee.repr` is often the placeholder closure layout
        // (ptr_fields=0) — wrong for a closure that captured GC pointers — so we
        // must NOT compute the offset from it. See `ai_closure_code_ptr`.
        let codef = self.module.get_function("ai_closure_code_ptr").unwrap();
        let code_ptr = call_result(self.build_managed_call(fcx,
            codef, &[fcx.thread.into(), env.into()], "code",
        ).unwrap()).into_pointer_value();

        // Build the call signature: (thread, env, args...) -> ret.
        let mut arg_vals: Vec<inkwell::values::BasicMetadataValueEnum> =
            vec![fcx.thread.into(), env.into()];
        let mut arg_tys: Vec<BasicMetadataTypeEnum> = vec![ptr.into(), ptr.into()];
        for a in args {
            if let Some(v) = self.gen_expr(fcx, a)? {
                arg_vals.push(v.into());
                arg_tys.push(self.llvm_ty(&a.repr).unwrap().into());
            }
        }
        let fn_ty = match self.llvm_ty(ret) {
            Some(rt) => rt.fn_type(&arg_tys, false),
            None => self.ctx.void_type().fn_type(&arg_tys, false),
        };
        if fcx.late_roots || fcx.masked { self.ensure_root_frame_linked(fcx); }
        if fcx.masked { self.store_site_origin(fcx); }
        let before = self.prepare_root_reload(fcx);
        let cs = self.builder.build_indirect_call(fn_ty, code_ptr, &arg_vals, "cclo").unwrap();
        self.refresh_copy_phase(fcx);
        if let Some((before, active)) = before { self.reload_if_relocated(fcx, before, active); }
        Ok(match cs.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => Some(v),
            inkwell::values::ValueKind::Instruction(_) => None,
        })
    }

    fn gen_field(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        base: &CoreExpr,
        loc: &FieldLoc,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // A value-struct field is an extractvalue on the inline aggregate.
        if let FieldLoc::ValueField { index } = loc {
            let value = self.gen_expr(fcx, base)?;
            let Repr::Value(vid) = base.repr else { return Err(CodegenError("inline field on non-value".into())); };
            let fields = &self.prog.values[vid as usize].fields;
            let field = fields.get(*index as usize).ok_or_else(|| CodegenError("inline field index out of bounds".into()))?;
            if matches!(field, Repr::Unit) { return Ok(None); }
            let physical = fields[..*index as usize].iter().filter(|r| !matches!(r, Repr::Unit)).count() as u32;
            let agg = value.unwrap().into_struct_value();
            let v = self.builder.build_extract_value(agg, physical, "vfld").unwrap();
            return Ok(Some(v));
        }
        if matches!(loc, FieldLoc::Unit) {
            self.gen_expr(fcx, base)?;
            return Ok(None);
        }
        let obj = self.gen_expr(fcx, base)?.unwrap().into_pointer_value();
        let aggregate = matches!(loc, FieldLoc::ValueAt { .. });
        let (off, lty): (u64, BasicTypeEnum) = match loc {
            FieldLoc::Ptr { idx } => (
                Self::HEADER + (*idx as u64) * 8,
                self.ctx.ptr_type(AddressSpace::default()).as_basic_type_enum(),
            ),
            FieldLoc::Raw { offset, repr } => {
                let lid = match &base.repr { Repr::Ref(l) => *l, _ => return Err(CodegenError("field on non-ref".into())) };
                let lay = &self.prog.layouts[lid as usize];
                let raw_base = Self::HEADER + (lay.ptr_fields as u64) * 8;
                (raw_base + *offset as u64, self.scalar_ty(*repr))
            }
            FieldLoc::ValueAt { offset, value } => {
                // A flattened value aggregate: load the whole LLVM struct from
                // its inline byte offset in the raw region.
                let lid = match &base.repr { Repr::Ref(l) => *l, _ => return Err(CodegenError("field on non-ref".into())) };
                let lay = &self.prog.layouts[lid as usize];
                let raw_base = Self::HEADER + (lay.ptr_fields as u64) * 8;
                (raw_base + *offset as u64, self.value_struct_ty(*value).as_basic_type_enum())
            }
            FieldLoc::Unit | FieldLoc::ValueField { .. } => unreachable!(),
        };
        let v = if aggregate {
            // Direct when nothing can race the read; otherwise a tear-free
            // snapshot through the runtime.
            let direct = self.aggregate_direct(fcx);
            let fast = self.ctx.append_basic_block(fcx.func, "aggregate.read.direct");
            let slow = self.ctx.append_basic_block(fcx.func, "aggregate.read.snapshot");
            let merge = self.ctx.append_basic_block(fcx.func, "aggregate.read.merge");
            self.builder.build_conditional_branch(direct, fast, slow).unwrap();
            self.builder.position_at_end(fast);
            let quick = self.builder.build_load(lty, self.obj_addr(obj, off), "fld.direct").unwrap();
            let fast_end = self.builder.get_insert_block().unwrap();
            self.builder.build_unconditional_branch(merge).unwrap();
            self.builder.position_at_end(slow);
            let snapshot = self.entry_alloca(lty, "fld.snapshot.storage");
            let FieldLoc::ValueAt { value, .. } = loc else { unreachable!() };
            let bytes = self.prog.values[*value as usize].size as u64;
            let read = self.module.get_function("ai_managed_read").unwrap();
            self.build_managed_call(fcx, read, &[
                fcx.thread.into(), obj.into(), self.ctx.i64_type().const_int(off, false).into(),
                snapshot.into(), self.ctx.i64_type().const_int(bytes, false).into(),
            ], "").unwrap();
            let snap = self.builder.build_load(lty, snapshot, "fld.snapshot").unwrap();
            let slow_end = self.builder.get_insert_block().unwrap();
            self.builder.build_unconditional_branch(merge).unwrap();
            self.builder.position_at_end(merge);
            let phi = self.builder.build_phi(lty, "fld.aggregate").unwrap();
            phi.add_incoming(&[(&quick, fast_end), (&snap, slow_end)]);
            phi.as_basic_value()
        } else {
            let repr = match loc {
                FieldLoc::Raw { repr, .. } => Repr::Scalar(*repr),
                FieldLoc::Ptr { .. } => Repr::Scalar(ScalarRepr::Ptr),
                _ => unreachable!(),
            };
            self.managed_load(fcx, obj, self.ctx.i64_type().const_int(off, false), &repr, "fld", base)?
        };
        Ok(Some(v))
    }

    // Read the same pointers-first or compact payload layout used by the
    // constructor and switch matcher. Guarded matches use these reads too.
    fn load_value_enum_payload(
        &mut self,
        aggregate: BasicValueEnum<'ctx>,
        id: ValueId,
        field: usize,
        reprs: &[Repr],
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let descriptor = &self.prog.values[id as usize];
        let variants = descriptor
            .variants
            .as_ref()
            .ok_or_else(|| CodegenError("payload read on non-enum value".into()))?;
        let max_ptrs = crate::core::value_enum_max_ptrs(variants, &self.prog.values) as u32;
        let ty = aggregate.get_type().into_struct_type();
        let slot = self.entry_alloca(ty, "vp.scrut");
        self.builder.build_store(slot, aggregate).unwrap();
        let pointers = if max_ptrs > 0 {
            Some(
                self.builder
                    .build_struct_gep(ty, slot, 0, "vp.ptrs")
                    .unwrap(),
            )
        } else {
            None
        };
        let raw = self
            .builder
            .build_struct_gep(ty, slot, if max_ptrs > 0 { 2 } else { 1 }, "vp.raw")
            .unwrap();
        let mut raw_offset = 0;
        let mut pointer_slot = 0;
        for (index, repr) in reprs.iter().enumerate() {
            let mut interiors = Vec::new();
            if let Repr::Value(nested) = repr {
                value_interior_offsets(&self.prog.values, *nested, 0, &mut interiors);
            }
            let addr = if matches!(repr, Repr::Ref(_)) {
                let addr = self.payload_field_addr(
                    pointers
                        .ok_or_else(|| CodegenError("missing value enum pointer region".into()))?,
                    pointer_slot * 8,
                );
                pointer_slot += 1;
                addr
            } else {
                let (size, align) = self.repr_size_align(repr);
                raw_offset = align_up64(raw_offset, align);
                let addr = self.payload_field_addr(raw, raw_offset);
                raw_offset += size;
                addr
            };
            if index == field {
                let ty = self
                    .llvm_ty(repr)
                    .ok_or_else(|| CodegenError("unit enum payload cannot be read".into()))?;
                let value = self.builder.build_load(ty, addr, "vp.field").unwrap();
                if interiors.is_empty() {
                    return Ok(value);
                }
                let restored = self.entry_alloca(ty, "vp.nested");
                self.builder.build_store(restored, value).unwrap();
                for offset in interiors {
                    let source = self.payload_field_addr(pointers.unwrap(), pointer_slot * 8);
                    let reference = self
                        .builder
                        .build_load(self.ctx.ptr_type(AddressSpace::default()), source, "vp.ref")
                        .unwrap();
                    let target = self.payload_field_addr(restored, offset as u64);
                    self.builder.build_store(target, reference).unwrap();
                    pointer_slot += 1;
                }
                return Ok(self.builder.build_load(ty, restored, "vp.value").unwrap());
            }
            pointer_slot += interiors.len() as u64;
        }
        Err(CodegenError("enum payload index out of bounds".into()))
    }

    fn gen_value_match(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        scrutinee: &CoreExpr,
        arms: &[CoreArm],
        repr: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let vid = match &scrutinee.repr {
            Repr::Value(v) => *v,
            _ => return Err(CodegenError("value match on non-value".into())),
        };
        let max_ptrs = crate::core::value_enum_max_ptrs(
            self.prog.values[vid as usize].variants.as_ref().unwrap(),
            &self.prog.values,
        );
        let agg = self.gen_expr(fcx, scrutinee)?.unwrap();
        let tag = self
            .builder
            .build_extract_value(
                agg.into_struct_value(),
                if max_ptrs == 0 { 0 } else { 1 },
                "vetag",
            )
            .unwrap()
            .into_int_value();
        let func = fcx.func;
        let cont_bb = self.ctx.append_basic_block(func, "vm.cont");
        let result_slot = self.llvm_ty(repr).map(|t| self.entry_alloca(t, "vm.res"));

        let mut default_bb = None;
        let mut cases = Vec::new();
        let mut arm_blocks = Vec::new();
        let i32t = self.ctx.i32_type();
        for (i, arm) in arms.iter().enumerate() {
            let bb = self.ctx.append_basic_block(func, &format!("varm{}", i));
            arm_blocks.push((arm, bb));
            if arm.tag == u32::MAX { default_bb = Some(bb); }
            else { cases.push((i32t.const_int(arm.tag as u64, false), bb)); }
        }
        let unreachable_bb = self.ctx.append_basic_block(func, "vm.unreach");
        self.builder.build_switch(tag, default_bb.unwrap_or(unreachable_bb), &cases).unwrap();
        self.builder.position_at_end(unreachable_bb);
        self.builder.build_unreachable().unwrap();

        let pre = fcx.scope.clone();
        let mut joined = None;
        for (arm, bb) in arm_blocks {
            self.builder.position_at_end(bb);
            let payload_reprs: Vec<_> = arm.binds.iter().map(|&local| fcx.local_reprs[local as usize].clone()).collect();
            for (field, &local) in arm.binds.iter().enumerate() {
                if self.llvm_ty(&payload_reprs[field]).is_none() { continue; }
                let value = self.load_value_enum_payload(agg, vid, field, &payload_reprs)?;
                if let Some(slot) = fcx.slots[local as usize] {
                    self.builder.build_store(slot, value).unwrap();
                    if let Some(root) = fcx.root_slots[local as usize] { self.store_mirror(fcx, root, value); }
                }
            }
            fcx.scope.clone_from(&pre);
            fcx.scope.extend(arm.binds.iter().copied().filter(|&b| !fcx.local_bits[b as usize].is_empty()));
            let v = self.gen_expr(fcx, &arm.body)?;
            if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                meet_scope(&mut joined, &fcx.scope);
                if let (Some(slot), Some(v)) = (result_slot, v) {
                    self.builder.build_store(slot, v).unwrap();
                }
                self.builder.build_unconditional_branch(cont_bb).unwrap();
            }
        }
        fcx.scope = joined.unwrap_or(pre);
        self.builder.position_at_end(cont_bb);
        match result_slot {
            Some(slot) => Ok(Some(self.builder.build_load(self.llvm_ty(repr).unwrap(), slot, "vm.v").unwrap())),
            None => Ok(None),
        }
    }

    fn gen_match(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        scrutinee: &CoreExpr,
        arms: &[CoreArm],
        repr: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let obj = self.gen_expr(fcx, scrutinee)?.unwrap().into_pointer_value();
        let lid = match &scrutinee.repr { Repr::Ref(l) => *l, _ => return Err(CodegenError("match on non-ref enum".into())) };
        let i32t = self.ctx.i32_type();
        let tag_off = self.enum_tag_offset(lid);
        let raw_base = self.enum_raw_base(lid);
        let tag_addr = self.obj_addr(obj, tag_off);
        let tag = self.builder.build_load(i32t, tag_addr, "tag").unwrap().into_int_value();

        let func = fcx.func;
        let cont_bb = self.ctx.append_basic_block(func, "match.cont");
        // Result slot (if non-unit).
        let result_slot = self.llvm_ty(repr).map(|t| self.entry_alloca(t, "match.res")
        );

        // Build arm blocks. A wildcard (tag u32::MAX) becomes the default.
        let mut default_bb = None;
        let mut cases = Vec::new();
        let mut arm_blocks = Vec::new();
        for (i, arm) in arms.iter().enumerate() {
            let bb = self.ctx.append_basic_block(func, &format!("arm{}", i));
            arm_blocks.push((arm, bb));
            if arm.tag == u32::MAX {
                default_bb = Some(bb);
            } else {
                cases.push((i32t.const_int(arm.tag as u64, false), bb));
            }
        }
        let unreachable_bb = self.ctx.append_basic_block(func, "match.unreach");
        let default = default_bb.unwrap_or(unreachable_bb);
        self.builder.build_switch(tag, default, &cases).unwrap();

        // Fill the unreachable default (exhaustive matches without wildcard).
        self.builder.position_at_end(unreachable_bb);
        self.builder.build_unreachable().unwrap();

        let pre = fcx.scope.clone();
        let mut joined = None;
        for (arm, bb) in arm_blocks {
            self.builder.position_at_end(bb);
            let payload_reprs: Vec<_> = arm.binds.iter().map(|&local| fcx.local_reprs[local as usize].clone()).collect();
            for (field, &local) in arm.binds.iter().enumerate() {
                if self.llvm_ty(&payload_reprs[field]).is_none() { continue; }
                let value = self.load_enum_payload(obj, raw_base, field, &payload_reprs)?;
                if let Some(slot) = fcx.slots[local as usize] {
                    self.builder.build_store(slot, value).unwrap();
                    if let Some(root) = fcx.root_slots[local as usize] { self.store_mirror(fcx, root, value); }
                }
            }
            fcx.scope.clone_from(&pre);
            fcx.scope.extend(arm.binds.iter().copied().filter(|&b| !fcx.local_bits[b as usize].is_empty()));
            let v = self.gen_expr(fcx, &arm.body)?;
            if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                meet_scope(&mut joined, &fcx.scope);
                if let (Some(slot), Some(v)) = (result_slot, v) {
                    self.builder.build_store(slot, v).unwrap();
                }
                self.builder.build_unconditional_branch(cont_bb).unwrap();
            }
        }
        fcx.scope = joined.unwrap_or(pre);

        self.builder.position_at_end(cont_bb);
        match result_slot {
            Some(slot) => {
                let ty = self.llvm_ty(repr).unwrap();
                Ok(Some(self.builder.build_load(ty, slot, "match.v").unwrap()))
            }
            None => Ok(None),
        }
    }

    /// Load payload field `field` from a reference enum object, replicating the
    /// physical slot layout the tag-switch binding path uses: pointer payloads
    /// occupy the leading pointer slots (`HEADER + k*8`), raw payloads are packed
    /// (aligned) from the metadata-derived `raw_base`. `payload_reprs` is the full
    /// ordered payload list so we can find `field`'s slot.
    fn load_enum_payload(
        &mut self,
        obj: PointerValue<'ctx>,
        raw_base: u64,
        field: usize,
        payload_reprs: &[Repr],
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let mut pointer_slot = 0;
        let mut raw_cursor = raw_base;
        for (index, repr) in payload_reprs.iter().enumerate() {
            let mut interiors = Vec::new();
            if let Repr::Value(id) = repr {
                value_interior_offsets(&self.prog.values, *id, 0, &mut interiors);
            }
            let addr = if matches!(repr, Repr::Ref(_)) {
                let addr = self.obj_addr(obj, Self::HEADER + pointer_slot * 8);
                pointer_slot += 1;
                addr
            } else {
                let (size, align) = self.repr_size_align(repr);
                // Reference enum value payloads have eight-byte alignment.
                raw_cursor = align_up64(
                    raw_cursor,
                    if matches!(repr, Repr::Value(_)) {
                        8
                    } else {
                        align
                    },
                );
                let addr = self.obj_addr(obj, raw_cursor);
                raw_cursor += size;
                addr
            };
            if index == field {
                let ty = self
                    .llvm_ty(repr)
                    .ok_or_else(|| CodegenError("unit enum payload cannot be read".into()))?;
                let value = self.builder.build_load(ty, addr, "enum.payload").unwrap();
                if interiors.is_empty() {
                    return Ok(value);
                }
                let restored = self.entry_alloca(ty, "enum.nested");
                self.builder.build_store(restored, value).unwrap();
                for offset in interiors {
                    let source = self.obj_addr(obj, Self::HEADER + pointer_slot * 8);
                    let reference = self
                        .builder
                        .build_load(
                            self.ctx.ptr_type(AddressSpace::default()),
                            source,
                            "enum.ref",
                        )
                        .unwrap();
                    self.builder
                        .build_store(self.payload_field_addr(restored, offset as u64), reference)
                        .unwrap();
                    pointer_slot += 1;
                }
                return Ok(self.builder.build_load(ty, restored, "enum.value").unwrap());
            }
            pointer_slot += interiors.len() as u64;
        }
        Err(CodegenError("enum payload index out of bounds".into()))
    }

    fn gen_bin(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        op: BinOp,
        l: &CoreExpr,
        r: &CoreExpr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // Short-circuit && and ||.
        if matches!(op, BinOp::And | BinOp::Or) {
            return self.gen_logical(fcx, op, l, r);
        }
        let scalar = match &l.repr {
            Repr::Scalar(s) => *s,
            _ => return Err(CodegenError("binary op on non-scalar".into())),
        };
        let lv = self.gen_expr(fcx, l)?.unwrap();
        let rv = self.gen_expr(fcx, r)?.unwrap();
        let b = &self.builder;

        if scalar == ScalarRepr::Ptr {
            let predicate = match op {
                BinOp::Eq => IntPredicate::EQ,
                BinOp::Ne => IntPredicate::NE,
                _ => return Err(CodegenError("invalid pointer operator".into())),
            };
            let left = b.build_ptr_to_int(lv.into_pointer_value(), self.ctx.i64_type(), "ptr.left").unwrap();
            let right = b.build_ptr_to_int(rv.into_pointer_value(), self.ctx.i64_type(), "ptr.right").unwrap();
            return Ok(Some(b.build_int_compare(predicate, left, right, "ptr.equal").unwrap().into()));
        }
        if scalar.is_float() {
            let lf = lv.into_float_value();
            let rf = rv.into_float_value();
            let v: BasicValueEnum = match op {
                BinOp::Add => b.build_float_add(lf, rf, "fadd").unwrap().into(),
                BinOp::Sub => b.build_float_sub(lf, rf, "fsub").unwrap().into(),
                BinOp::Mul => b.build_float_mul(lf, rf, "fmul").unwrap().into(),
                BinOp::Div => b.build_float_div(lf, rf, "fdiv").unwrap().into(),
                BinOp::Rem => b.build_float_rem(lf, rf, "frem").unwrap().into(),
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    let pred = float_pred(op);
                    b.build_float_compare(pred, lf, rf, "fcmp").unwrap().into()
                }
                _ => return Err(CodegenError("invalid float op".into())),
            };
            return Ok(Some(v));
        }

        let li = lv.into_int_value();
        let ri = rv.into_int_value();
        let signed = scalar.is_signed();
        // Integer overflow WRAPS (two's complement), like Java/Go/OCaml — see
        // docs/overflow.md. The plain `build_int_*` builders emit LLVM `add`/
        // `sub`/`mul` with NO `nsw`/`nuw` flags, which is defined wrapping. Do
        // NOT switch to the `*_nsw`/`*_nuw` variants: those make overflow UB and
        // would silently break this language guarantee.
        let v: BasicValueEnum = match op {
            BinOp::Add => b.build_int_add(li, ri, "add").unwrap().into(),
            BinOp::Sub => b.build_int_sub(li, ri, "sub").unwrap().into(),
            BinOp::Mul => b.build_int_mul(li, ri, "mul").unwrap().into(),
            BinOp::Div | BinOp::Rem => self.gen_integer_division(fcx, op, li, ri, signed).into(),
            BinOp::BitAnd => b.build_and(li, ri, "and").unwrap().into(),
            BinOp::BitOr => b.build_or(li, ri, "or").unwrap().into(),
            BinOp::BitXor => b.build_xor(li, ri, "xor").unwrap().into(),
            BinOp::Shl | BinOp::Shr => {
                let mask = ri.get_type().const_int((li.get_type().get_bit_width() - 1) as u64, false);
                let count = b.build_and(ri, mask, "shift.count").unwrap();
                if op == BinOp::Shl {
                    b.build_left_shift(li, count, "shl").unwrap().into()
                } else {
                    b.build_right_shift(li, count, signed, "shr").unwrap().into()
                }
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let pred = int_pred(op, signed);
                b.build_int_compare(pred, li, ri, "icmp").unwrap().into()
            }
            BinOp::And | BinOp::Or => unreachable!(),
        };
        Ok(Some(v))
    }

    fn gen_integer_division(
        &self, fcx: &FnCtx<'ctx>, op: BinOp, lhs: IntValue<'ctx>,
        rhs: IntValue<'ctx>, signed: bool,
    ) -> IntValue<'ctx> {
        let ty = lhs.get_type();
        let zero = self.builder.build_int_compare(IntPredicate::EQ, rhs, ty.const_zero(), "div.zero").unwrap();
        let fail = self.ctx.append_basic_block(fcx.func, "div.fail");
        let valid = self.ctx.append_basic_block(fcx.func, "div.valid");
        self.builder.build_conditional_branch(zero, fail, valid).unwrap();
        self.builder.position_at_end(fail);
        let abort = self.module.get_function("ai_arithmetic_fail").unwrap();
        let reason = self.ctx.i32_type().const_int((op == BinOp::Rem) as u64, false);
        self.build_managed_call(fcx, abort, &[fcx.thread.into(), reason.into()], "").unwrap();
        self.builder.build_unreachable().unwrap();
        self.builder.position_at_end(valid);

        // MIN / -1 wraps to MIN and MIN % -1 is zero. Substitute +1 before
        // emitting LLVM division so neither instruction ever receives poison
        // operands, even if later optimization speculates the calculation.
        let rhs = if signed {
            let minimum = match ty.get_bit_width() {
                128 => ty.const_int_arbitrary_precision(&[0, 1u64 << 63]),
                bits => ty.const_int(1u64 << (bits - 1), false),
            };
            let is_min = self.builder.build_int_compare(IntPredicate::EQ, lhs, minimum, "div.min").unwrap();
            let is_minus_one = self.builder.build_int_compare(IntPredicate::EQ, rhs, ty.const_all_ones(), "div.minus1").unwrap();
            let overflow = self.builder.build_and(is_min, is_minus_one, "div.overflow").unwrap();
            self.builder.build_select(overflow, ty.const_int(1, false), rhs, "div.denominator").unwrap().into_int_value()
        } else { rhs };
        match (op, signed) {
            (BinOp::Div, true) => self.builder.build_int_signed_div(lhs, rhs, "sdiv").unwrap(),
            (BinOp::Div, false) => self.builder.build_int_unsigned_div(lhs, rhs, "udiv").unwrap(),
            (BinOp::Rem, true) => self.builder.build_int_signed_rem(lhs, rhs, "srem").unwrap(),
            (BinOp::Rem, false) => self.builder.build_int_unsigned_rem(lhs, rhs, "urem").unwrap(),
            _ => unreachable!(),
        }
    }

    fn gen_logical(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        op: BinOp,
        l: &CoreExpr,
        r: &CoreExpr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // a && b  =>  if a { b } else { false }
        // a || b  =>  if a { true } else { b }
        let lv = self.gen_expr(fcx, l)?.unwrap().into_int_value();
        let func = fcx.func;
        let rhs_bb = self.ctx.append_basic_block(func, "logic.rhs");
        let cont_bb = self.ctx.append_basic_block(func, "logic.cont");
        let entry_bb = self.builder.get_insert_block().unwrap();

        match op {
            BinOp::And => self.builder.build_conditional_branch(lv, rhs_bb, cont_bb).unwrap(),
            BinOp::Or => self.builder.build_conditional_branch(lv, cont_bb, rhs_bb).unwrap(),
            _ => unreachable!(),
        };

        self.builder.position_at_end(rhs_bb);
        let pre = fcx.scope.clone();
        let rv = self.gen_expr(fcx, r)?.unwrap().into_int_value();
        fcx.scope = pre;
        let rhs_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(cont_bb).unwrap();

        self.builder.position_at_end(cont_bb);
        let phi = self.builder.build_phi(self.ctx.bool_type(), "logic").unwrap();
        let short = self.ctx.bool_type().const_int((op == BinOp::Or) as u64, false);
        phi.add_incoming(&[(&short, entry_bb), (&rv, rhs_end)]);
        Ok(Some(phi.as_basic_value()))
    }

    fn gen_un(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        op: UnOp,
        inner: &CoreExpr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let v = self.gen_expr(fcx, inner)?.unwrap();
        let res: BasicValueEnum = match op {
            UnOp::Neg => match &inner.repr {
                Repr::Scalar(s) if s.is_float() => self.builder.build_float_neg(v.into_float_value(), "fneg").unwrap().into()
                ,
                _ => self.builder.build_int_neg(v.into_int_value(), "neg").unwrap().into(),
            },
            UnOp::Not => {
                // bool not, or bitwise not on ints.
                self.builder.build_not(v.into_int_value(), "not").unwrap().into()
            }
        };
        Ok(Some(res))
    }

    fn gen_float_intrinsic(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        intr: crate::core::FloatIntrinsic,
        inner: &CoreExpr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        use crate::core::FloatIntrinsic::*;
        let v = self.gen_expr(fcx, inner)?.unwrap().into_float_value();
        let bits = match &inner.repr { Repr::Scalar(s) => s.bits(), _ => 64 };
        let suffix = if bits == 32 { "f32" } else { "f64" };
        let fty = if bits == 32 { self.ctx.f32_type() } else { self.ctx.f64_type() };
        let intr_name = match intr {
            Sqrt => "llvm.sqrt",
            Abs => "llvm.fabs",
            Floor => "llvm.floor",
            Ceil => "llvm.ceil",
            Round => "llvm.round",
            Trunc => "llvm.trunc",
            Sin => "llvm.sin",
            Cos => "llvm.cos",
            Tan => "llvm.tan",
            Exp => "llvm.exp",
            Ln => "llvm.log",
            Log10 => "llvm.log10",
            Log2 => "llvm.log2",
        };
        let full = format!("{}.{}", intr_name, suffix);
        let f = self.module.get_function(&full).unwrap_or_else(|| {
            let fnty = fty.fn_type(&[fty.into()], false);
            self.module.add_function(&full, fnty, None)
        });
        let r = call_result(self.build_managed_call(fcx, f, &[v.into()], "fi").unwrap());
        Ok(Some(r))
    }

    fn gen_cast(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        value: &CoreExpr,
        from: &Repr,
        to: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let v = self.gen_expr(fcx, value)?.unwrap();
        let (fs, ts) = match (from, to) {
            (Repr::Scalar(a), Repr::Scalar(b)) => (*a, *b),
            _ => return Err(CodegenError("non-scalar cast in v0 slice".into())),
        };
        if ts == ScalarRepr::Char && fs != ScalarRepr::Char {
            // Validate the original integer before narrowing: truncation could
            // turn a negative or oversized input into an apparently valid char.
            let integer = v.into_int_value();
            let wide = if fs.bits() < 64 {
                if fs.is_signed() {
                    self.builder
                        .build_int_s_extend(integer, self.ctx.i64_type(), "char.source")
                        .unwrap()
                } else {
                    self.builder
                        .build_int_z_extend(integer, self.ctx.i64_type(), "char.source")
                        .unwrap()
                }
            } else {
                integer
            };
            let ty = self.ctx.i64_type();
            let too_large = self
                .builder
                .build_int_compare(
                    IntPredicate::UGT,
                    wide,
                    ty.const_int(0x10ffff, false),
                    "char.range",
                )
                .unwrap();
            let surrogate_low = self
                .builder
                .build_int_compare(
                    IntPredicate::UGE,
                    wide,
                    ty.const_int(0xd800, false),
                    "char.surrogate.low",
                )
                .unwrap();
            let surrogate_high = self
                .builder
                .build_int_compare(
                    IntPredicate::ULE,
                    wide,
                    ty.const_int(0xdfff, false),
                    "char.surrogate.high",
                )
                .unwrap();
            let surrogate = self
                .builder
                .build_and(surrogate_low, surrogate_high, "char.surrogate")
                .unwrap();
            let invalid = self
                .builder
                .build_or(too_large, surrogate, "char.invalid")
                .unwrap();
            let fail = self.ctx.append_basic_block(fcx.func, "char.fail");
            let valid = self.ctx.append_basic_block(fcx.func, "char.valid");
            self.builder
                .build_conditional_branch(invalid, fail, valid)
                .unwrap();
            self.builder.position_at_end(fail);
            let abort = self.module.get_function("ai_arithmetic_fail").unwrap();
            self.build_managed_call(
                fcx,
                abort,
                &[
                    fcx.thread.into(),
                    self.ctx.i32_type().const_int(2, false).into(),
                ],
                "",
            )
            .map_err(|error| CodegenError(error.to_string()))?;
            self.builder.build_unreachable().unwrap();
            self.builder.position_at_end(valid);
        }
        let b = &self.builder;
        let res: BasicValueEnum = match (fs.is_float(), ts.is_float()) {
            (false, false) => {
                let iv = v.into_int_value();
                let dst = self.scalar_ty(ts).into_int_type();
                if ts.bits() < fs.bits() {
                    b.build_int_truncate(iv, dst, "trunc").unwrap().into()
                } else if ts.bits() > fs.bits() {
                    if fs.is_signed() {
                        b.build_int_s_extend(iv, dst, "sext").unwrap().into()
                    } else {
                        b.build_int_z_extend(iv, dst, "zext").unwrap().into()
                    }
                } else {
                    iv.into()
                }
            }
            (true, true) => {
                let fv = v.into_float_value();
                let dst = self.scalar_ty(ts).into_float_type();
                if ts.bits() < fs.bits() {
                    b.build_float_trunc(fv, dst, "fptrunc").unwrap().into()
                } else if ts.bits() > fs.bits() {
                    b.build_float_ext(fv, dst, "fpext").unwrap().into()
                } else {
                    fv.into()
                }
            }
            (false, true) => {
                let iv = v.into_int_value();
                let dst = self.scalar_ty(ts).into_float_type();
                if fs.is_signed() {
                    b.build_signed_int_to_float(iv, dst, "sitofp")
                        .unwrap()
                        .into()
                } else {
                    b.build_unsigned_int_to_float(iv, dst, "uitofp")
                        .unwrap()
                        .into()
                }
            }
            (true, false) => {
                let fv = v.into_float_value();
                let dst = self.scalar_ty(ts).into_int_type();
                // Plain fptosi/fptoui produce poison on NaN and out-of-range
                // input. The saturating intrinsics define all inputs, including
                // the 64-bit boundaries that cannot be represented exactly in
                // either source float type.
                let name = if ts.is_signed() {
                    "llvm.fptosi.sat"
                } else {
                    "llvm.fptoui.sat"
                };
                let intrinsic = inkwell::intrinsics::Intrinsic::find(name)
                    .ok_or_else(|| CodegenError(format!("missing intrinsic {name}")))?;
                let function = intrinsic
                    .get_declaration(&self.module, &[dst.into(), fv.get_type().into()])
                    .ok_or_else(|| CodegenError(format!("cannot declare intrinsic {name}")))?;
                b.build_call(function, &[fv.into()], "float.cast")
                    .map_err(|error| CodegenError(error.to_string()))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| CodegenError(format!("intrinsic {name} returned no value")))?
            }
        };
        Ok(Some(res))
    }

    /// Emit path-local cleanup without consuming compile-time scope entries:
    /// another branch can still continue evaluating this same foreign call.
    /// Releasing foreign buffers may collect: a traced exit value computed
    /// before it is read again from its (rooted) local.
    fn reload_after_cleanup(&mut self, fcx: &mut FnCtx<'ctx>, e: &CoreExpr, buffer_base: usize, value: &mut Option<BasicValueEnum<'ctx>>) -> Result<(), CodegenError> {
        let may_move = fcx.pending_ffi_buffers.len() > buffer_base;
        let traced = matches!(e.repr, Repr::Ref(_)) || matches!(e.repr, Repr::Value(id) if value_has_ref(&self.prog.values, id));
        if may_move && traced {
            match e.kind.as_ref() {
                CoreExprKind::Local(_) => *value = self.gen_expr(fcx, e)?,
                CoreExprKind::ConstZero(_) => {}
                _ => return Err(CodegenError("exit cleanup requires a rooted ANF value".into())),
            }
        }
        Ok(())
    }

    fn emit_ffi_cleanup(&self, fcx: &FnCtx<'ctx>, buffer_base: usize, stack_base: usize) {
        let free = self.module.get_function("ai_ffi_buffer_free_v2").unwrap();
        for &token in fcx.pending_ffi_buffers[buffer_base..].iter().rev() {
            self.build_managed_call(fcx, free, &[fcx.thread.into(), token.into()], "").unwrap();
        }
        for &token in fcx.ffi_stack_tokens[stack_base..].iter().rev() {
            let intrinsic = inkwell::intrinsics::Intrinsic::find("llvm.stackrestore").unwrap();
            let types = [self.ctx.ptr_type(AddressSpace::default()).into()];
            let overloads = if intrinsic.is_overloaded() { &types[..] } else { &[] };
            let restore = intrinsic.get_declaration(&self.module, overloads).unwrap();
            self.build_managed_call(fcx, restore, &[token.into()], "").unwrap();
        }
    }
    fn finish_terminated_call(&self, fcx: &mut FnCtx<'ctx>, copies: usize, buffers: usize, stacks: usize) -> bool {
        if self.builder.get_insert_block().unwrap().get_terminator().is_none() { return false; }
        fcx.pending_copy_outs.truncate(copies);
        fcx.pending_ffi_buffers.truncate(buffers);
        fcx.ffi_stack_tokens.truncate(stacks);
        true
    }

    fn gen_call(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        fid: FuncId,
        args: &[CoreExpr],
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let callee = self.funcs[&fid];
        // Foreign `extern "C"` callees take no leading Thread* — they're plain C.
        let is_extern = self.prog.funcs[fid as usize].is_extern;
        // ABI coercion temporaries live only through this call and its copy-out.
        // Restore their stack storage before returning to a calling loop.
        let stack_base = fcx.ffi_stack_tokens.len();
        let stack_token = if is_extern {
            let intrinsic = inkwell::intrinsics::Intrinsic::find("llvm.stacksave").unwrap();
            let types = [self.ctx.ptr_type(AddressSpace::default()).into()];
            let overloads = if intrinsic.is_overloaded() { &types[..] } else { &[] };
            let save = intrinsic.get_declaration(&self.module, overloads).unwrap();
            Some(call_result(self.build_managed_call(fcx, save, &[], "ffi.stack").unwrap()))
        } else { None };
        if let Some(token) = stack_token { fcx.ffi_stack_tokens.push(token); }
        let mut cargs: Vec<inkwell::values::BasicMetadataValueEnum> =
            if is_extern { vec![] } else { vec![fcx.thread.into()] };
        let param_reprs = self.prog.funcs[fid as usize].params.clone();
        let extern_by_ref = self.prog.funcs[fid as usize].extern_by_ref.clone();
        let foreign_args = if is_extern { self.foreign_args(&self.prog.funcs[fid as usize]) } else { vec![] };
        // Any FFI copy-out buffers queued while evaluating THIS call's arguments
        // are drained after the call; snapshot the queue depth to isolate them.
        let copy_out_base = fcx.pending_copy_outs.len();
        let buffer_base = fcx.pending_ffi_buffers.len();

        // GC-SAFETY: a `Ref` argument backed by a `Local` must be loaded from its
        // root slot AFTER every sibling argument that might allocate — otherwise a
        // collection triggered while evaluating a later argument (e.g. the closure
        // in `atom.swap(|x| ..)`) relocates the object, leaving the
        // already-materialized pointer dangling. So we DEFER such loads to a
        // second pass, right before the call. Their slots are always current (the
        // collector updates them), so reloading is always safe.
        // `cargs` is built positionally; `deferred` holds (cargs-index, local-id).
        let mut deferred: Vec<(usize, u32)> = Vec::new();
        for (i, a) in args.iter().enumerate() {
            // For an extern callee, a value-struct argument crosses either BY
            // VALUE (coerced into register-shaped types per the C ABI) or BY
            // POINTER (a `mut` out-param, or > 16 bytes passed indirectly). The
            // struct lives on the native stack — never a GC heap object — so its
            // address is stable for the call and needs no pinning. See docs/ffi.md.
            if is_extern {
                if let Some(Repr::Value(_)) = param_reprs.get(i) {
                    let by_ref = extern_by_ref.get(i).copied().unwrap_or(false);
                    let coerce = match foreign_args[i] {
                        ForeignArg::Direct(ty) => Some(ty),
                        _ => None,
                    };
                    match coerce {
                        // By value (≤ 16B): materialize the struct, reinterpret it
                        // into the coercion type through a stack slot (the upper
                        // bytes of a partly-filled register slot are don't-care, as
                        // in clang's lowering), and pass the coerced value.
                        Some(coerce_ty) => {
                            let value = self.gen_expr(fcx, a)?;
                            if self.finish_terminated_call(fcx, copy_out_base, buffer_base, stack_base) { return Ok(None); }
                            if let Some(v) = value {
                                let slot =
                                    self.entry_alloca(coerce_ty, "ffi.coerce");
                                self.builder.build_store(slot, v).unwrap();
                                let loaded =
                                    self.builder.build_load(coerce_ty, slot, "ffi.arg").unwrap();
                                cargs.push(loaded.into());
                            }
                            continue;
                        }
                        // By pointer: a value-struct LOCAL passes its own alloca
                        // (so a `mut` out-param's writes land back in the caller's
                        // variable); a temporary spills to a fresh alloca.
                        None => {
                            if by_ref {
                                if let CoreExprKind::Local(id) = a.kind.as_ref() {
                                    if let Some(slot) = fcx.slots[*id as usize] {
                                        cargs.push(slot.into());
                                        continue;
                                    }
                                }
                            }
                            let value = self.gen_expr(fcx, a)?;
                            if self.finish_terminated_call(fcx, copy_out_base, buffer_base, stack_base) { return Ok(None); }
                            if let Some(v) = value {
                                let slot =
                                    self.entry_alloca(v.get_type(), "ffi.arg");
                                self.builder.build_store(slot, v).unwrap();
                                cargs.push(slot.into());
                            }
                            continue;
                        }
                    }
                }
            }
            // Defer a `Ref` argument that is a plain `Local` (reload from its slot
            // after all args are evaluated — see GC-SAFETY note above).
            if let CoreExprKind::Local(id) = a.kind.as_ref() {
                if matches!(a.repr, Repr::Ref(_)) && fcx.slots[*id as usize].is_some() {
                    deferred.push((cargs.len(), *id));
                    cargs.push(self.ctx.ptr_type(AddressSpace::default()).const_null().into());
                    continue;
                }
            }
            let value = self.gen_expr(fcx, a)?;
            if self.finish_terminated_call(fcx, copy_out_base, buffer_base, stack_base) { return Ok(None); }
            if let Some(v) = value {
                cargs.push(v.into());
            }
        }
        // Second pass: reload deferred `Ref`-`Local` args from their (now-current,
        // post-any-allocation) root slots.
        for (ci, id) in deferred {
            let slot = fcx.slots[id as usize].unwrap();
            let lty = self.llvm_ty(&fcx.local_reprs[id as usize]).unwrap();
            let v = self.builder.build_load(lty, slot, "arg.reload").unwrap();
            cargs[ci] = v.into();
        }
        // Wrap a foreign call in the managed↔native transition: publish our
        // frame chain so a concurrent GC can find this thread's roots while we
        // are in native code, then clear it on return. Safe by default — the
        // call's correctness does not depend on the C function not allocating.
        if is_extern {
            let enter = self.module.get_function("ai_ffi_enter").unwrap();
            self.build_managed_call(fcx, enter, &[fcx.thread.into()], "").unwrap();
        }
        let cs = if is_extern { self.builder.build_call(callee, &cargs, "call").unwrap() }
            else { self.build_managed_call(fcx, callee, &cargs, "call").unwrap() };
        if is_extern {
            for (index, abi) in foreign_args.iter().enumerate() {
                if matches!(abi, ForeignArg::Indirect { byval: true }) {
                    let Repr::Value(vid) = param_reprs[index] else { unreachable!() };
                    cs.add_attribute(inkwell::attributes::AttributeLoc::Param(index as u32),
                        self.foreign_byval_attribute(vid));
                }
            }
            let leave = self.module.get_function("ai_ffi_leave").unwrap();
            self.build_managed_call(fcx, leave, &[fcx.thread.into()], "").unwrap();
        }
        // Copy-out: write each `mut` array's native buffer back into its heap
        // object now that C has filled it (e.g. read(fd, buf, n)).
        if fcx.pending_copy_outs.len() > copy_out_base {
            let cout = self.module.get_function("ai_buf_copy_out_v2").unwrap();
            let pending: Vec<_> = fcx.pending_copy_outs.drain(copy_out_base..).collect();
            for (src, buf, byte_len, bits) in pending {
                // Native callbacks or another mutator may have moved the array.
                let obj = self.gen_expr(fcx, &src)?.unwrap().into_pointer_value();
                self.build_managed_call(fcx,
                    cout,
                    &[fcx.thread.into(), obj.into(), buf.into(), byte_len.into(), self.ctx.i64_type().const_int(bits as u64, false).into()],
                    "",
                ).unwrap();
            }
        }
        let mut ret_val = match cs.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => Some(v),
            inkwell::values::ValueKind::Instruction(_) => None,
        };
        // Decode a by-value value-struct return: the call yielded the ABI
        // coercion type (an `iN` / `[N x i64]` / `[N x float]`); reinterpret it
        // back into the value struct through a stack slot.
        if is_extern {
            if let Repr::Value(vid) = self.prog.funcs[fid as usize].ret {
                if let (Some(coerce_ty), Some(coerced)) =
                    (self.abi_coerce(vid, true), ret_val)
                {
                    let struct_ty = self.value_struct_ty(vid);
                    let slot = self.entry_alloca(coerce_ty, "ffi.ret");
                    self.builder.build_store(slot, coerced).unwrap();
                    let sval =
                        self.builder.build_load(struct_ty, slot, "ffi.retval").unwrap();
                    ret_val = Some(sval);
                }
            }
        }
        self.emit_ffi_cleanup(fcx, buffer_base, stack_base);
        fcx.pending_ffi_buffers.truncate(buffer_base);
        fcx.ffi_stack_tokens.truncate(stack_base);
        Ok(ret_val)
    }

    /// Emit a GC safepoint poll: `if (thread.state != 0) ai_gc_pollcheck_slow(thread)`.
    /// Acquire pairs with the coordinator's release store. Volatile also keeps
    /// polls at loop checkpoints; volatility alone would not prevent a race.
    fn emit_safepoint_poll(&self, fcx: &FnCtx<'ctx>) {
        let i8t = self.ctx.i8_type();
        let state_ptr = self.thread_field_ptr(fcx.func, crate::runtime::thread_offsets::STATE);
        let load = self.builder.build_load(i8t, state_ptr, "gcstate").unwrap();
        let instruction = load.as_instruction_value().unwrap();
        instruction.set_alignment(1).unwrap();
        instruction
            .set_atomic_ordering(AtomicOrdering::Acquire)
            .unwrap();
        instruction.set_volatile(true).unwrap();
        let is_set = self
            .builder
            .build_int_compare(
                IntPredicate::NE,
                load.into_int_value(),
                i8t.const_zero(),
                "gcpoll",
            )
            .unwrap();
        let slow_bb = self.ctx.append_basic_block(fcx.func, "gc.slow");
        let cont_bb = self.ctx.append_basic_block(fcx.func, "gc.cont");
        self.builder.build_conditional_branch(is_set, slow_bb, cont_bb).unwrap();
        self.builder.position_at_end(slow_bb);
        let poll = self.module.get_function("ai_gc_pollcheck_slow").unwrap();
        self.build_managed_call(fcx, poll, &[fcx.thread.into()], "").unwrap();
        self.builder.build_unconditional_branch(cont_bb).unwrap();
        self.builder.position_at_end(cont_bb);
    }

    /// The remaining body cannot park. Only the slow entry path needs a frame;
    /// register incoming refs before parking, refresh them and unlink on return.
    fn emit_entry_only_poll(&self, fcx: &mut FnCtx<'ctx>) {
        let address = self.thread_field_ptr(fcx.func, crate::runtime::thread_offsets::STATE);
        let load = self.builder.build_load(self.ctx.i8_type(), address, "gcstate").unwrap();
        let instruction = load.as_instruction_value().unwrap();
        instruction.set_alignment(1).unwrap();
        instruction.set_atomic_ordering(AtomicOrdering::Acquire).unwrap();
        instruction.set_volatile(true).unwrap();
        let requested = self.builder.build_int_compare(IntPredicate::NE, load.into_int_value(), self.ctx.i8_type().const_zero(), "gcpoll").unwrap();
        let slow = self.ctx.append_basic_block(fcx.func, "entry.gc.slow");
        let ready = self.ctx.append_basic_block(fcx.func, "entry.gc.ready");
        self.builder.build_conditional_branch(requested, slow, ready).unwrap();
        self.builder.position_at_end(slow);
        self.ensure_root_frame_linked(fcx);
        for (index, root) in fcx.root_slots.iter().enumerate() {
            if let Some(root) = root {
                let value = self.builder.build_load(self.ctx.ptr_type(AddressSpace::default()), fcx.slots[index].unwrap(), "entry.root").unwrap();
                self.builder.build_store(*root, value).unwrap();
            }
        }
        let poll = self.module.get_function("ai_gc_pollcheck_slow").unwrap();
        self.builder.build_call(poll, &[fcx.thread.into()], "").unwrap();
        self.refresh_copy_phase(fcx);
        self.reload_root_mirrors(fcx);
        self.emit_unlink(fcx);
        self.builder.build_unconditional_branch(ready).unwrap();
        self.builder.position_at_end(ready);
        fcx.root_slots.fill(None);
        fcx.unlink = None;
        fcx.frame_active = None;
        fcx.deferred_frame_origin = None;
    }

    fn gen_loop(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        body: &CoreBlock,
        repr: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let func = fcx.func;
        let header_bb = self.ctx.append_basic_block(func, "loop.header");
        let cont_bb = self.ctx.append_basic_block(func, "loop.cont");
        // A `loop` that breaks with a value stores it here; the value is read
        // right after the loop, with no safepoint in between.
        let slot = match self.llvm_ty(repr) {
            Some(t) => Some(self.entry_alloca(t, "loop.value")),
            None => None,
        };
        self.builder.build_unconditional_branch(header_bb).unwrap();

        fcx.loops.push((cont_bb, slot));
        fcx.loop_headers.push(header_bb);
        fcx.loop_ffi_bases.push((fcx.pending_ffi_buffers.len(), fcx.ffi_stack_tokens.len()));

        self.builder.position_at_end(header_bb);
        // GC safepoint poll at the loop header: atomic acquire, volatile;
        // if non-zero, trap into ai_gc_pollcheck_slow so the mutator parks.
        if !self.bounded_loops.contains(body) { self.emit_safepoint_poll(fcx); }
        let pre = fcx.scope.clone();
        fcx.loop_scopes.push(None);
        self.gen_block(fcx, body)?;
        fcx.scope = fcx.loop_scopes.pop().unwrap().unwrap_or(pre);
        // Back-edge: if the body fell through, loop again.
        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            self.builder.build_unconditional_branch(header_bb).unwrap();
        }

        fcx.loops.pop();
        fcx.loop_headers.pop();
        fcx.loop_ffi_bases.pop();
        self.builder.position_at_end(cont_bb);
        if let (Some(slot), Some(t)) = (slot, self.llvm_ty(repr)) {
            return Ok(Some(self.builder.build_load(t, slot, "loop.result").unwrap()));
        }
        Ok(None)
    }

    /// A block that `Leave` exits early; see `CoreExprKind::Once`.
    fn gen_once(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        label: u32,
        body: &CoreBlock,
        repr: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let exit = self.ctx.append_basic_block(fcx.func, "once.exit");
        let ty = self.llvm_ty(repr);
        let slot = ty.map(|t| self.entry_alloca(t, "once.value"));
        fcx.onces.push((label, exit, slot, (fcx.pending_ffi_buffers.len(), fcx.ffi_stack_tokens.len())));
        let pre = fcx.scope.clone();
        fcx.once_scopes.push(None);
        let value = self.gen_block(fcx, body)?;
        fcx.onces.pop();
        let mut joined = fcx.once_scopes.pop().unwrap();
        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            meet_scope(&mut joined, &fcx.scope);
            match (slot, value) {
                (Some(slot), Some(v)) => { self.builder.build_store(slot, v).unwrap(); }
                // A value block that ends open without a value diverged.
                (Some(_), None) => { self.builder.build_unreachable().unwrap(); }
                _ => {}
            }
            if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                self.builder.build_unconditional_branch(exit).unwrap();
            }
        }
        fcx.scope = joined.unwrap_or(pre);
        self.builder.position_at_end(exit);
        Ok(match (slot, ty) {
            (Some(slot), Some(t)) => Some(self.builder.build_load(t, slot, "once.result").unwrap()),
            _ => None,
        })
    }

    fn gen_if(
        &mut self,
        fcx: &mut FnCtx<'ctx>,
        cond: &CoreExpr,
        then_b: &CoreBlock,
        else_b: &CoreBlock,
        repr: &Repr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let cv = self.gen_expr(fcx, cond)?.unwrap().into_int_value();
        let func = fcx.func;
        let then_bb = self.ctx.append_basic_block(func, "then");
        let else_bb = self.ctx.append_basic_block(func, "else");
        let cont_bb = self.ctx.append_basic_block(func, "ifcont");
        self.builder.build_conditional_branch(cv, then_bb, else_bb).unwrap();

        // A branch of a value-typed `if` that ends open without a value
        // diverged (return/continue/break) and left the builder in a dead
        // block; it must not reach the merge, whose phi has no value for it.
        let wants_value = self.llvm_ty(repr).is_some();

        self.builder.position_at_end(then_bb);
        let pre = fcx.scope.clone();
        let mut joined = None;
        let tv = self.gen_block(fcx, then_b)?;
        let then_end = self.builder.get_insert_block().unwrap();
        let mut then_open = then_end.get_terminator().is_none();
        if then_open && wants_value && tv.is_none() {
            self.builder.build_unreachable().unwrap();
            then_open = false;
        }
        if then_open {
            self.builder.build_unconditional_branch(cont_bb).unwrap();
            meet_scope(&mut joined, &fcx.scope);
        }

        self.builder.position_at_end(else_bb);
        fcx.scope.clone_from(&pre);
        let ev = self.gen_block(fcx, else_b)?;
        let else_end = self.builder.get_insert_block().unwrap();
        let mut else_open = else_end.get_terminator().is_none();
        if else_open && wants_value && ev.is_none() {
            self.builder.build_unreachable().unwrap();
            else_open = false;
        }
        if else_open {
            self.builder.build_unconditional_branch(cont_bb).unwrap();
            meet_scope(&mut joined, &fcx.scope);
        }
        fcx.scope = joined.unwrap_or(pre);

        self.builder.position_at_end(cont_bb);
        match (self.llvm_ty(repr), then_open || else_open) {
            (Some(ty), true) => {
                let phi = self.builder.build_phi(ty, "ifval").unwrap();
                if then_open {
                    if let Some(tv) = tv {
                        phi.add_incoming(&[(&tv, then_end)]);
                    }
                }
                if else_open {
                    if let Some(ev) = ev {
                        phi.add_incoming(&[(&ev, else_end)]);
                    }
                }
                Ok(Some(phi.as_basic_value()))
            }
            _ => Ok(None),
        }
    }
}

/// How a freshly allocated fixed-size object is initialized.
#[derive(Clone, Copy)]
enum FixedInit {
    /// A struct or enum variant of `logical`, with its fields.
    Fields { logical: LayoutId, tag: Option<u32> },
    /// The environment of closure `code`: its code pointer and captures.
    ClosureEnv { code: FuncId },
}

impl FixedInit {
    /// A runtime-allocated object starts zeroed, so only a nonzero enum tag
    /// has to be stored.
    fn nonzero_tag(self) -> bool {
        matches!(self, FixedInit::Fields { tag: Some(tag), .. } if tag != 0)
    }
}

/// An inline TLAB reservation in progress (see `tlab_reserve`).
struct TlabReservation<'ctx> {
    window: PointerValue<'ctx>,
    cursor_addr: PointerValue<'ctx>,
    end: IntValue<'ctx>,
    size: IntValue<'ctx>,
    object: PointerValue<'ctx>,
    slow: inkwell::basic_block::BasicBlock<'ctx>,
    merge: inkwell::basic_block::BasicBlock<'ctx>,
}

struct FnCtx<'ctx> {
    func: FunctionValue<'ctx>,
    slots: Vec<Option<PointerValue<'ctx>>>,
    root_slots: Vec<Option<PointerValue<'ctx>>>,
    frame_active: Option<PointerValue<'ctx>>,
    deferred_frame_origin: Option<PointerValue<'ctx>>,
    local_reprs: Vec<Repr>,
    thread: PointerValue<'ctx>,
    /// Per active loop: (continuation block, optional break-value slot).
    loops: Vec<(BasicBlock<'ctx>, Option<PointerValue<'ctx>>)>,
    /// Per active loop: the header block (`continue` target).
    loop_headers: Vec<BasicBlock<'ctx>>,
    /// Native buffer and stack marks at each loop entry.
    loop_ffi_bases: Vec<(usize, usize)>,
    /// Enclosing `Once` blocks: label, exit block, value slot, and the FFI
    /// buffer/token depths to unwind to on `Leave`.
    onces: Vec<(u32, inkwell::basic_block::BasicBlock<'ctx>, Option<PointerValue<'ctx>>, (usize, usize))>,
    /// Active foreign scopes, restored on normal completion or escaping edges.
    ffi_stack_tokens: Vec<BasicValueEnum<'ctx>>,
    /// `(frame ptr, frame type, &thread.top_frame)` for the GC frame epilogue.
    /// `None` when this function allocated no GC frame (no Ref locals).
    unlink: Option<(PointerValue<'ctx>, inkwell::types::StructType<'ctx>, PointerValue<'ctx>)>,
    /// FFI copy-out buffers awaiting write-back: `(heap object, native buffer,
    /// byte length)`. Filled by an `AsCBytes { copy_out: true }` argument and
    /// drained by `gen_call` AFTER the extern call returns. See `docs/ffi.md`.
    pending_copy_outs: Vec<(CoreExpr, PointerValue<'ctx>, IntValue<'ctx>, u32)>,
    /// Opaque native ownership tokens for enclosing foreign calls, including nested arguments.
    pending_ffi_buffers: Vec<PointerValue<'ctx>>,
    /// The copying phase as of the last safepoint, in a local. Copying can
    /// only start in a pause, so it is refreshed after each call that may
    /// safepoint; between them, one test settles every managed access.
    phase: Option<PointerValue<'ctx>>,
    /// A deferred frame whose references may already be live in working
    /// slots: linking it copies them into the traced mirrors.
    late_roots: bool,
    /// (indirect frame slot, value-local interior address) pairs a late frame
    /// records when it is linked.
    late_indirect: Vec<(PointerValue<'ctx>, PointerValue<'ctx>)>,
    /// A masked frame: never zeroed; each safepoint call stores the
    /// descriptor for the traced locals in `scope` (see the prologue).
    masked: bool,
    /// The traced locals defined on the path being generated.
    scope: Vec<LocalId>,
    /// The scopes that reach each enclosing loop's exit and `Once`'s exit
    /// (parallel to `loops` and `onces`), met over every `break`/`leave`.
    loop_scopes: Vec<Option<Vec<LocalId>>>,
    once_scopes: Vec<Option<Vec<LocalId>>>,
    /// Locals whose slot another local also uses (see `codegen_slots`).
    shared_slot: Vec<bool>,
    /// Each local's frame slots, as mask bits (empty for untraced locals).
    local_bits: Vec<Vec<u32>>,
    /// (function name, root slots, indirect slots).
    frame_dims: (String, u32, u32, Option<PointerValue<'ctx>>),
    /// This function's call-site descriptors, by mask.
    site_origins: std::cell::RefCell<HashMap<Vec<u64>, PointerValue<'ctx>>>,
    /// This function's DWARF `DISubprogram` scope (debugger P2) — `Some` only when
    /// emitting debug info. Debug locations for the function's nodes use it as
    /// their scope (one function = one source, so scope/file stay consistent).
    subprogram: Option<DISubprogram<'ctx>>,
}

fn align_up64(n: u64, a: u64) -> u64 {
    if a == 0 { n } else { (n + a - 1) & !(a - 1) }
}

/// Extract a call's basic-value result (this inkwell fork returns a `ValueKind`).
fn call_result<'ctx>(cs: inkwell::values::CallSiteValue<'ctx>) -> BasicValueEnum<'ctx> {
    match cs.try_as_basic_value() {
        inkwell::values::ValueKind::Basic(v) => v,
        inkwell::values::ValueKind::Instruction(_) => panic!("call produced no value"),
    }
}

fn int_pred(op: BinOp, signed: bool) -> IntPredicate {
    use BinOp::*;
    match op {
        Eq => IntPredicate::EQ,
        Ne => IntPredicate::NE,
        Lt => {
            if signed { IntPredicate::SLT } else { IntPredicate::ULT }}
        Le => {
            if signed { IntPredicate::SLE } else { IntPredicate::ULE }}
        Gt => {
            if signed { IntPredicate::SGT } else { IntPredicate::UGT }}
        Ge => {
            if signed { IntPredicate::SGE } else { IntPredicate::UGE }},
        _ => unreachable!(),
    }
}

fn float_pred(op: BinOp) -> FloatPredicate {
    use BinOp::*;
    match op {
        Eq => FloatPredicate::OEQ,
        // IEEE inequality is true when either operand is NaN (unordered).
        Ne => FloatPredicate::UNE,
        Lt => FloatPredicate::OLT,
        Le => FloatPredicate::OLE,
        Gt => FloatPredicate::OGT,
        Ge => FloatPredicate::OGE,
        _ => unreachable!(),
    }
}

/// Run the standard LLVM optimization pipeline (`default<O3>`) over the module
/// in place: mem2reg, inlining, instcombine, GVN, loop opts, etc. This is where
/// monomorphized gc-rust code gets its speed.
fn optimize_module(module: &Module) -> Result<(), CodegenError> {
    use inkwell::OptimizationLevel;
    use inkwell::passes::PassBuilderOptions;
    use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};

    Target::initialize_native(&InitializationConfig::default()).map_err(|e| CodegenError(e.to_string()))?;
    let triple = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple).map_err(|e| CodegenError(e.to_string()))?;
    let machine = target.create_target_machine(
        &triple,
        &TargetMachine::get_host_cpu_name().to_string(),
        &TargetMachine::get_host_cpu_features().to_string(),
        OptimizationLevel::Aggressive,
        RelocMode::Default,
        CodeModel::Default,
    ).ok_or_else(|| CodegenError("cannot create optimization target machine".into()))?;
    let opts = PassBuilderOptions::create();
    // Canonicalize aggregate construction before IPSCCP builds a lattice for
    // every field of every intermediate insertvalue. Early scalar replacement
    // and combining retain the full O3 pipeline without quadratic wide-value
    // state for chains whose fields can already be simplified.
    // Match the bounded InstCombine settings in LLVM's standard O3 pipeline;
    // a single early sweep need not itself reach the final optimization fixpoint.
    module.run_passes("function(sroa,instcombine<max-iterations=1;no-verify-fixpoint>),default<O3>", &machine, opts)
        .map_err(|e| CodegenError(format!("LLVM optimization failed: {e}")))?;
    module.verify().map_err(|e| CodegenError(format!("optimized LLVM module verify failed: {e}")))
}

/// Collector shapes are separate from nominal Core layout IDs. Payload-free
/// enum constructions can use a header-only shape while ordinary variants use
/// the union shape. Both keep the same reflection name and variant metadata.
/// This single deterministic expansion is shared by codegen, JIT and AOT.
struct PhysicalLayouts {
    layouts: Vec<Layout>,
    empty_variants: HashMap<LayoutId, LayoutId>,
}

fn physical_layouts(prog: &CoreProgram) -> PhysicalLayouts {
    let mut result = PhysicalLayouts { layouts: prog.layouts.clone(), empty_variants: HashMap::new() };
    for (logical, layout) in prog.layouts.iter().enumerate() {
        let crate::gc::TypeKind::Enum { tag_offset, variants } = &layout.meta.kind else { continue ;
        };
        if *tag_offset as usize != crate::gc::Full::ENUM_TAG_OFFSET
            || (layout.ptr_fields == 0 && layout.raw_bytes == 0)
            || !matches!(layout.varlen, VarLen::None)
            || !variants.iter().any(|variant| variant.fields.is_empty())
        { continue; }
        // Exhausting the physical u16 namespace simply retains the valid union
        // representation; nominal IDs and program semantics never change.
        let Ok(physical) = u16::try_from(result.layouts.len()) else { break ;
        };
        let mut compact = layout.clone();
        compact.ptr_fields = 0;
        compact.raw_bytes = 0;
        compact.interior_ptrs.clear();
        compact.field_map.clear();
        result.layouts.push(compact);
        result.empty_variants.insert(logical as LayoutId, physical as LayoutId);
    }
    result
}

/// Convert the program's core [`Layout`]s into `gc::TypeInfo`s, one per
/// physical shape (index = `type_id`). Nominal layouts precede compact enum
/// shapes. This is the bridge to the collector: pointer
/// fields first (traced), then raw bytes, then any varlen tail.
pub fn layouts_to_type_infos(prog: &CoreProgram) -> Vec<crate::gc::TypeInfo> {
    use crate::gc::{Full, ObjHeader, TypeInfo};
    physical_layouts(prog).layouts
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let mut ti = TypeInfo::for_header(Full::SIZE)
                .with_type_id(i as u16)
                .with_fields(l.ptr_fields)
                .with_raw_bytes(l.raw_bytes);
            ti = match l.varlen {
                crate::core::VarLen::None => ti,
                crate::core::VarLen::Values => ti.with_varlen_values(l.ptr_fields),
                crate::core::VarLen::Bytes => ti.with_varlen_bytes(l.ptr_fields),
                crate::core::VarLen::Records => return ti.with_varlen_records(l.ptr_fields, l.elem_stride, l.interior_ptrs.clone()),
            };
            if !l.interior_ptrs.is_empty() {
                ti = ti.with_owned_interior_ptrs(l.interior_ptrs.clone());
            }
            ti
        })
        .collect()
}

/// Size and alignment (bytes) of a value-aggregate field's repr — mirrors
/// `layout::value_size_align`'s per-field rule so interior offsets match the LLVM
/// struct layout codegen actually uses.
fn value_field_size_align(values: &[crate::core::ValueLayout], f: &crate::core::Repr) -> (u32, u32) {
    use crate::core::Repr;
    match f {
        Repr::Unit => (0, 1),
        Repr::Scalar(s) => {
            let b = (s.bits().max(8) / 8).max(1);
            (b, b)
        }
        Repr::Ref(_) => (8, 8),
        Repr::Value(sub) => (values[*sub as usize].size, values[*sub as usize].align),
    }
}

/// Whether value type `vid` (transitively) holds a GC reference.
fn value_has_ref(values: &[crate::core::ValueLayout], vid: u32) -> bool {
    use crate::core::Repr;
    let vl = &values[vid as usize];
    let mut all: Vec<&Repr> = Vec::new();
    match &vl.variants {
        Some(variants) => variants.iter().for_each(|v| all.extend(v.fields.iter())),
        None => all.extend(vl.fields.iter()),
    }
    all.iter().any(|f| match f {
        Repr::Ref(_) => true,
        Repr::Value(s) => value_has_ref(values, *s),
        _ => false,
    })
}

/// Byte offsets (value-relative, shifted by `base`) of GC refs inside a flattened
/// value aggregate `vid`, recursing into nested value structs. Mirrors
/// `layout::value_ref_offsets`. A value **enum** reserves `max_ptrs` leading slots
/// shared across variants (pointers-first layout), so its embedded refs are at
/// fixed offsets `base, base+8, …` regardless of the active variant.
fn value_interior_offsets(values: &[crate::core::ValueLayout], vid: u32, base: u16, out: &mut Vec<u16>) {
    crate::layout::visit_value_ref_offsets(values, vid, base as u32, &mut |offset| out.push(offset))
        .expect("reference offsets must be validated during layout construction");
}

/// Collect the program's per-layout reflection metadata into a runtime
/// [`gc::TypeMeta`] table, one entry per physical shape (index = `type_id`). The
/// metadata is built during layout lowering (see `src/layout.rs`) and travels
/// inside each `Layout`; here we just clone it out and stamp the `type_id` from
/// its table position. Parallel to [`layouts_to_type_infos`].
pub fn layouts_to_type_meta(prog: &CoreProgram) -> Vec<crate::gc::TypeMeta> {
    physical_layouts(prog).layouts
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let mut m = l.meta.clone();
            m.type_id = i as u16;
            m
        })
        .collect()
}

/// Build reflection views for canonical inline values, including references
/// hoisted out of nested enum payloads. Reject unrepresentable metadata offsets.
pub fn layouts_to_value_meta(prog: &CoreProgram) -> Result<Vec<crate::gc::ValueMeta>, CodegenError> {
    use crate::core::Repr;
    use crate::gc::{FieldMeta, FieldTy, TypeKind, ValueMeta, VariantMeta};
    let error = || CodegenError("reflection value layout exceeds representable offset limit".into());
    let offset = |n: u32| u16::try_from(n).map_err(|_| error());
    let align_up = |off: u32, alignment: u32| off.checked_add(alignment - 1)
        .map(|n| n & !(alignment - 1)).ok_or_else(error);
    prog.values.iter().enumerate().map(|(vid, value)| {
        let kind = if let Some(variants) = &value.variants {
            let tag = crate::core::value_enum_max_ptrs(variants, &prog.values) as u32 * 8;
            let tag_offset = offset(tag)?;
            let raw_base = tag.checked_add(8).ok_or_else(error)?;
            let variants = variants.iter().enumerate().map(|(tag, variant)| {
                let mut pointer = 0u32;
                let mut raw = 0u32;
                let mut fields = Vec::new();
                for (index, repr) in variant.fields.iter().enumerate() {
                    let (at, ty) = match repr {
                        Repr::Unit => continue,
                        Repr::Ref(id) => {
                            let at = pointer * 8;
                            pointer += 1;
                            (at, FieldTy::Ref(u16::try_from(*id).map_err(|_| error())?))
                        }
                        Repr::Scalar(scalar) => {
                            let size = (scalar.bits().max(8) / 8).max(1);
                            raw = align_up(raw, size)?;
                            let at = raw_base.checked_add(raw).ok_or_else(error)?;
                            raw = raw.checked_add(size).ok_or_else(error)?;
                            (at, FieldTy::Scalar(crate::layout::scalar_kind(*scalar)))
                        }
                        Repr::Value(id) => {
                            let nested = &prog.values[*id as usize];
                            raw = align_up(raw, nested.align)?;
                            let at = raw_base.checked_add(raw).ok_or_else(error)?;
                            raw = raw.checked_add(nested.size).ok_or_else(error)?;
                            let mut interiors = Vec::new();
                            value_interior_offsets(&prog.values, *id, 0, &mut interiors);
                            let value_id = u16::try_from(*id).map_err(|_| error())?;
                            let ty = if interiors.is_empty() { FieldTy::Value(value_id) } else {
                                let reference_offsets = interiors.into_iter().map(|child| {
                                    let source = offset(pointer * 8)?;
                                    pointer += 1;
                                    Ok((child, source))
                                }).collect::<Result<Vec<_>, CodegenError>>()?;
                                FieldTy::ProjectedValue { value_id, reference_offsets }
                            };
                            (at, ty)
                        }
                    };
                    fields.push(FieldMeta { name: variant.field_names.get(index).cloned().unwrap_or_else(|| index.to_string()), offset: offset(at)?, ty });
                }
                Ok(VariantMeta { name: variant.name.clone(), tag: u32::try_from(tag).map_err(|_| error())?, fields })
            }).collect::<Result<Vec<_>, CodegenError>>()?;
            TypeKind::Enum { tag_offset, variants }
        } else {
            let mut cursor = 0u32;
            let mut fields = Vec::new();
            for (index, repr) in value.fields.iter().enumerate() {
                let (size, alignment) = value_field_size_align(&prog.values, repr);
                let ty = match repr {
                    Repr::Unit => continue,
                    Repr::Scalar(scalar) => FieldTy::Scalar(crate::layout::scalar_kind(*scalar)),
                    Repr::Ref(id) => FieldTy::Ref(u16::try_from(*id).map_err(|_| error())?),
                    Repr::Value(id) => FieldTy::Value(u16::try_from(*id).map_err(|_| error())?),
                };
                cursor = align_up(cursor, alignment)?;
                fields.push(FieldMeta { name: value.field_names.get(index).cloned().unwrap_or_else(|| index.to_string()), offset: offset(cursor)?, ty });
                cursor = cursor.checked_add(size).ok_or_else(error)?;
            }
            TypeKind::Struct { fields }
        };
        Ok(ValueMeta { value_id: u16::try_from(vid).map_err(|_| error())?, name: value.name.clone(), kind })
    }).collect()
}

/// Install the same runtime ABI mappings for every JIT execution context.
fn map_runtime_symbols(module: &Module<'_>, engine: &inkwell::execution_engine::ExecutionEngine<'_>) {
    use crate::runtime;
    // Map runtime externs to their Rust implementations.
    for (name, addr) in [
        ("ai_gc_alloc_fixed", runtime::ai_gc_alloc_fixed as *const () as usize),
        ("ai_gc_record_alloc", runtime::ai_gc_record_alloc as *const () as usize),
        ("ai_gc_alloc_varlen", runtime::ai_gc_alloc_varlen as *const () as usize),
        ("ai_gc_pollcheck_slow", runtime::ai_gc_pollcheck_slow as *const () as usize),
        ("ai_ffi_enter", runtime::ai_ffi_enter as *const () as usize),
        ("ai_ffi_leave", runtime::ai_ffi_leave as *const () as usize),
        ("ai_current_thread", runtime::ai_current_thread as *const () as usize),
        ("ai_ffi_reenter", runtime::ai_ffi_reenter as *const () as usize),
        ("ai_ffi_exit", runtime::ai_ffi_exit as *const () as usize),
        ("ai_print_int", runtime::ai_print_int as *const () as usize),
        ("ai_print_float", runtime::ai_print_float as *const () as usize),
        ("ai_print_str", runtime::ai_print_str as *const () as usize),
        ("ai_print_str_raw", runtime::ai_print_str_raw as *const () as usize),
        ("ai_str_len", runtime::ai_str_len as *const () as usize),
        ("ai_str_eq", runtime::ai_str_eq as *const () as usize),
        ("ai_str_eq_v2", runtime::ai_str_eq_v2 as *const () as usize),
        ("ai_str_concat", runtime::ai_str_concat as *const () as usize),
        ("ai_panic", runtime::ai_panic as *const () as usize),
        ("ai_str_join", runtime::ai_str_join as *const () as usize),
        ("ai_host_call", runtime::io::ai_host_call as *const () as usize),
        ("ai_str_get", runtime::ai_str_get as *const () as usize),
        ("ai_str_substring", runtime::ai_str_substring as *const () as usize),
        ("ai_read_file", runtime::ai_read_file as *const () as usize),
        ("ai_str_from_int", runtime::ai_str_from_int as *const () as usize),
        ("ai_str_from_float", runtime::ai_str_from_float as *const () as usize),
        ("ai_fmt_f64", runtime::ai_fmt_f64 as *const () as usize),
        ("ai_str_from_bytes", runtime::ai_str_from_bytes as *const () as usize),
        ("ai_str_from_utf8_prefix", runtime::ai_str_from_utf8_prefix as *const () as usize),
        ("ai_str_copy_bytes", runtime::ai_str_copy_bytes as *const () as usize),
        ("ai_array_copy", runtime::ai_array_copy as *const () as usize),
        ("ai_str_eq_bytes", runtime::ai_str_eq_bytes as *const () as usize),
        ("ai_char_to_str", runtime::ai_char_to_str as *const () as usize),
        ("ai_type_name", runtime::ai_type_name as *const () as usize),
        ("ai_heap_snapshot", runtime::ai_heap_snapshot as *const () as usize),
        ("ai_reflect_field_count", runtime::ai_reflect_field_count as *const () as usize),
        ("ai_monotonic_nanos", runtime::ai_monotonic_nanos as *const () as usize),
        ("ai_thread_mark_daemon", runtime::ai_thread_mark_daemon as *const () as usize),
        ("ai_reflect_field_kind", runtime::ai_reflect_field_kind as *const () as usize),
        ("ai_reflect_field_i64", runtime::ai_reflect_field_i64 as *const () as usize),
        ("ai_reflect_field_name", runtime::ai_reflect_field_name as *const () as usize),
        ("ai_str_to_float", runtime::ai_str_to_float as *const () as usize),
        ("ai_str_to_float_v2", runtime::ai_str_to_float_v2 as *const () as usize),
        ("ai_str_hash", runtime::ai_str_hash as *const () as usize),
        ("ai_str_hash_v2", runtime::ai_str_hash_v2 as *const () as usize),
        ("ai_ffi_buffer_alloc_v1", runtime::ai_ffi_buffer_alloc_v1 as *const () as usize),
        ("ai_ffi_buffer_free_v1", runtime::ai_ffi_buffer_free_v1 as *const () as usize),
        ("ai_ffi_buffer_alloc_v2", runtime::ai_ffi_buffer_alloc_v2 as *const () as usize),
        ("ai_ffi_buffer_free_v2", runtime::ai_ffi_buffer_free_v2 as *const () as usize),
        ("ai_ffi_buffer_data_v2", runtime::ai_ffi_buffer_data_v2 as *const () as usize),
        ("ai_str_copy_to_buf", runtime::ai_str_copy_to_buf as *const () as usize),
        ("ai_str_copy_to_buf_v2", runtime::ai_str_copy_to_buf_v2 as *const () as usize),
        ("ai_managed_lock", runtime::ai_managed_lock as *const () as usize),
        ("ai_managed_read", runtime::ai_managed_read as *const () as usize),
        ("ai_managed_try_write", runtime::ai_managed_try_write as *const () as usize),
        ("ai_managed_unlock", runtime::ai_managed_unlock as *const () as usize),
        ("ai_buf_copy_in", runtime::ai_buf_copy_in as *const () as usize),
        ("ai_buf_copy_in_v2", runtime::ai_buf_copy_in_v2 as *const () as usize),
        ("ai_buf_copy_out", runtime::ai_buf_copy_out as *const () as usize),
        ("ai_buf_copy_out_v2", runtime::ai_buf_copy_out_v2 as *const () as usize),
        ("ai_thread_spawn", runtime::ai_thread_spawn as *const () as usize),
        ("ai_thread_join", runtime::ai_thread_join as *const () as usize),
        ("ai_thread_bind_owner", runtime::ai_thread_bind_owner as *const () as usize),
        ("ai_atomic_i64_bind_owner", runtime::ai_atomic_i64_bind_owner as *const () as usize),
        ("ai_thread_sleep", runtime::ai_thread_sleep as *const () as usize),
        ("ai_thread_yield", runtime::ai_thread_yield as *const () as usize),
        ("ai_thread_current_id", runtime::ai_thread_current_id as *const () as usize),
        ("ai_statics_install", runtime::ai_statics_install as *const () as usize),
        ("ai_statics_table", runtime::ai_statics_table as *const () as usize),
        ("ai_closure_code_ptr", runtime::ai_closure_code_ptr as *const () as usize),
        ("ai_object_type_id", runtime::ai_object_type_id as *const () as usize),
        ("ai_chan_new", runtime::ai_chan_new as *const () as usize),
        ("ai_chan_bind_owner", runtime::ai_chan_bind_owner as *const () as usize),
        ("ai_chan_close", runtime::ai_chan_close as *const () as usize),
        ("ai_chan_waiting_senders", runtime::ai_chan_waiting_senders as *const () as usize),
        ("ai_chan_waiting_receivers", runtime::ai_chan_waiting_receivers as *const () as usize),
        ("ai_chan_send", runtime::ai_chan_send as *const () as usize),
        ("ai_chan_recv", runtime::ai_chan_recv as *const () as usize),
        ("ai_chan_sender_clone", runtime::ai_chan_sender_clone as *const () as usize),
        ("ai_chan_sender_drop", runtime::ai_chan_sender_drop as *const () as usize),
        ("ai_atomic_i64_new", runtime::ai_atomic_i64_new as *const () as usize),
        ("ai_atomic_i64_load", runtime::ai_atomic_i64_load as *const () as usize),
        ("ai_atomic_i64_store", runtime::ai_atomic_i64_store as *const () as usize),
        ("ai_atomic_i64_fetch_add", runtime::ai_atomic_i64_fetch_add as *const () as usize),
        ("ai_atomic_i64_compare_and_set", runtime::ai_atomic_i64_compare_and_set as *const () as usize),
        ("ai_gc_write_barrier", runtime::ai_gc_write_barrier as *const () as usize),
        ("ai_atom_ref_load", runtime::ai_atom_ref_load as *const () as usize),
        ("ai_atom_ref_cas", runtime::ai_atom_ref_cas as *const () as usize),
        ("ai_slot_bits8_load", runtime::ai_slot_bits8_load as *const () as usize),
        ("ai_slot_bits8_store", runtime::ai_slot_bits8_store as *const () as usize),
        ("ai_slot_bits16_load", runtime::ai_slot_bits16_load as *const () as usize),
        ("ai_slot_bits16_store", runtime::ai_slot_bits16_store as *const () as usize),
        ("ai_slot_bits32_load", runtime::ai_slot_bits32_load as *const () as usize),
        ("ai_slot_bits32_store", runtime::ai_slot_bits32_store as *const () as usize),
        ("ai_slot_bits64_load", runtime::ai_slot_bits64_load as *const () as usize),
        ("ai_slot_bits64_store", runtime::ai_slot_bits64_store as *const () as usize),
        ("ai_slot_ref_store", runtime::ai_slot_ref_store as *const () as usize),
        ("ai_atom_bits8_load", runtime::ai_atom_bits8_load as *const () as usize),
        ("ai_atom_bits8_cas", runtime::ai_atom_bits8_cas as *const () as usize),
        ("ai_atom_bits16_load", runtime::ai_atom_bits16_load as *const () as usize),
        ("ai_atom_bits16_cas", runtime::ai_atom_bits16_cas as *const () as usize),
        ("ai_atom_bits32_load", runtime::ai_atom_bits32_load as *const () as usize),
        ("ai_atom_bits32_cas", runtime::ai_atom_bits32_cas as *const () as usize),
        ("ai_atom_bits64_load", runtime::ai_atom_bits64_load as *const () as usize),
        ("ai_atom_bits64_cas", runtime::ai_atom_bits64_cas as *const () as usize),
        ("ai_bounds_fail", runtime::ai_bounds_fail as *const () as usize),
        ("ai_uninitialized_array_fail", runtime::ai_uninitialized_array_fail as *const () as usize),
        ("ai_array_allocation_len", runtime::ai_array_allocation_len as *const () as usize),
        ("ai_arithmetic_fail", runtime::ai_arithmetic_fail as *const () as usize),
    ] {
        if let Some(f) = module.get_function(name) {
            engine.add_global_mapping(&f, addr);
        }
    }

}

/// JIT-compile and run a 0-arg `-> i64` entry with a real GC runtime, returning
/// its result. Sets up a [`RuntimeContext`] over the program's layouts, wires
/// the `ai_gc_*` externs, and passes a live `Thread*`. `stress` forces a
/// collection on every allocation (exercises the GC + precise roots).
/// How the JIT driver configures the GC heap. Lets tests force frequent
/// collections (a tiny semi-space that fills during construction) to exercise
/// precise rooting and relocation without the (multithread-unsafe) every-alloc
/// stress mode.
#[derive(Clone, Copy)]
pub enum GcRunMode {
    /// Production default: generational nursery over tenured.
    Generational,
    /// `--gc-stress`: semi-space + collect on every allocation (single-thread aid).
    Stress,
    /// Semi-space of the given byte size; collects when it fills. A small size
    /// triggers real collections during a program's own construction.
    SemiSpace(usize),
}

///
/// # Safety
/// The program must preserve the compiler's typed Core/GC/ABI invariants. Any
/// source-level unsafe operations and foreign functions must satisfy their
/// runtime, pointer and callback-lifetime contracts. Execution invokes native
/// machine code in this process; manually constructed Core is not validated.
pub unsafe fn jit_run_i64_gc(prog: &CoreProgram, stress: bool) -> Result<i64, CodegenError> {
    unsafe { jit_run_i64_mode(prog, if stress { GcRunMode::Stress } else { GcRunMode::Generational }) }
}

/// JIT-run with an explicit GC heap mode (see [`GcRunMode`]).
///
/// # Safety
/// The program must preserve the compiler's typed Core/GC/ABI invariants. Any
/// source-level unsafe operations and foreign functions must satisfy their
/// runtime, pointer and callback-lifetime contracts. Execution invokes native
/// machine code in this process; manually constructed Core is not validated.
pub unsafe fn jit_run_i64_mode(prog: &CoreProgram, mode: GcRunMode) -> Result<i64, CodegenError> {
    unsafe { jit_run_i64_with_args(prog, mode, std::env::args_os().collect()) }
}

static EXIT_WITH_MAIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// JIT runs are the whole process (the `gcr run` driver): when `main`
/// returns while daemon threads (`std::thread::spawn`) still run, the
/// process ends with `main`'s result rather than waiting for them.
pub fn set_exit_with_main(on: bool) {
    EXIT_WITH_MAIN.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Execute with independent argv; children inherit this execution's arguments.
///
/// # Safety
/// The program must preserve the compiler's typed Core/GC/ABI invariants. Any
/// source-level unsafe operations and foreign functions must satisfy their
/// runtime, pointer and callback-lifetime contracts. Execution invokes native
/// machine code in this process; manually constructed Core is not validated.
pub unsafe fn jit_run_i64_with_args(
    prog: &CoreProgram,
    mode: GcRunMode,
    arguments: Vec<std::ffi::OsString>,
) -> Result<i64, CodegenError> {
    use crate::runtime::{self, RuntimeContext};
    let ctx = Context::create();
    let compiled = codegen(&ctx, prog)?;
    // Optimize the module (mem2reg + inlining + the standard O3 pipeline) so the
    // monomorphized, GC-framed code is actually fast. Without this the JIT runs
    // naive IR (every local a stack slot).
    optimize_module(&compiled.module)?;
    let ee = compiled
        .module
        .create_jit_execution_engine(OptimizationLevel::Aggressive)
        .map_err(|e| CodegenError(e.to_string()))?;

    map_runtime_symbols(&compiled.module, &ee);

    let tis = layouts_to_type_infos(prog);
    // Normal runs use the GENERATIONAL heap: a small nursery (collected cheaply
    // and often by minor GCs) over a large tenured generation.
    //
    // `--gc-stress` instead uses the single-generation SEMI-SPACE heap with
    // collect-on-every-alloc. That mode's invariant checks (e.g. "every root
    // points to from-space after a flip") are semi-space-specific and don't hold
    // for a nursery, so stress validates relocation correctness on the
    // semi-space collector, the collector it was designed to torture-test.
    let mut rt = match mode {
        GcRunMode::Stress => {
            let rt = RuntimeContext::new(runtime::configured_stress_heap_size(), tis);
            rt.heap().set_gc_every_alloc(true);
            rt
        }
        GcRunMode::SemiSpace(size) => RuntimeContext::new(size, tis),
        GcRunMode::Generational => {
            // Shared with the AOT runtime so JIT and AOT agree (was 16 MB here vs
            // 1 MB in gcr_runtime_main — same program, different GC by run mode).
            let (nursery, tenured) = runtime::configured_heap_sizes();
            RuntimeContext::new_generational(nursery, tenured, tis)
        }
    };
    rt.set_arguments(arguments);
    // Install reflection metadata so heap-exploration tooling and in-language
    // reflection can recover type/field names (the GC type table is nameless).
    rt.heap().set_type_meta(layouts_to_type_meta(prog));
    rt.heap().set_value_meta(layouts_to_value_meta(prog)?);
    // Install the allocation-site table (Target-1b) so GCR_ALLOC_PROFILE can
    // name each site's (function, type).
    rt.heap().set_alloc_sites(compiled.alloc_sites.clone());
    if let Some(path) = std::env::var_os("GCR_GC_LOG") {
        rt.heap().start_gc_log(&path)
            .map_err(|error| CodegenError(format!("cannot create GC log {:?}: {error}", path)))?;
    }

    let addr = ee
        .get_function_address(&compiled.entry_name)
        .map_err(|_| CodegenError("entry not found".into()))?;
    type EntryFn = unsafe extern "C" fn(*mut runtime::Thread) -> i64;
    let f: EntryFn = unsafe { std::mem::transmute(addr) };
    let thread = rt.thread_ptr();
    // Publish the current thread so FFI callback trampolines can recover it.
    let result = rt.enter(|thread| unsafe { f(thread) });
    unsafe {
        // A program run as a process (`gcr run`) ends with its `main`, as in
        // Rust, even while daemon threads run; an embedding waits for them.
        if EXIT_WITH_MAIN.load(std::sync::atomic::Ordering::Relaxed) {
            rt.exit_if_daemons_remain(result as i32);
        }
        rt.finish_threads();
    }
    // Opt-in heap dump once the program returns (heap quiescent).
    // `GCR_HEAP_DUMP=json` emits a structured snapshot; other values emit text.
    if let Some(mode) = std::env::var_os("GCR_HEAP_DUMP") {
        let dump = if mode == "json" {
            unsafe { crate::gc::dump_heap_json(rt.heap()) }
        } else {
            unsafe { crate::gc::dump_heap_text(rt.heap()) }
        };
        // `GCR_HEAP_DUMP_FILE` redirects the dump to a file instead of stderr, so
        // it is never interleaved with the program's own output — the clean data
        // source for `gcr heap` and the heap-explorer widget.
        if let Some(path) = std::env::var_os("GCR_HEAP_DUMP_FILE") {
            if let Err(e) = std::fs::write(&path, &dump) {
                let message = format!("gc-rust: cannot write heap dump to {:?}: {e}\n", path);
                unsafe { runtime::write_diagnostic(thread, message.as_bytes()); }
            }
        } else {
            unsafe { runtime::write_diagnostic(thread, dump.as_bytes()); }
        }
    }
    // Opt-in GC stats (set GCR_GC_STATS=1) — useful to confirm the generational
    // split is working: minor collections should dominate on young-heavy loads.
    if std::env::var("GCR_GC_STATS").is_ok() {
        let summary = format!("gc-rust: {} minor + {} major collections\n{}", rt.heap().minor_collections(), rt.heap().collections(), rt.heap().gc_stats_summary());
        unsafe { runtime::write_diagnostic(thread, summary.as_bytes()); }
    }
    // Opt-in allocation-site profile (Target-1b): GCR_ALLOC_PROFILE=1 prints the
    // per-site count+bytes table at program end (heap quiescent).
    if std::env::var_os("GCR_ALLOC_PROFILE").is_some() {
        let report = rt.heap().alloc_site_profile_report();
        unsafe { runtime::write_diagnostic(thread, report.as_bytes()); }
    }
    // `gcr bench` metrics sink: GCR_METRICS_FILE=<path> writes the machine-readable
    // per-run metrics (GC cycles/pauses, allocation churn, peak heap) as JSON.
    let metrics_path = std::env::var_os("GCR_METRICS_FILE");
    runtime::finish_gc_telemetry(rt.heap(), metrics_path.as_deref().map(std::path::Path::new))
        .map_err(CodegenError)?;
    Ok(result)
}

/// JIT-compile and run a 0-arg `-> i64` entry. Uses the GC runtime so heap
/// programs work; allocation-free programs still run (null-frame fast path).
///
/// # Safety
/// The program must preserve the compiler's typed Core/GC/ABI invariants. Any
/// source-level unsafe operations and foreign functions must satisfy their
/// runtime, pointer and callback-lifetime contracts. Execution invokes native
/// machine code in this process; manually constructed Core is not validated.
pub unsafe fn jit_run_i64(prog: &CoreProgram) -> Result<i64, CodegenError> {
    unsafe { jit_run_i64_gc(prog, false) }
}

// =============================================================================
// AOT (ahead-of-time) compilation: emit a native object + link an executable
// =============================================================================

/// Encode the program's layouts as `AotLayout` source records (the same data
/// the JIT path derives `TypeInfo`s from). `varlen`: 0=None, 1=Values, 2=Bytes.
/// The runtime's `gcr_runtime_main` rebuilds the `TypeInfo` table from these.
fn layouts_to_aot_records(prog: &CoreProgram) -> Vec<(u16, u16, u8, u16)> {
    physical_layouts(prog).layouts
        .iter()
        .map(|l| {
            let varlen = match l.varlen {
                crate::core::VarLen::None => 0u8,
                crate::core::VarLen::Values => 1u8,
                crate::core::VarLen::Bytes => 2u8,
                crate::core::VarLen::Records => 3u8,
            };
            let stride = if matches!(l.varlen, crate::core::VarLen::Records) { l.elem_stride } else { 0 };
            (l.ptr_fields, l.raw_bytes, varlen, stride)
        })
        .collect()
}

/// Build a `TargetMachine` configured for the host (mirrors `optimize_module`'s
/// setup) for object emission.
fn host_target_machine() -> Result<inkwell::targets::TargetMachine, CodegenError> {
    host_target_machine_opt(OptimizationLevel::Aggressive)
}

/// Build the host [`TargetMachine`] at a given backend optimization level. The
/// level matters for debug builds: even with the IR-level `optimize_module`
/// skipped, an `Aggressive` backend promotes allocas into registers and the
/// `dbg.declare` frame-slot locations go stale. Full-debug (P3) passes `None`
/// so locals stay in their stack slots and `frame variable` reads them.
fn host_target_machine_opt(
    opt: OptimizationLevel,
) -> Result<inkwell::targets::TargetMachine, CodegenError> {
    use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};
    Target::initialize_native(&InitializationConfig::default())
        .map_err(|e| CodegenError(format!("target init failed: {}", e)))?;
    let triple = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple)
        .map_err(|e| CodegenError(format!("no target for {}: {}", triple, e.to_string())))?;
    // PIC reloc + Default code model so the object links cleanly into a normal
    // (PIE) executable produced by `cc`.
    target
        .create_target_machine(
            &triple,
            &TargetMachine::get_host_cpu_name().to_string(),
            &TargetMachine::get_host_cpu_features().to_string(),
            opt,
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| CodegenError("could not create target machine".into()))
}

/// Compile `prog` to a native object file at `obj_path`.
///
/// The compiled program entry is renamed to `gcrust_entry` and a native
/// `int main()` is emitted that calls `gcr_runtime_main(&TYPE_TABLE, N,
/// gcrust_entry)` (which sets up the GC `RuntimeContext` and runs the program)
/// and returns the program's `i64` result truncated to the process exit code
/// (`i32`, so the shell sees it modulo 256). The per-program layout table is
/// emitted as an `AotLayout[]` global constant so the runtime can rebuild the
/// GC `TypeInfo` table at startup.
pub fn codegen_aot_object(
    prog: &CoreProgram,
    obj_path: &std::path::Path,
) -> Result<(), CodegenError> {
    codegen_aot_object_level(prog, obj_path, DebugLevel::LineTables)
}

/// Like [`codegen_aot_object`] but with an explicit [`DebugLevel`].
/// [`DebugLevel::Full`] (debugger P3) emits local-variable DIEs **and skips
/// optimization** — locals/allocas must survive for `frame variable` to read
/// them. The default ([`DebugLevel::LineTables`], P2) keeps the O3 pipeline;
/// LLVM preserves line debug-locations through it, so stepping still works.
pub fn codegen_aot_object_level(
    prog: &CoreProgram,
    obj_path: &std::path::Path,
    level: DebugLevel,
) -> Result<(), CodegenError> {
    use inkwell::targets::FileType;

    let ctx = Context::create();
    // AOT: emit DWARF (debugger P2/P3) — it lands in the object where
    // lldb / llvm-dwarfdump read it.
    let compiled = codegen_with_debug(&ctx, prog, level)?;
    let module = &compiled.module;

    // ---- Rename the program entry to a stable symbol ----------------------
    // It is currently named `prog.funcs[entry].name` (e.g. "main"); rename it so
    // the native `main` we emit below doesn't collide and the runtime can call a
    // known symbol.
    let entry_fn = module
        .get_function(&compiled.entry_name)
        .ok_or_else(|| {
        CodegenError(format!("entry `{}` not found in module", compiled.entry_name))})?;
    entry_fn.as_global_value().set_name("gcrust_entry");

    let i16t = ctx.i16_type();
    let i8t = ctx.i8_type();
    let i32t = ctx.i32_type();
    let i64t = ctx.i64_type();
    let ptr = ctx.ptr_type(AddressSpace::default());

    // ---- Emit the layout table as an `AotLayout[]` constant ---------------
    // AotLayout (runtime, #[repr(C)]): { u16 ptr_fields, u16 raw_bytes,
    // u8 varlen, [3 x u8] pad } — size 8, align 2. The LLVM struct mirrors it.
    let aot_ty = ctx.struct_type(
        &[
            i16t.into(),
            i16t.into(),
            i8t.into(),
            i8t.array_type(3).into(),
        ],
        false,
    );
    let records = layouts_to_aot_records(prog);
    let elems: Vec<_> = records
        .iter()
        .map(|(pf, rb, vl, stride)| {
            // A record array's stride rides in the first two pad bytes.
            let [lo, hi] = stride.to_le_bytes();
            let pad = i8t.const_array(&[i8t.const_int(lo as u64, false), i8t.const_int(hi as u64, false), i8t.const_zero()]);
            aot_ty.const_named_struct(&[
                i16t.const_int(*pf as u64, false).into(),
                i16t.const_int(*rb as u64, false).into(),
                i8t.const_int(*vl as u64, false).into(),
                pad.into(),
            ])
        })
        .collect();
    let table_arr = aot_ty.const_array(&elems);
    let table_global = module.add_global(
        aot_ty.array_type(records.len() as u32),
        None,
        "gcrust_type_table",
    );
    table_global.set_constant(true);
    table_global.set_initializer(&table_arr);

    // ---- Emit the reflection metadata blob as an i8[] constant ------------
    // Encoded by `gc::reflect::encode`; decoded by `gcr_runtime_main` at startup
    // into the heap's TypeMeta table (type/field names + field types). Nameless
    // programs (no reflection) still get a valid empty-table blob.
    let interior: Vec<Vec<u16>> = physical_layouts(prog).layouts.iter().map(|l| l.interior_ptrs.clone()).collect();
    let meta_bytes = crate::gc::reflect::encode(
        &layouts_to_type_meta(prog),
        &layouts_to_value_meta(prog)?,
        &interior,
        &compiled.alloc_sites,
    ).map_err(|error| CodegenError(error.to_string()))?;
    let meta_vals: Vec<_> = meta_bytes
        .iter()
        .map(|b| i8t.const_int(*b as u64, false))
        .collect();
    let meta_arr = i8t.const_array(&meta_vals);
    let meta_global = module.add_global(
        i8t.array_type(meta_bytes.len() as u32),
        None,
        "gcrust_type_meta",
    );
    meta_global.set_constant(true);
    meta_global.set_initializer(&meta_arr);

    // ---- Declare gcr_runtime_main -----------------------------------------
    // i64 gcr_runtime_main(ptr layouts, i64 ti_count, ptr meta, i64 meta_len,
    //                      ptr entry)
    let runtime_main_ty = i64t.fn_type(
        &[ptr.into(), i64t.into(), ptr.into(), i64t.into(), ptr.into()],
        false,
    );
    let runtime_main = module.add_function(
        "gcr_runtime_process_main_v4",
        runtime_main_ty,
        Some(inkwell::module::Linkage::External),
    );

    // ---- Emit native `int main()` -----------------------------------------
    let main_ty = i32t.fn_type(&[], false);
    let main_fn = module.add_function("main", main_ty, None);
    let builder = ctx.create_builder();
    let bb = ctx.append_basic_block(main_fn, "entry");
    builder.position_at_end(bb);
    let entry_ptr = entry_fn.as_global_value().as_pointer_value();
    let call = builder
        .build_call(
            runtime_main,
            &[
                table_global.as_pointer_value().into(),
                i64t.const_int(records.len() as u64, false).into(),
                meta_global.as_pointer_value().into(),
                i64t.const_int(meta_bytes.len() as u64, false).into(),
                entry_ptr.into(),
            ],
            "rc",
        )
        .unwrap();
    let result = call_result(call).into_int_value();
    // Truncate the i64 program result to the process exit code (i32).
    let code = builder.build_int_truncate(result, i32t, "code").unwrap();
    builder.build_return(Some(&code)).unwrap();

    module
        .verify()
        .map_err(|e| CodegenError(format!("AOT module verify failed: {}", e.to_string())))?;

    // ---- Optimize then emit the object ------------------------------------
    // Full-debug builds (P3) skip optimization so locals/allocas survive for
    // `frame variable`. Line-table builds (P2) optimize as usual — debug
    // locations survive the O3 pipeline, so stepping is unaffected.
    // Full-debug builds (P3) keep the backend at `None` too: an optimizing
    // backend promotes the (still-present) allocas into registers, leaving the
    // `dbg.declare` frame-slot locations stale.
    let machine = if level.is_full() {
        host_target_machine_opt(OptimizationLevel::None)?
    } else {
        optimize_module(module)?;
        host_target_machine()?
    };
    if std::env::var_os("GCR_CODEGEN_TSAN").is_some_and(|v| v != "0" && !v.is_empty()) {
        use inkwell::attributes::{Attribute, AttributeLoc};
        use inkwell::passes::PassBuilderOptions;
        let kind = Attribute::get_named_enum_kind_id("sanitize_thread");
        for function in module.get_functions() {
            if function.count_basic_blocks() != 0 {
                function.add_attribute(AttributeLoc::Function, ctx.create_enum_attribute(kind, 0));
            }
        }
        module.run_passes("tsan-module,function(tsan)", &machine, PassBuilderOptions::create())
            .map_err(|e| CodegenError(format!("ThreadSanitizer instrumentation failed: {e}")))?;
        module.verify().map_err(|e| CodegenError(format!("instrumented AOT module verify failed: {e}")))?;
    }
    machine
        .write_to_file(module, FileType::Object, obj_path)
        .map_err(|e| CodegenError(format!("object emission failed: {}", e.to_string())))?;
    Ok(())
}

/// AOT-compile `prog` into a standalone native executable at `out_path`.
///
/// Emits a native object (via [`codegen_aot_object`]) into a temp file, then
/// links it against the gc-rust runtime staticlib (`libgcrust_rt.a`, built by
/// cargo as a `staticlib` crate type) using the system `cc`. The runtime
/// provides the `ai_gc_*` / `ai_print_*` externs and `gcr_runtime_main`.
pub fn build_executable(
    prog: &CoreProgram,
    out_path: &std::path::Path,
    extra_link_args: &[String],
) -> Result<(), CodegenError> {
    build_executable_level(prog, out_path, extra_link_args, DebugLevel::LineTables)
}

/// Like [`build_executable`] but with an explicit [`DebugLevel`].
/// [`DebugLevel::Full`] is `gcr build --debug` (debugger P3: unoptimized +
/// local-variable DIEs so `lldb`'s `frame variable` shows source names/values).
pub fn build_executable_level(
    prog: &CoreProgram,
    out_path: &std::path::Path,
    extra_link_args: &[String],
    level: DebugLevel,
) -> Result<(), CodegenError> {
    // Emit the object next to the output so cleanup is easy and paths are stable.
    let obj_path = out_path.with_extension("o");
    codegen_aot_object_level(prog, &obj_path, level)?;

    let staticlib = locate_runtime_staticlib()?;

    // Link: object + runtime staticlib + system libs. The runtime is Rust, so it
    // needs libpthread/libdl/libm/libc + (on glibc) libgcc_s for unwinding. `cc`
    // pulls libc; we add the rest explicitly. `extra_link_args` (from `gcr build
    // --link-arg …`) is appended for FFI programs — notably the self-hosting
    // compiler, which links libLLVM (`-L<llvm>/lib -lLLVM -Wl,-rpath,…`).
    // Treat CC as one executable path, preserving non-UTF-8 paths and spaces.
    // Arguments remain separate process arguments, never shell input.
    let linker_program = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let mut linker = std::process::Command::new(&linker_program);
    if std::env::var_os("GCR_CODEGEN_TSAN").is_some_and(|v| v != "0" && !v.is_empty()) {
        linker.arg("-fsanitize=thread");
    }
    // An executable exports only `main` (gc-rust has no exported functions),
    // and unreferenced runtime code is dropped: a small export trie and image
    // keep `dyld`'s work at launch small.
    #[cfg(target_os = "macos")]
    linker.args(["-Wl,-dead_strip", "-Wl,-exported_symbol,_main"]);
    let status = linker
        .arg("-o")
        .arg(out_path)
        .arg(&obj_path)
        .arg(&staticlib.path)
        .args(["-lpthread", "-ldl", "-lm"])
        .args(extra_link_args)
        .status()
        .map_err(|e| CodegenError(format!("failed to invoke linker ({}): {}", linker_program.to_string_lossy(), e)))?;
    if !status.success() {
        return Err(CodegenError(format!(
            "linker ({}) failed with status {}",
            linker_program.to_string_lossy(), status
        )));
    }

    // On macOS, DWARF stays in the `.o` (the linked executable only holds a debug
    // MAP referencing it). Run `dsymutil` BEFORE we delete the `.o` to collect a
    // `.dSYM` bundle next to the executable, so `lldb` resolves gc-rust source
    // lines (debugger P2). Best-effort: if dsymutil is absent the binary still
    // runs, just without source-level debug. On ELF, DWARF is in the executable
    // already — no dSYM step.
    #[cfg(target_os = "macos")]
    {
        let ran = std::process::Command::new("dsymutil").arg(out_path).status();
        if ran.is_err() {
            let _ = std::process::Command::new("xcrun")
                .args(["dsymutil"])
                .arg(out_path)
                .status();
        }
    }

    // Remove the intermediate object on success.
    let _ = std::fs::remove_file(&obj_path);
    Ok(())
}

/// Keep ownership of the development archive through linking and debug-symbol
/// extraction. Cargo's own target lock ends when its subprocess exits; another
/// native build must not replace the archive while a linker is reading it.
struct RuntimeArchive {
    path: std::path::PathBuf,
    _lock: Option<std::fs::File>,
}

fn locate_runtime_staticlib() -> Result<RuntimeArchive, CodegenError> {
    use std::path::PathBuf;
    if let Some(path) = std::env::var_os("GCRUST_RUNTIME_LIB") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(CodegenError(format!(
                "GCRUST_RUNTIME_LIB={} is not a file",
                path.display()
            )));
        }
        // Explicit archives (including sanitizer/packaged builds) are owned by
        // the caller. No development build or implicit replacement is performed.
        return Ok(RuntimeArchive { path, _lock: None });
    }
    let baked = PathBuf::from(env!("GCRUST_RT_STATICLIB"));
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if !manifest.join("crates/gcrust-rt/Cargo.toml").is_file() {
        if baked.is_file() {
            return Ok(RuntimeArchive {
                path: baked,
                _lock: None,
            });
        }
        return Err(CodegenError(format!(
            "gc-rust runtime archive is missing at {}; set GCRUST_RUNTIME_LIB to the packaged archive",
            baked.display()
        )));
    }

    // An AOT-owned Cargo target prevents ordinary workspace/dependency builds
    // from replacing our archive. Always ask Cargo to check all inputs rather
    // than guessing freshness from mtimes (which misses lockfiles/configuration).
    let (cache, profile, target) = runtime_cache_location(&baked)?;
    std::fs::create_dir_all(&cache).map_err(|e| {
        CodegenError(format!(
            "cannot create runtime cache {}: {e}",
            cache.display()
        ))
    })?;
    let lock_path = cache.join("archive.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| {
            CodegenError(format!(
                "cannot open runtime archive lock {}: {e}",
                lock_path.display()
            ))
        })?;
    lock.lock().map_err(|e| {
        CodegenError(format!(
            "cannot lock runtime archive {}: {e}",
            lock_path.display()
        ))
    })?;
    let mut command =
        std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(manifest)
        .args([
            "build",
            "--locked",
            "-p",
            "gcrust-rt",
            "--profile",
            if profile == "debug" { "dev" } else { &profile },
            "--target-dir",
        ])
        .arg(&cache);
    if let Some(target) = &target {
        command.arg("--target").arg(target);
    }
    let output = command
        .output()
        .map_err(|e| CodegenError(format!("cannot build native runtime: {e}")))?;
    if !output.status.success() {
        return Err(CodegenError(format!(
            "native runtime build failed with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let root = target
        .as_ref()
        .map_or_else(|| cache.clone(), |target| cache.join(target));
    let path = root.join(profile).join("libgcrust_rt.a");
    if !path.is_file() {
        return Err(CodegenError(format!(
            "Cargo did not produce runtime archive {}",
            path.display()
        )));
    }
    Ok(RuntimeArchive {
        path,
        _lock: Some(lock),
    })
}

/// Preserve custom target directories, explicit triples, and profile names from
/// the compiler's baked archive path. The explicit target-dir overrides a caller
/// CARGO_TARGET_DIR, so the producer and consumer always agree on the path.
fn runtime_cache_location(
    baked: &std::path::Path,
) -> Result<(std::path::PathBuf, String, Option<String>), CodegenError> {
    let profile_dir = baked
        .parent()
        .ok_or_else(|| CodegenError("runtime archive has no profile directory".into()))?;
    let profile = profile_dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| CodegenError("runtime archive has no valid profile name".into()))?
        .to_owned();
    let parent = profile_dir
        .parent()
        .ok_or_else(|| CodegenError("runtime archive has no target directory".into()))?;
    // TARGET is exported by build.rs; matching the actual directory avoids
    // guessing whether an arbitrary custom directory name is a target triple.
    let (target_root, target) = if parent
        .file_name()
        .is_some_and(|name| name == env!("GCRUST_BUILD_TARGET"))
    {
        (
            parent
                .parent()
                .ok_or_else(|| CodegenError("runtime target triple has no parent".into()))?,
            Some(env!("GCRUST_BUILD_TARGET").to_owned()),
        )
    } else {
        (parent, None)
    };
    Ok((target_root.join("gcr-aot-runtime"), profile, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aot_runtime_cache_preserves_profile_and_target_root() {
        use std::path::{Path, PathBuf};
        assert_eq!(
            runtime_cache_location(Path::new("/tmp/custom/debug/libgcrust_rt.a")).unwrap(),
            (
                PathBuf::from("/tmp/custom/gcr-aot-runtime"),
                "debug".into(),
                None
            )
        );
        let triple = env!("GCRUST_BUILD_TARGET");
        let baked = PathBuf::from("/tmp/custom")
            .join(triple)
            .join("release/libgcrust_rt.a");
        assert_eq!(
            runtime_cache_location(&baked).unwrap(),
            (
                PathBuf::from("/tmp/custom/gcr-aot-runtime"),
                "release".into(),
                Some(triple.into())
            )
        );
        assert!(runtime_cache_location(Path::new("/")).is_err());
    }

    use crate::lexer::lex;
    use crate::lower::lower_program;
    use crate::parser::parse_module;
    use crate::resolve::resolve_module;

    fn run(src: &str) -> i64 {
        let m = parse_module(&lex(src).unwrap()).unwrap();
        let r = resolve_module(m).unwrap();
        let prog = lower_program(&r.globals).unwrap();
        unsafe { jit_run_i64(&prog) }.unwrap()
    }

    #[test]
    fn arithmetic() {
        assert_eq!(run("fn main() -> i64 { 2 + 3 * 4 }"), 14);
        assert_eq!(run("fn main() -> i64 { (10 - 2) / 4 }"), 2);
    }

    #[test]
    fn if_expr() {
        assert_eq!(run("fn main() -> i64 { let x = 7; if x < 10 { 1 } else { 0 } }"), 1);
    }

    #[test]
    fn fib_runs() {
        assert_eq!(run(include_str!("../examples/fib.gcr")), 2178309);
    }

    #[test]
    fn unsigned_div() {
        // 200/3 unsigned = 66
        assert_eq!(run("fn main() -> i64 { let x = 200u32 / 3u32; x as i64 }"), 66);
    }

    #[test]
    fn recursion_and_calls() {
        assert_eq!(
            run("fn sq(n: i64) -> i64 { n * n } fn main() -> i64 { sq(9) + sq(3) }"),
            90
        );
    }

    #[test]
    fn while_loop_sum() {
        // sum 1..=100 iteratively = 5050
        assert_eq!(
            run("fn main() -> i64 { \
                   let mut i = 1; let mut acc = 0; \
                   while i <= 100 { acc = acc + i; i = i + 1; } \
                   acc }"),
            5050
        );
    }

    #[test]
    fn loop_with_break() {
        assert_eq!(
            run("fn main() -> i64 { \
                   let mut i = 0; \
                   loop { if i >= 42 { break; } i = i + 1; } \
                   i }"),
            42
        );
    }

    #[test]
    fn compound_assign() {
        assert_eq!(
            run("fn main() -> i64 { let mut x = 10; x += 5; x *= 2; x }"),
            30
        );
    }

    // ---- floats ------------------------------------------------------------

    fn run_f64(src: &str) -> i64 {
        // Wrap: a program returning i64 that does float work internally.
        run(src)
    }

    #[test]
    fn float_arithmetic() {
        // (1.5 * 4.0 + 1.0) as i64 = 7
        assert_eq!(run_f64("fn main() -> i64 { let x = 1.5 * 4.0 + 1.0; x as i64 }"), 7);
    }

    #[test]
    fn float_compare_and_branch() {
        let src = "fn main() -> i64 { \
                     let a = 3.14; \
                     if a > 3.0 { 1 } else { 0 } \
                   }";
        assert_eq!(run_f64(src), 1);
    }

    #[test]
    fn float_loop_accumulate() {
        // Sum 1.0..=10.0 as f64, return as i64 = 55.
        let src = "fn main() -> i64 { \
                     let mut i = 0; \
                     let mut acc = 0.0; \
                     while i < 10 { acc = acc + (i as f64) + 1.0; i = i + 1; } \
                     acc as i64 \
                   }";
        assert_eq!(run_f64(src), 55);
    }

    #[test]
    fn float_f32() {
        assert_eq!(run("fn main() -> i64 { let x = 2.5f32 * 2.0f32; x as i64 }"), 5);
    }

    #[test]
    fn block_stmt_then_paren() {
        // Regression: `while c {} (expr)` must be two statements, not a call.
        assert_eq!(
            run("fn main() -> i64 { let mut i = 0; while i < 10 { i = i + 1; } (i * 2) as i64 }"),
            20
        );
        // `if {} else {}` as a statement followed by `(...)`.
        assert_eq!(
            run("fn main() -> i64 { let mut x = 0; if true { x = 5; } else { x = 1; } (x + 1) }"),
            6
        );
    }

    #[test]
    fn binary_trees_allocation_heavy() {
        // The GC-throughput benchmark shape: build + check many heap trees.
        // depth 12 = 8191 nodes/tree; 8 trees = 65528. Exercises alloc + trace
        // + collection (the live tree is rooted across recursive allocations).
        let src = "enum Tree { Leaf, Node(Tree, Tree) } \
                   fn make(d: i64) -> Tree { if d == 0 { Tree::Leaf } else { Tree::Node(make(d-1), make(d-1)) } } \
                   fn check(t: Tree) -> i64 { match t { Tree::Leaf => 1, Tree::Node(l, r) => 1 + check(l) + check(r) } } \
                   fn main() -> i64 { \
                       let mut total = 0; let mut i = 0; \
                       while i < 8 { total = total + check(make(12)); i = i + 1; } \
                       total \
                   }";
        assert_eq!(run_gc(src, false), 65528);
    }

    #[test]
    fn nbody_kernel_runs() {
        // A scaled n-body-like float kernel with sqrt; deterministic checksum.
        let src = "fn pair(ax: f64, bx: f64, dt: f64) -> f64 { \
                       let dx = ax - bx; let d2 = dx * dx + 1.0; \
                       dt / (d2 * sqrt(d2)) \
                   } \
                   fn main() -> i64 { \
                       let mut i = 0; let mut acc = 0.0; let mut px = 1.0; let mut vx = 0.0; \
                       while i < 1000 { \
                           let dv = pair(px, 0.0, 0.01); \
                           vx = vx + dv; px = px + vx * 0.01; \
                           acc = acc + sqrt(px * px + 1.0); \
                           i = i + 1; \
                       } \
                       (acc * 100.0) as i64 \
                   }";
        // Just assert it runs and is positive + deterministic.
        let v = run(src);
        assert!(v > 0, "nbody kernel checksum should be positive, got {}", v);
    }

    #[test]
    fn float_sqrt() {
        // sqrt(144.0) = 12
        assert_eq!(run("fn main() -> i64 { let x = sqrt(144.0); x as i64 }"), 12);
    }

    #[test]
    fn float_sqrt_in_expr() {
        // sqrt(3*3 + 4*4) = sqrt(25) = 5
        let src = "fn main() -> i64 { \
                     let a = 3.0; let b = 4.0; \
                     let d = sqrt(a * a + b * b); \
                     d as i64 \
                   }";
        assert_eq!(run(src), 5);
    }

    // ---- heap types + GC ---------------------------------------------------

    fn run_gc(src: &str, stress: bool) -> i64 {
        let m = parse_module(&lex(src).unwrap()).unwrap();
        let r = resolve_module(m).unwrap();
        let prog = lower_program(&r.globals).unwrap();
        unsafe { jit_run_i64_gc(&prog, stress) }.unwrap()
    }

    #[test]
    fn struct_alloc_and_field() {
        let src = "struct Point { x: i64, y: i64 } \
                   fn main() -> i64 { let p = Point { x: 3, y: 4 }; p.x + p.y }";
        assert_eq!(run_gc(src, false), 7);
    }

    #[test]
    fn generated_aggregate_access_survives_active_concurrent_cycles() {
    // Inspects heap layout: every struct a heap object as written.
    crate::compile::set_optimize(false);
        if std::env::var_os("GCR_GENERATED_CONCURRENT_CHILD").is_none() {
            let log = std::env::temp_dir().join(format!(
                "gcr-generated-concurrent-{}.log",
                std::process::id()
            ));
            let output = std::fs::File::create(&log).unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "codegen::tests::generated_aggregate_access_survives_active_concurrent_cycles",
                    "--nocapture",
                ])
                .env("GCR_GENERATED_CONCURRENT_CHILD", "1")
                .env("GCR_GC_VERIFY", "1")
                .env("GCRUST_POISON", "1")
                .stdout(output.try_clone().unwrap())
                .stderr(output)
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!(
                        "generated concurrent workload stalled: {}",
                        std::fs::read_to_string(&log).unwrap()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let details = std::fs::read_to_string(&log).unwrap();
            std::fs::remove_file(log).unwrap();
            assert!(status.success(), "{details}");
            for line in details
                .lines()
                .filter(|line| line.starts_with("generated concurrent:"))
            {
                eprintln!("{line}");
            }
            return;
        }
        use crate::gc::IdentityPtrPolicy;
        use crate::runtime::RuntimeContext;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicUsize, Ordering},
        };
        static ACTIVE_WRITES: AtomicUsize = AtomicUsize::new(0);
        static OPERATIONS: AtomicUsize = AtomicUsize::new(0);
        static OLD_ALIAS_WRITES: AtomicUsize = AtomicUsize::new(0);
        static YOUNG_EDGE_WRITES: AtomicUsize = AtomicUsize::new(0);
        static FIRST_YOUNG_MINOR: AtomicUsize = AtomicUsize::new(usize::MAX);
        static FIRST_WRITER: AtomicUsize = AtomicUsize::new(0);
        static SECOND_WRITER_SEEN: AtomicBool = AtomicBool::new(false);
        static SECOND_WRITER: AtomicUsize = AtomicUsize::new(0);
        static READER_SEEN: AtomicBool = AtomicBool::new(false);
        static ACTIVE_READER_SNAPSHOTS: AtomicUsize = AtomicUsize::new(0);
        static ACTIVE_READER_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
        static FIRST_ACTIVE_WRITER: AtomicUsize = AtomicUsize::new(0);
        static SECOND_ACTIVE_WRITER_SEEN: AtomicBool = AtomicBool::new(false);
        static COLLECTIONS_DONE: AtomicBool = AtomicBool::new(false);
        static COMPLETION_CELL: AtomicPtr<AtomicI64> = AtomicPtr::new(std::ptr::null_mut());
        unsafe extern "C" fn observe_completion_new(
            thread: *mut crate::runtime::Thread, value: i64,
        ) -> *mut AtomicI64 {
            let cell = unsafe { crate::runtime::ai_atomic_i64_new(thread, value) };
            assert!(COMPLETION_CELL.compare_exchange(std::ptr::null_mut(), cell,
                Ordering::AcqRel, Ordering::Acquire).is_ok(), "one completion cell per mode");
            cell
        }
        unsafe extern "C" fn observe_completion_load(
            thread: *mut crate::runtime::Thread, cell: *const AtomicI64,
        ) -> i64 {
            // Signal through the owning RUNNING mutator, never by dereferencing
            // a borrowed native cell from the collector after its owner exits.
            if cell == COMPLETION_CELL.load(Ordering::Acquire)
                && COLLECTIONS_DONE.load(Ordering::Acquire)
            {
                unsafe { crate::runtime::ai_atomic_i64_store(thread, cell, 1); }
            }
            unsafe { crate::runtime::ai_atomic_i64_load(thread, cell) }
        }
        unsafe extern "C" fn observe_write(
            thread: *mut crate::runtime::Thread,
            object: *mut u8,
            offset: usize,
            source: *const u8,
            bytes: usize,
            references: *const u16,
            count: usize,
        ) -> u8 {
            let heap = unsafe { &*(*thread).heap };
            let active = heap.barriers_active() && !heap.gc_requested();
            let old_alias = active && unsafe { heap.read_barrier(object) } != object;
            let young_edge = active
                && !heap.is_nursery(object)
                && (0..count).any(|index| unsafe {
                    let at = references.add(index).read() as usize;
                    heap.is_nursery(source.add(at).cast::<u64>().read_unaligned() as *const u8)
                });
            let result = unsafe {
                crate::runtime::ai_managed_try_write(
                    thread, object, offset, source, bytes, references, count,
                )
            };
            // This runtime operation cannot safepoint. Copying cannot finish
            // while this RUNNING mutator is inside the observed operation.
            if result == 1 {
                OPERATIONS.fetch_add(1, Ordering::Release);
                let address = thread as usize;
                let first = FIRST_WRITER
                    .compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire)
                    .unwrap_or_else(|previous| previous);
                if first != 0 && first != address {
                    SECOND_WRITER.store(address, Ordering::Release);
                    SECOND_WRITER_SEEN.store(true, Ordering::Release);
                }
                if active {
                    ACTIVE_WRITES.fetch_add(1, Ordering::Relaxed);
                    let first = FIRST_ACTIVE_WRITER
                        .compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire)
                        .unwrap_or_else(|previous| previous);
                    if first != 0 && first != address {
                        SECOND_ACTIVE_WRITER_SEEN.store(true, Ordering::Release);
                    }
                }
            }
            if young_edge && result == 1 {
                let _ = FIRST_YOUNG_MINOR.compare_exchange(
                    usize::MAX,
                    heap.minor_collections(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                YOUNG_EDGE_WRITES.fetch_add(1, Ordering::Release);
            }
            if old_alias && result == 1 {
                OLD_ALIAS_WRITES.fetch_add(1, Ordering::Relaxed);
            }
            result
        }
        unsafe extern "C" fn observe_read(
            thread: *mut crate::runtime::Thread,
            object: *mut u8,
            offset: usize,
            destination: *mut u8,
            bytes: usize,
        ) {
            let heap = unsafe { &*(*thread).heap };
            let active = heap.barriers_active() && !heap.gc_requested();
            let first = FIRST_WRITER.load(Ordering::Acquire);
            let second = SECOND_WRITER.load(Ordering::Acquire);
            let reader =
                first != 0 && second != 0 && thread as usize != first && thread as usize != second;
            if reader && active {
                ACTIVE_READER_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
            }
            unsafe {
                crate::runtime::ai_managed_read(thread, object, offset, destination, bytes);
            }
            OPERATIONS.fetch_add(1, Ordering::Release);
            if reader {
                READER_SEEN.store(true, Ordering::Release);
                // A contended read may park and finish after copying ends.
                // Count only reads that return while copying remains active.
                if active && heap.barriers_active() {
                    ACTIVE_READER_SNAPSHOTS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // Keep a substantial reference graph alive while fresh aggregates and
        // scalar slots change. The collector runs independently of allocations.
        let source = r#"
            struct Leaf { n: i64 }
            #[value] struct Pair { left: Leaf, right: Leaf, total: i64 }
            struct Holder { pair: Pair, scalar: i64 }
            struct Container { holder: Holder }
            fn main() -> i64 {
                let mut live: Array<Leaf> = Array::new(120000);
                for i in 0..120000 { live[i] = Leaf { n: i }; }
                let container = Container { holder: Holder {
                    pair: Pair { left: live[0], right: live[1], total: 1 }, scalar: 0
                } };
                // Escape through an atomic owner so this stays an observable
                // heap-edge load rather than a loop-invariant rooted Holder.
                let published = Atom::new(container);
                let done = AtomicI64::new(0);
                let mut i = 0;
                // A native atomic load does not park through the copying phase.
                while i < 100000 || i % 1024 != 0 || done.load() == 0 {
                    // Reload through a heap edge. Concurrent discovery defers
                    // healing this edge, so old aliases remain observable.
                    let container = published.deref();
                    let mut holder = container.holder;
                    let left = Leaf { n: i };
                    let right = Leaf { n: 0 - i };
                    holder.pair = Pair { left: left, right: right, total: 0 };
                    holder.scalar = i;
                    let snapshot = holder.pair;
                    if snapshot.left.n != i || snapshot.right.n != 0 - i || snapshot.total != 0 { return 81; }
                    if holder.scalar != i { return 82; }
                    if live[i % 120000].n != i % 120000 { return 83; }
                    i = i + 1;
                }
                42
            }
        "#;
        for (case, source) in [
            ("single", source),
            (
                "shared",
                include_str!("../tests/fixtures/concurrent_aggregates.gcr"),
            ),
        ] {
            let shared = case == "shared";
            let (module, _) = crate::compile::parse_with_prelude(source).unwrap();
            let resolved = resolve_module(module).unwrap();
            let program = lower_program(&resolved.globals).unwrap();
            for (generational, optimized) in
                [(false, false), (false, true), (true, false), (true, true)]
            {
                let mode = if generational {
                    "generational"
                } else {
                    "semispace"
                };
                let context = Context::create();
                let compiled = codegen(&context, &program).unwrap();
                if optimized {
                    optimize_module(&compiled.module).unwrap();
                }
                let engine = compiled
                    .module
                    .create_jit_execution_engine(OptimizationLevel::None)
                    .unwrap();
                map_runtime_symbols(&compiled.module, &engine);
                ACTIVE_WRITES.store(0, Ordering::Relaxed);
                OPERATIONS.store(0, Ordering::Relaxed);
                OLD_ALIAS_WRITES.store(0, Ordering::Relaxed);
                YOUNG_EDGE_WRITES.store(0, Ordering::Relaxed);
                FIRST_YOUNG_MINOR.store(usize::MAX, Ordering::Relaxed);
                COLLECTIONS_DONE.store(false, Ordering::Relaxed);
                COMPLETION_CELL.store(std::ptr::null_mut(), Ordering::Relaxed);
                FIRST_WRITER.store(0, Ordering::Relaxed);
                SECOND_WRITER_SEEN.store(false, Ordering::Relaxed);
                SECOND_WRITER.store(0, Ordering::Relaxed);
                READER_SEEN.store(false, Ordering::Relaxed);
                ACTIVE_READER_SNAPSHOTS.store(0, Ordering::Relaxed);
                ACTIVE_READER_ATTEMPTS.store(0, Ordering::Relaxed);
                FIRST_ACTIVE_WRITER.store(0, Ordering::Relaxed);
                SECOND_ACTIVE_WRITER_SEEN.store(false, Ordering::Relaxed);
                engine.add_global_mapping(
                    &compiled.module.get_function("ai_atomic_i64_new").unwrap(),
                    observe_completion_new as *const () as usize,
                );
                engine.add_global_mapping(
                    &compiled.module.get_function("ai_atomic_i64_load").unwrap(),
                    observe_completion_load as *const () as usize,
                );
                engine.add_global_mapping(
                    &compiled
                        .module
                        .get_function("ai_managed_try_write")
                        .unwrap(),
                    observe_write as *const () as usize,
                );
                engine.add_global_mapping(
                    &compiled.module.get_function("ai_managed_read").unwrap(),
                    observe_read as *const () as usize,
                );
                let mut runtime = if generational {
                    RuntimeContext::new_generational(1 << 20, 32 << 20, layouts_to_type_infos(&program))
                } else {
                    RuntimeContext::new(16 << 20, layouts_to_type_infos(&program))
                };
                let heap = runtime.heap().clone();
                let stop = Arc::new(AtomicBool::new(false));
                let collector_stop = stop.clone();
                let collector_heap = heap.clone();
                let collector = std::thread::spawn(move || {
                    // Avoid counting cycles over an empty heap before generated code
                    // has started publishing the live graph.
                    // A lone mutator writes aggregates directly while no copy
                    // runs, so its first runtime write only comes once copying
                    // starts; shared mutators always write through the runtime.
                    while collector_heap.from_used() < 128 * 1024
                        || (shared && FIRST_WRITER.load(Ordering::Acquire) == 0)
                        || (shared
                            && (!SECOND_WRITER_SEEN.load(Ordering::Acquire)
                                || !READER_SEEN.load(Ordering::Acquire)))
                    {
                        if collector_stop.load(Ordering::Acquire) {
                            return 0;
                        }
                        std::thread::yield_now();
                    }
                    let mut cycles = 0;
                    let participants_covered = || {
                        if shared {
                            SECOND_ACTIVE_WRITER_SEEN.load(Ordering::Acquire)
                                && ACTIVE_READER_SNAPSHOTS.load(Ordering::Acquire) > 0
                                && (generational || OLD_ALIAS_WRITES.load(Ordering::Acquire) > 0)
                        } else {
                            generational || OLD_ALIAS_WRITES.load(Ordering::Acquire) > 0
                        }
                    };
                    while !collector_stop.load(Ordering::Acquire)
                        && (cycles < 8
                            || !participants_covered()
                            || (generational
                                && (YOUNG_EDGE_WRITES.load(Ordering::Acquire) == 0
                                    || collector_heap.minor_collections()
                                        <= FIRST_YOUNG_MINOR.load(Ordering::Acquire))))
                    {
                        // Once major-copy coverage is complete, let normal nursery
                        // pressure trigger the required later minor without
                        // flooding the mutators with unrelated major cycles.
                        if generational
                            && cycles >= 8
                            && participants_covered()
                            && YOUNG_EDGE_WRITES.load(Ordering::Acquire) > 0
                        {
                            std::thread::yield_now();
                            continue;
                        }
                        unsafe {
                            collector_heap.concurrent_collect::<IdentityPtrPolicy>();
                        }
                        cycles += 1;
                        if cycles == 1 || cycles % 128 == 0 {
                            eprintln!(
                                "generated concurrent progress: case={case} mode={mode} optimized={optimized} cycles={cycles} young_edges={} first_minor={} minors={} old_alias_writes={}",
                                YOUNG_EDGE_WRITES.load(Ordering::Acquire),
                                FIRST_YOUNG_MINOR.load(Ordering::Acquire),
                                collector_heap.minor_collections(),
                                OLD_ALIAS_WRITES.load(Ordering::Acquire)
                            );
                        }
                        // Give resumed mutators a real opportunity to execute
                        // between cycles instead of monopolizing their wakeups.
                        // A lone mutator writes aggregates directly while no
                        // copy runs, so it is not always observed: wait for an
                        // observed operation or a short while.
                        let completed = OPERATIONS.load(Ordering::Acquire);
                        let waited = std::time::Instant::now();
                        while OPERATIONS.load(Ordering::Acquire) == completed
                            && !collector_stop.load(Ordering::Acquire)
                            && (shared || waited.elapsed() < std::time::Duration::from_millis(2))
                        {
                            std::thread::yield_now();
                        }
                    }
                    COLLECTIONS_DONE.store(true, Ordering::Release);
                    cycles
                });
                let address = engine.get_function_address(&compiled.entry_name).unwrap();
                let entry: unsafe extern "C" fn(*mut crate::runtime::Thread) -> i64 =
                    unsafe { std::mem::transmute(address) };
                let result = runtime.enter(|thread| unsafe { entry(thread) });
                stop.store(true, Ordering::Release);
                let cycles = collector.join().unwrap();
                unsafe {
                    runtime.finish_threads();
                }
                assert_eq!(result, 42, "case={case} optimized={optimized}");
                assert!(
                    cycles >= 8,
                    "generated workload did not span eight concurrent cycles"
                );
                let active_writes = ACTIVE_WRITES.load(Ordering::Relaxed);
                assert!(
                    active_writes > 0,
                    "no generated replacement completed during copying"
                );
                let old_alias_writes = OLD_ALIAS_WRITES.load(Ordering::Relaxed);
                if !generational {
                    assert!(
                        old_alias_writes > 0,
                        "no generated replacement used a forwarded old alias"
                    );
                }
                let active_reader_snapshots = ACTIVE_READER_SNAPSHOTS.load(Ordering::Relaxed);
                if shared {
                    assert!(
                        active_reader_snapshots > 0,
                        "dedicated reader did not complete a snapshot during copying; attempts={}",
                        ACTIVE_READER_ATTEMPTS.load(Ordering::Relaxed)
                    );
                    assert!(
                        SECOND_ACTIVE_WRITER_SEEN.load(Ordering::Acquire),
                        "two generated writers did not complete writes during copying"
                    );
                }
                let young_edges = YOUNG_EDGE_WRITES.load(Ordering::Acquire);
                let minors = heap.minor_collections();
                let first_young_minor = FIRST_YOUNG_MINOR.load(Ordering::Acquire);
                if generational {
                    assert!(young_edges > 0, "no active-copy old-to-young replacements");
                    assert!(
                        minors > FIRST_YOUNG_MINOR.load(Ordering::Acquire),
                        "no minor after active-copy young-edge publication"
                    );
                }
                eprintln!(
                    "generated concurrent: case={case} mode={mode} optimized={optimized} cycles={cycles} active_writes={active_writes} old_alias_writes={old_alias_writes} active_reader_snapshots={active_reader_snapshots} young_edges={young_edges} minors={minors} first_young_minor={first_young_minor}"
                );
            }
        }
    }

    #[test]
    fn generated_allocation_counters_saturate_at_unsigned_limits() {
    // Inspects heap layout: every struct a heap object as written.
    crate::compile::set_optimize(false);
        use crate::runtime::{self, RuntimeContext};
        use crate::gc::SiteCounter;
        let src = "struct Point { x: i64, y: i64 } \
                   fn mk() -> Point { Point { x: 3, y: 4 } } \
                   fn main() -> i64 { let p = mk(); p.x + p.y }";
        let module = parse_module(&lex(src).unwrap()).unwrap();
        let resolved = resolve_module(module).unwrap();
        let program = lower_program(&resolved.globals).unwrap();
        for optimized in [false, true] {
            let ctx = Context::create();
            let compiled = codegen(&ctx, &program).unwrap();
            if optimized { optimize_module(&compiled.module).unwrap(); }
            let engine = compiled.module.create_jit_execution_engine(OptimizationLevel::None).unwrap();
            // Use the production bindings: generated constructors may invoke
            // additional access helpers, even when this source only allocates.
            // Platform dynamic-symbol lookup is not a substitute for mapping.
            map_runtime_symbols(&compiled.module, &engine);
            let mut rt = RuntimeContext::new_generational(64 * 1024, 64 * 1024,
                layouts_to_type_infos(&program));
            rt.heap().set_alloc_sites(compiled.alloc_sites.clone());
            let address = engine.get_function_address(&compiled.entry_name).unwrap();
            let entry: unsafe extern "C" fn(*mut runtime::Thread) -> i64 = unsafe { std::mem::transmute(address) };
            let thread = rt.thread_ptr();
            unsafe {
                assert_eq!(rt.enter(|thread| entry(thread)), 7); // Initialize native site storage and TLAB.
                let window = &mut *(*thread).tlab_window;
                assert_eq!(window.counters_len, 1);
                assert!(window.limit - window.cursor >= 3 * 32);
                window.counters.write(SiteCounter { count: u64::MAX - 1, bytes: u64::MAX - 32 });
                for _ in 0..3 {
                    let before = (*(*thread).tlab_window).cursor;
                    assert_eq!(rt.enter(|thread| entry(thread)), 7);
                    let window = &*(*thread).tlab_window;
                    assert_eq!(window.cursor, before + 32, "must exercise generated fast allocation");
                    let counter = window.counters.read();
                    assert_eq!((counter.count, counter.bytes), (u64::MAX, u64::MAX), "optimized={optimized}");
                }
            }
        }
    }

    #[test]
    fn struct_survives_gc_stress() {
        // Allocate, then more allocations (each triggers a collection under
        // stress) — the rooted struct must survive relocation and still read 7.
        let src = "struct Point { x: i64, y: i64 } \
                   fn mk(a: i64, b: i64) -> Point { Point { x: a, y: b } } \
                   fn main() -> i64 { \
                       let p = mk(3, 4); \
                       let _q = mk(100, 200); \
                       let _r = mk(5, 6); \
                       p.x + p.y \
                   }";
        assert_eq!(run_gc(src, true), 7);
    }

    #[test]
    fn enum_match() {
        let src = "enum Shape { Circle(i64), Square(i64) } \
                   fn area(s: Shape) -> i64 { \
                       match s { Shape::Circle(r) => r * r * 3, Shape::Square(w) => w * w } \
                   } \
                   fn main() -> i64 { area(Shape::Circle(2)) + area(Shape::Square(3)) }";
        // 2*2*3 + 3*3 = 12 + 9 = 21
        assert_eq!(run_gc(src, false), 21);
    }

    #[test]
    fn enum_match_under_stress() {
        let src = "enum Shape { Circle(i64), Square(i64) } \
                   fn area(s: Shape) -> i64 { \
                       match s { Shape::Circle(r) => r * r * 3, Shape::Square(w) => w * w } \
                   } \
                   fn main() -> i64 { area(Shape::Square(7)) }";
        assert_eq!(run_gc(src, true), 49);
    }

    // ---- match completion (scalars, literals, guards) ---------------------

    #[test]
    fn match_on_integer_literals() {
        let src = "fn classify(n: i64) -> i64 { \
                     match n { 0 => 100, 1 => 200, 2 => 300, _ => 999 } \
                   } \
                   fn main() -> i64 { \
                     classify(0) + classify(1) + classify(2) + classify(9) \
                   }";
        // 100 + 200 + 300 + 999 = 1599
        assert_eq!(run_gc(src, false), 1599);
    }

    #[test]
    fn match_with_guards() {
        let src = "fn sign(n: i64) -> i64 { \
                     match n { x if x < 0 => 0 - 1, 0 => 0, _ => 1 } \
                   } \
                   fn main() -> i64 { \
                     (sign(0 - 5) + 10) * 100 + (sign(0) + 10) * 10 + (sign(42) + 10) \
                   }";
        // sign: -1, 0, 1 -> (9)*100 + (10)*10 + (11) = 900+100+11 = 1011
        assert_eq!(run_gc(src, false), 1011);
    }

    #[test]
    fn match_on_string_literals() {
        let src = "fn code(s: String) -> i64 { \
                     match s { \"red\" => 1, \"green\" => 2, \"blue\" => 3, _ => 0 } \
                   } \
                   fn main() -> i64 { code(\"green\") * 100 + code(\"blue\") * 10 + code(\"x\") }";
        // 2*100 + 3*10 + 0 = 230
        assert_eq!(run_gc(src, false), 230);
    }

    #[test]
    fn match_enum_with_guard() {
        let src = "enum Opt { None, Some(i64) } \
                   fn describe(o: Opt) -> i64 { \
                     match o { \
                       Opt::Some(x) if x > 10 => 1000 + x, \
                       Opt::Some(x) => x, \
                       Opt::None => 0 - 1, \
                     } \
                   } \
                   fn main() -> i64 { describe(Opt::Some(42)) + describe(Opt::Some(5)) + describe(Opt::None) }";
        // 1042 + 5 + (-1) = 1046
        assert_eq!(run_gc(src, true), 1046);
    }

    #[test]
    fn deferred_init_let() {
        let src = "fn main() -> i64 { \
                     let mut x: i64; \
                     if true { x = 7; } else { x = 0; } \
                     let mut y: i64; \
                     y = x * 6; \
                     y \
                   }";
        assert_eq!(run_gc(src, false), 42);
    }

    // ---- monomorphized generics -------------------------------------------

    #[test]
    fn generic_identity() {
        let src = "fn id<T>(x: T) -> T { x } \
                   fn main() -> i64 { id(41) + 1 }";
        assert_eq!(run("fn id<T>(x: T) -> T { x } fn main() -> i64 { id(41) + 1 }"), 42);
        let _ = src;
    }

    #[test]
    fn generic_used_at_two_types() {
        // `first<T>` instantiated at i64 and at u32 → two specialized funcs.
        let src = "fn dup<T>(x: T) -> T { x } \
                   fn main() -> i64 { \
                       let a = dup(10); \
                       let b = dup(5u32); \
                       a + (b as i64) \
                   }";
        assert_eq!(run(src), 15);
    }

    #[test]
    fn generic_arithmetic_specializes() {
        // A generic that does work; signedness must follow the instantiation.
        let src = "fn twice<T>(x: T) -> T { x } \
                   fn main() -> i64 { \
                       let x = twice(7); \
                       let y = twice(200u32) / twice(3u32); \
                       x + (y as i64) \
                   }";
        // 7 + (200/3 unsigned = 66) = 73
        assert_eq!(run(src), 73);
    }

    #[test]
    fn string_literal_in_result() {
        // A String literal as a Result Err payload; match returns an i64 sentinel.
        let src = format!(
            "{PRELUDE} \
             fn check(b: i64) -> Result<i64, String> {{ \
                 if b == 0 {{ Result::Err(\"zero\") }} else {{ Result::Ok(b) }} \
             }} \
             fn get(r: Result<i64, String>) -> i64 {{ \
                 match r {{ Result::Ok(v) => v, Result::Err(_e) => -1 }} \
             }} \
             fn main() -> i64 {{ get(check(5)) + get(check(0)) }}"
        );
        // 5 + (-1) = 4
        assert_eq!(run_gc(&src, true), 4);
    }

    #[test]
    fn tuples() {
        assert_eq!(run("fn main() -> i64 { let t = (10, 32); t.0 + t.1 }"), 42);
        assert_eq!(
            run("fn pair() -> (i64, i64) { (3, 4) } fn main() -> i64 { let p = pair(); p.0 * p.1 }"),
            12
        );
        // mixed-type tuple
        assert_eq!(
            run("fn main() -> i64 { let t = (5, 2.5); t.0 + (t.1 as i64) }"),
            7
        );
    }

    // ---- ergonomics: for / index / field-assign ---------------------------

    #[test]
    fn for_range_loop() {
        assert_eq!(run("fn main() -> i64 { let mut s = 0; for i in 0..100 { s = s + i; } s }"), 4950);
        // inclusive range
        assert_eq!(run("fn main() -> i64 { let mut s = 0; for i in 1..=10 { s = s + i; } s }"), 55);
    }

    #[test]
    fn index_get_set() {
        let src = "fn main() -> i64 { \
                     let mut a: Array<i64> = array_new(4); \
                     a[0] = 10; a[1] = 20; a[3] = 12; \
                     a[0] + a[1] + a[3] \
                   }";
        assert_eq!(run_gc(src, false), 42);
    }

    #[test]
    fn index_in_for_loop() {
        let src = "fn main() -> i64 { \
                     let mut a: Array<i64> = array_new(50); \
                     for i in 0..50 { a[i] = i * 2; } \
                     let mut sum = 0; \
                     for j in 0..50 { sum = sum + a[j]; } \
                     sum \
                   }";
        // 2 * (0+..+49) = 2 * 1225 = 2450
        assert_eq!(run_gc(src, false), 2450);
    }

    #[test]
    fn struct_field_assignment() {
        let src = "struct Counter { n: i64 } \
                   fn main() -> i64 { \
                       let mut c = Counter { n: 0 }; \
                       c.n = 10; \
                       c.n = c.n + 32; \
                       c.n \
                   }";
        assert_eq!(run_gc(src, true), 42);
    }

    #[test]
    fn compound_index_assign() {
        let src = "fn main() -> i64 { \
                     let mut a: Array<i64> = array_new(2); \
                     a[0] = 10; a[0] += 5; a[0] *= 2; \
                     a[0] \
                   }";
        assert_eq!(run_gc(src, false), 30);
    }

    // ---- value types (inline, flat) ---------------------------------------

    #[test]
    fn value_enum_with_payloads() {
        let src = "#[value] enum Shape { Circle(i64), Rect(i64, i64), Empty } \
                   fn area(s: Shape) -> i64 { \
                       match s { \
                           Shape::Circle(r) => r * r * 3, \
                           Shape::Rect(w, h) => w * h, \
                           Shape::Empty => 0, \
                       } \
                   } \
                   fn main() -> i64 { \
                       area(Shape::Circle(2)) + area(Shape::Rect(4, 5)) + area(Shape::Empty) \
                   }";
        // 12 + 20 + 0 = 32
        assert_eq!(run_gc(src, false), 32);
    }

    #[test]
    fn value_option_with_payload() {
        let src = "#[value] enum Opt { None, Some(i64) } \
                   fn unwrap(o: Opt, d: i64) -> i64 { match o { Opt::Some(x) => x, Opt::None => d } } \
                   fn main() -> i64 { unwrap(Opt::Some(42), 0) + unwrap(Opt::None, 8) }";
        assert_eq!(run_gc(src, false), 50);
    }

    #[test]
    fn match_exhaustiveness_enforced() {
        // `f` IS reachable from main, so its non-exhaustive match is checked.
        let src = "enum E { A, B, C } \
                   fn f(e: E) -> i64 { match e { E::A => 1, E::B => 2 } } \
                   fn main() -> i64 { f(E::A) }";
        let m = parse_module(&lex(src).unwrap()).unwrap();
        let r = resolve_module(m).unwrap();
        assert!(lower_program(&r.globals).is_err(), "non-exhaustive match should be rejected");
        // A wildcard makes it exhaustive.
        let ok = "enum E { A, B, C } \
                  fn f(e: E) -> i64 { match e { E::A => 1, _ => 0 } } \
                  fn main() -> i64 { f(E::B) }";
        let m2 = parse_module(&lex(ok).unwrap()).unwrap();
        let r2 = resolve_module(m2).unwrap();
        assert!(lower_program(&r2.globals).is_ok());
    }

    #[test]
    fn value_enum_construct_and_match() {
        let src = "#[value] enum Color { Red, Green, Blue } \
                   fn code(c: Color) -> i64 { \
                       match c { Color::Red => 1, Color::Green => 2, Color::Blue => 3 } \
                   } \
                   fn main() -> i64 { \
                       code(Color::Red) + code(Color::Green) * 10 + code(Color::Blue) * 100 \
                   }";
        assert_eq!(run_gc(src, false), 321);
    }

    #[test]
    fn value_enum_no_heap_under_stress() {
        // C-style value enums are inline (no heap), so they survive GC stress.
        let src = "#[value] enum Dir { N, S, E, W } \
                   fn dx(d: Dir) -> i64 { match d { Dir::E => 1, Dir::W => 0 - 1, Dir::N => 0, Dir::S => 0 } } \
                   fn main() -> i64 { dx(Dir::E) + dx(Dir::W) + dx(Dir::E) }";
        assert_eq!(run_gc(src, true), 1);
    }

    #[test]
    fn value_struct_field_access() {
        let src = "#[value] struct Vec3 { x: i64, y: i64, z: i64 } \
                   fn main() -> i64 { \
                       let v = Vec3 { x: 3, y: 4, z: 5 }; \
                       v.x + v.y + v.z \
                   }";
        assert_eq!(run_gc(src, false), 12);
    }

    #[test]
    fn value_struct_passed_by_value() {
        let src = "#[value] struct Vec3 { x: f64, y: f64, z: f64 } \
                   fn dot(a: Vec3, b: Vec3) -> f64 { a.x * b.x + a.y * b.y + a.z * b.z } \
                   fn main() -> i64 { \
                       let v = Vec3 { x: 1.0, y: 2.0, z: 3.0 }; \
                       dot(v, v) as i64 \
                   }";
        // 1+4+9 = 14
        assert_eq!(run_gc(src, false), 14);
    }

    #[test]
    fn value_struct_in_let_chain() {
        let src = "#[value] struct P { a: i64, b: i64 } \
                   fn mk(x: i64) -> P { P { a: x, b: x * 2 } } \
                   fn main() -> i64 { \
                       let p = mk(10); \
                       let q = mk(5); \
                       p.a + p.b + q.a + q.b \
                   }";
        // (10+20) + (5+10) = 45
        assert_eq!(run_gc(src, false), 45);
    }

    #[test]
    fn value_struct_no_heap_alloc_under_stress() {
        // Value structs are inline — even under GC-every-alloc stress, building
        // and reading them triggers no heap allocation for the struct itself.
        let src = "#[value] struct Pt { x: i64, y: i64 } \
                   fn sum(p: Pt) -> i64 { p.x + p.y } \
                   fn main() -> i64 { \
                       let a = Pt { x: 40, y: 2 }; \
                       sum(a) \
                   }";
        assert_eq!(run_gc(src, true), 42);
    }

    // ---- arrays -----------------------------------------------------------

    #[test]
    fn array_int_set_get() {
        let src = "fn main() -> i64 { \
                     let mut a: Array<i64> = array_new(5); \
                     array_set(a, 0, 10); \
                     array_set(a, 1, 20); \
                     array_set(a, 4, 12); \
                     array_get_unchecked(a, 0) + array_get_unchecked(a, 1) + array_get_unchecked(a, 4) \
                   }";
        assert_eq!(run_gc(src, false), 42);
    }

    #[test]
    fn array_len_works() {
        for (source, expected) in [
            ("fn main() -> i64 { let a: Array<i64> = array_new(7); array_len(a) }", 7),
            ("fn main() -> i64 { let a = [11, 22, 33]; array_len(a) }", 3),
            ("fn length<T>(a: Array<T>) -> i64 { array_len(a) } fn main() -> i64 { let a: Array<()> = Array::new(7); length(a) }", 7),
            ("fn main() -> i64 { let a: Array<String> = Array::new(3); array_len(a) }", 3),
            ("fn array_len(value: i64) -> i64 { value + 1 } fn main() -> i64 { array_len(41) }", 42),
        ] {
            assert_eq!(run_gc(source, false), expected, "{source}");
        }
    }

    #[test]
    fn array_sum_loop() {
        let src = "fn main() -> i64 { \
                     let mut a: Array<i64> = array_new(100); \
                     let mut i = 0; \
                     while i < 100 { array_set(a, i, i); i = i + 1; } \
                     let mut sum = 0; \
                     let mut j = 0; \
                     while j < 100 { sum = sum + array_get_unchecked(a, j); j = j + 1; } \
                     sum \
                   }";
        // sum 0..99 = 4950
        assert_eq!(run_gc(src, false), 4950);
    }

    #[test]
    fn array_float() {
        let src = "fn main() -> i64 { \
                     let mut a: Array<f64> = array_new(3); \
                     array_set(a, 0, 1.5); \
                     array_set(a, 1, 2.5); \
                     array_set(a, 2, 4.0); \
                     (array_get_unchecked(a, 0) + array_get_unchecked(a, 1) + array_get_unchecked(a, 2)) as i64 \
                   }";
        assert_eq!(run_gc(src, false), 8);
    }

    #[test]
    fn array_survives_gc_stress() {
        // Array of i64 filled, then more allocations under stress; the array is
        // rooted and its scalar contents must survive (untraced Bytes tail).
        let src = "fn mk() -> Array<i64> { array_new(10) } \
                   fn main() -> i64 { \
                       let mut a: Array<i64> = array_new(3); \
                       array_set(a, 0, 100); array_set(a, 1, 20); array_set(a, 2, 3); \
                       let _x = mk(); let _y = mk(); \
                       array_get_unchecked(a, 0) + array_get_unchecked(a, 1) + array_get_unchecked(a, 2) \
                   }";
        assert_eq!(run_gc(src, true), 123);
    }

    // ---- strings ----------------------------------------------------------

    #[test]
    fn str_len_and_eq() {
        let src = "fn main() -> i64 { \
                     let s = \"hello\"; \
                     let n = str_len(s); \
                     let e = if str_eq(s, \"hello\") { 1 } else { 0 }; \
                     let d = if str_eq(s, \"nope\") { 1 } else { 0 }; \
                     n * 100 + e * 10 + d \
                   }";
        // len 5, eq 1, neq 0 -> 510
        assert_eq!(run_gc(src, false), 510);
    }

    #[test]
    fn str_concat_basic() {
        let src = "fn main() -> i64 { \
                     let t = str_concat(\"foo\", \"barbaz\"); \
                     str_len(t) \
                   }";
        assert_eq!(run_gc(src, false), 9);
    }

    #[test]
    fn str_concat_survives_gc_stress() {
        // Repeatedly concatenate under stress (a collection per allocation); the
        // accumulated String is a rooted local and must survive every relocation.
        let src = "fn main() -> i64 { \
                     let mut acc = \"\"; \
                     let mut i = 0; \
                     while i < 50 { acc = str_concat(acc, \"ab\"); i = i + 1; } \
                     str_len(acc) \
                   }";
        assert_eq!(run_gc(src, true), 100);
    }

    #[test]
    fn str_get_and_substring() {
        let src = "fn main() -> i64 { \
                     let s = \"hello world\"; \
                     let c = str_get(s, 0); \
                     let sub = str_substring(s, 6, 11); \
                     c * 1000 + str_len(sub) \
                   }";
        // 'h' = 104, substring \"world\" len 5 -> 104005
        assert_eq!(run_gc(src, false), 104005);
    }

    #[test]
    fn to_string_int_and_concat() {
        let src = "fn main() -> i64 { \
                     let s = str_concat(\"n=\", to_string(42)); \
                     str_len(s) \
                   }";
        // \"n=42\" -> length 4
        assert_eq!(run_gc(src, false), 4);
    }

    #[test]
    fn str_to_float_intrinsic() {
        // \"2.5\" parses to 2.5; *4 = 10.0 -> 10 as i64.
        let src = "fn main() -> i64 { (str_to_float(\"2.5\") * 4.0) as i64 }";
        assert_eq!(run_gc(src, false), 10);
    }

    #[test]
    fn substring_survives_gc_stress() {
        // Take a substring then allocate heavily; the substring is a fresh rooted
        // String and must survive collection with its bytes intact.
        let src = "fn main() -> i64 { \
                     let sub = str_substring(\"abcdefgh\", 2, 6); \
                     let mut i = 0; \
                     while i < 30 { let mut j: Array<i64> = array_new(40); array_set(j, 0, i); i = i + 1; } \
                     str_len(sub) * 100 + str_get(sub, 0) \
                   }";
        // sub = \"cdef\", len 4, first byte 'c' = 99 -> 499
        assert_eq!(run_gc(src, true), 499);
    }

    #[test]
    fn print_returns_zero_and_runs() {
        // print_int returns 0; the program drives output as a side effect.
        assert_eq!(run("fn main() -> i64 { print_int(7); print_int(42); 0 }"), 0);
        assert_eq!(run("fn main() -> i64 { print_float(2.5); 0 }"), 0);
    }

    // ---- closures ---------------------------------------------------------

    #[test]
    fn closure_no_capture() {
        let src = "fn apply(f: fn(i64) -> i64, x: i64) -> i64 { f(x) } \
                   fn main() -> i64 { let inc = |n: i64| n + 1; apply(inc, 41) }";
        assert_eq!(run_gc(src, false), 42);
    }

    #[test]
    fn closure_captures_scalar() {
        let src = "fn apply(f: fn(i64) -> i64, x: i64) -> i64 { f(x) } \
                   fn main() -> i64 { \
                       let k = 10; \
                       let add_k = |n: i64| n + k; \
                       apply(add_k, 32) \
                   }";
        assert_eq!(run_gc(src, false), 42);
    }

    #[test]
    fn closure_called_directly() {
        let src = "fn main() -> i64 { let f = |a: i64, b: i64| a * b; f(6, 7) }";
        assert_eq!(run_gc(src, false), 42);
    }

    #[test]
    fn closure_captures_under_gc_stress() {
        let src = "fn apply(f: fn(i64) -> i64, x: i64) -> i64 { f(x) } \
                   fn main() -> i64 { \
                       let base = 100; \
                       let g = |n: i64| n + base; \
                       let _junk = apply(g, 1); \
                       let _junk2 = apply(g, 2); \
                       apply(g, 23) \
                   }";
        assert_eq!(run_gc(src, true), 123);
    }

    // ---- methods + traits -------------------------------------------------

    #[test]
    fn inherent_method() {
        let src = "struct Point { x: i64, y: i64 } \
                   impl Point { \
                       fn sum(self) -> i64 { self.x + self.y } \
                   } \
                   fn main() -> i64 { let p = Point { x: 3, y: 4 }; p.sum() }";
        assert_eq!(run_gc(src, false), 7);
    }

    #[test]
    fn method_with_args() {
        let src = "struct Counter { n: i64 } \
                   impl Counter { \
                       fn add(self, k: i64) -> i64 { self.n + k } \
                   } \
                   fn main() -> i64 { let c = Counter { n: 10 }; c.add(32) }";
        assert_eq!(run_gc(src, false), 42);
    }

    #[test]
    fn trait_method_static_dispatch() {
        let src = "trait Area { fn area(self) -> i64; } \
                   struct Square { side: i64 } \
                   impl Area for Square { fn area(self) -> i64 { self.side * self.side } } \
                   fn main() -> i64 { let s = Square { side: 6 }; s.area() }";
        assert_eq!(run_gc(src, false), 36);
    }

    #[test]
    fn method_under_gc_stress() {
        let src = "struct Vec2 { x: i64, y: i64 } \
                   impl Vec2 { fn dot(self, o: Vec2) -> i64 { self.x * o.x + self.y * o.y } } \
                   fn main() -> i64 { \
                       let a = Vec2 { x: 2, y: 3 }; \
                       let b = Vec2 { x: 4, y: 5 }; \
                       a.dot(b) \
                   }";
        // 2*4 + 3*5 = 23
        assert_eq!(run_gc(src, true), 23);
    }

    // ---- generic heap types (Option/Result) -------------------------------

    const PRELUDE: &str = "#[value] enum Option<T> { None, Some(T) } \
                           enum Result<T, E> { Ok(T), Err(E) } ";

    #[test]
    fn generic_option_construct_and_match() {
        let src = format!(
            "{PRELUDE} \
             fn unwrap_or(o: Option<i64>, d: i64) -> i64 {{ \
                 match o {{ Option::Some(x) => x, Option::None => d }} \
             }} \
             fn main() -> i64 {{ unwrap_or(Option::Some(42), 0) + unwrap_or(Option::None, 7) }}"
        );
        assert_eq!(run_gc(&src, false), 49);
    }

    #[test]
    fn generic_result_and_match() {
        let src = format!(
            "{PRELUDE} \
             fn safe_div(a: i64, b: i64) -> Result<i64, i64> {{ \
                 if b == 0 {{ Result::Err(0 - 1) }} else {{ Result::Ok(a / b) }} \
             }} \
             fn get(r: Result<i64, i64>) -> i64 {{ \
                 match r {{ Result::Ok(v) => v, Result::Err(e) => e }} \
             }} \
             fn main() -> i64 {{ get(safe_div(20, 4)) + get(safe_div(1, 0)) }}"
        );
        // 5 + (-1) = 4
        assert_eq!(run_gc(&src, false), 4);
    }

    #[test]
    fn generic_option_under_gc_stress() {
        let src = format!(
            "{PRELUDE} \
             fn unwrap_or(o: Option<i64>, d: i64) -> i64 {{ \
                 match o {{ Option::Some(x) => x, Option::None => d }} \
             }} \
             fn main() -> i64 {{ \
                 let a = Option::Some(100); \
                 let _b = Option::Some(200); \
                 let _c = Option::Some(300); \
                 unwrap_or(a, 0) \
             }}"
        );
        assert_eq!(run_gc(&src, true), 100);
    }

    #[test]
    fn try_operator() {
        let src = format!(
            "{PRELUDE} \
             fn checked(a: i64, b: i64) -> Result<i64, i64> {{ \
                 if b == 0 {{ Result::Err(0 - 99) }} else {{ Result::Ok(a / b) }} \
             }} \
             fn compute(x: i64, d: i64) -> Result<i64, i64> {{ \
                 let q = checked(x, d)?; \
                 let r = checked(q + 6, 2)?; \
                 Result::Ok(r + 1) \
             }} \
             fn get(r: Result<i64, i64>) -> i64 {{ \
                 match r {{ Result::Ok(v) => v, Result::Err(e) => e }} \
             }} \
             fn main() -> i64 {{ get(compute(20, 2)) + get(compute(1, 0)) }}"
        );
        // compute(20,2): q=10, r=(10+6)/2=8, Ok(9) -> 9
        // compute(1,0): checked(1,0)=Err(-99) -> ? returns Err(-99) -> get= -99
        // 9 + (-99) = -90
        assert_eq!(run_gc(&src, false), -90);
    }

    #[test]
    fn try_operator_under_stress() {
        let src = format!(
            "{PRELUDE} \
             fn checked(a: i64, b: i64) -> Result<i64, i64> {{ \
                 if b == 0 {{ Result::Err(0 - 1) }} else {{ Result::Ok(a / b) }} \
             }} \
             fn chain(x: i64) -> Result<i64, i64> {{ \
                 let a = checked(x, 2)?; \
                 let b = checked(a, 1)?; \
                 Result::Ok(b) \
             }} \
             fn get(r: Result<i64, i64>) -> i64 {{ match r {{ Result::Ok(v) => v, Result::Err(e) => e }} }} \
             fn main() -> i64 {{ get(chain(40)) }}"
        );
        assert_eq!(run_gc(&src, true), 20);
    }

    #[test]
    fn generic_struct_pair() {
        let src = "struct Pair<A, B> { first: A, second: B } \
                   fn main() -> i64 { \
                       let p = Pair { first: 10, second: 32 }; \
                       p.first + p.second \
                   }";
        assert_eq!(run_gc(src, true), 42);
    }

    #[test]
    fn nested_ref_struct_survives_gc() {
        // Wrap holds a *reference* field (inner: Pair) plus a raw tag. Under
        // stress, the inner Pair must be traced through Wrap's pointer slot and
        // both must relocate correctly.
        let src = "struct Pair { a: i64, b: i64 } \
                   struct Wrap { inner: Pair, tag: i64 } \
                   fn main() -> i64 { \
                       let p = Pair { a: 5, b: 6 }; \
                       let w = Wrap { inner: p, tag: 9 }; \
                       w.inner.a + w.inner.b + w.tag \
                   }";
        assert_eq!(run_gc(src, true), 20);
    }
}

/// Meets the scope of one path into a join: only locals defined on every
/// path stay in scope after it.
fn meet_scope(joined: &mut Option<Vec<LocalId>>, scope: &[LocalId]) {
    match joined {
        None => *joined = Some(scope.to_vec()),
        Some(j) => j.retain(|l| scope.contains(l)),
    }
}
