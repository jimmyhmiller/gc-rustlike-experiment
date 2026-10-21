mod alloc;
mod barrier;
pub mod card_table;
pub mod dump;
mod field;
mod failure;
mod header;
mod heap;
mod installed_metadata;
mod layout_table;
mod mutator;
mod object_index;
mod ptr_policy;
pub mod reflect;
pub mod roots;
mod scan;
mod semi_space;
pub mod statemap;
mod thread;
mod telemetry;
mod type_info;

pub use alloc::{InlineTlab, inline_tlab_offsets, Alloc, AllocWindow, AtomicBumpAllocator, BumpAllocator, HeapWalker, alloc_obj};
pub use barrier::{SATBBuffer, SATBQueue, SatbQueueStats, read_barrier, read_barrier_atomic};
pub use card_table::CardTable;
pub use dump::{dump_heap_json, dump_heap_json_with_policy, dump_heap_text, dump_heap_text_with_policy};
pub use field::{
    init_header, lookup_type_info, raw_data_mut, read_raw_bytes, read_type_id,
    read_varlen_bytes, read_varlen_count, write_varlen_count,
};
pub use header::{Compact, Full, ObjHeader};
pub use heap::{CollectorScratchStats, collector_scratch_stats, AllocSiteStat, GcEvent, GcKind, Heap, ObjectAccess};
#[cfg(test)]
pub(crate) use heap::{ChannelEdges, ConditionalChannels};
pub use mutator::{GcRef, Mutator, Root, RootScope};
pub use ptr_policy::IdentityPtrPolicy;
pub use reflect::{
    AllocSite, FieldMeta, FieldTy, ScalarKind, TypeKind, TypeMeta, ValueMeta, VariantMeta,
};
pub use roots::{
    AtomicRootSet, DynRootFrame, FrameChain, FrameGuard, FrameHeader, RootFrame, RootSet,
    RootSource,
};
pub use scan::scan_object;
pub use semi_space::{FORWARDING_BIT, PtrPolicy, SemiSpace, follow_forwarding};
pub use statemap::StatemapTracer;
pub use telemetry::GC_EVENT_HISTORY_LIMIT;
pub use thread::{MutatorThread, SiteCounter, ThreadState, ThreadStateHandle, ThreadStateWeak};
pub use type_info::{TypeInfo, VarLenKind};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod graph_model_tests;

#[cfg(test)]
mod satb_pressure_tests;

#[cfg(test)]
mod lock_safe_tests;

pub(crate) use heap::OwnedPauseCallers;
