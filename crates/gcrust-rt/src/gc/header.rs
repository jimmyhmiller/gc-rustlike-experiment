use crate::gc::type_info::TypeInfo;

/// Defines the header prepended to every heap object.
///
/// Choose compact (8 bytes), full (16 bytes), or define your own.
/// The header lives at the start of every heap allocation and lets
/// the runtime (GC, debugger, etc.) identify the object's shape.
///
/// # Safety
///
/// Implementations must have a stable layout, with `SIZE == size_of::<Self>()`,
/// a size of at least eight bytes that is a multiple of eight, and an aligned
/// `u16` type-id field at `TYPE_ID_OFFSET` wholly within the header. `new(id)`
/// must initialize that field to `id` and every byte read by the collector.
/// The first eight-byte word is reserved for forwarding during collection;
/// newly initialized headers must leave its forwarding bit (bit 63) clear.
/// Implementations must not require an address-dependent representation.
pub unsafe trait ObjHeader: Copy + 'static {
    /// Size of this header in bytes.
    const SIZE: usize;

    /// Byte offset of the `type_id` field within this header.
    /// Used by the heap walker to recover the type from any object.
    const TYPE_ID_OFFSET: usize;

    /// Initialize a header for a newly allocated object.
    fn new(type_id: u16) -> Self;

    /// Get the type ID (index into the runtime's TypeInfo table).
    fn type_id(&self) -> u16;
}

// Compact header: type_id + padding (8 bytes total).
// The remaining 6 bytes after type_id are available for future use
// (e.g., GC mark bits, hash code, etc.)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Compact {
    type_id: u16,
    _pad: u16,
    _pad2: u32,
}

unsafe impl ObjHeader for Compact {
    const SIZE: usize = core::mem::size_of::<Compact>();
    const TYPE_ID_OFFSET: usize = core::mem::offset_of!(Compact, type_id);

    #[inline(always)]
    fn new(type_id: u16) -> Self {
        Compact {
            type_id,
            _pad: 0,
            _pad2: 0,
        }
    }

    #[inline(always)]
    fn type_id(&self) -> u16 {
        self.type_id
    }
}

// Full header: GC word + type_id + padding (16 bytes total).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Full {
    gc_word: u64,
    type_id: u16,
    _pad: u16,
    _pad2: u32,
}

unsafe impl ObjHeader for Full {
    const SIZE: usize = core::mem::size_of::<Full>();
    const TYPE_ID_OFFSET: usize = core::mem::offset_of!(Full, type_id);

    #[inline(always)]
    fn new(type_id: u16) -> Self {
        Full {
            gc_word: 0,
            type_id,
            _pad: 0,
            _pad2: 0,
        }
    }

    #[inline(always)]
    fn type_id(&self) -> u16 {
        self.type_id
    }
}

impl Full {
    #[inline(always)]
    pub fn gc_word(&self) -> u64 {
        self.gc_word
    }

    #[inline(always)]
    pub fn set_gc_word(&mut self, val: u64) {
        self.gc_word = val;
    }
}

// `TypeInfo` is referenced here so the type_info module is reachable.
#[allow(dead_code)]
fn _link_type_info(_t: &TypeInfo) {}

/// Validated physical header layout, retained by arenas to reject mismatched
/// allocation descriptors and header initializers before reserving storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeaderLayout {
    size: usize,
    align: usize,
    type_id_offset: usize,
}

impl HeaderLayout {
    pub(crate) fn of<H: ObjHeader>() -> Self {
        let layout = Self {
            size: core::mem::size_of::<H>(),
            align: core::mem::align_of::<H>(),
            type_id_offset: H::TYPE_ID_OFFSET,
        };
        assert_eq!(
            H::SIZE,
            layout.size,
            "header SIZE differs from physical size"
        );
        assert!(
            layout.size >= 8 && layout.size % 8 == 0,
            "header must contain whole eight-byte words"
        );
        assert!(
            layout.type_id_offset % core::mem::align_of::<u16>() == 0
                && layout
                    .type_id_offset
                    .checked_add(2)
                    .is_some_and(|end| end <= layout.size),
            "header type-id offset is invalid"
        );
        layout
    }

    pub(crate) fn accepts(&self, info: &TypeInfo, len: usize) -> bool {
        info.header_size as usize == self.size
            && 1usize
                .checked_shl(info.align_log2 as u32)
                .is_some_and(|align| align >= self.align)
            && info.checked_allocation_size(len).is_some()
    }

    pub(crate) fn is<H: ObjHeader>(&self) -> bool {
        *self == Self::of::<H>()
    }

    pub(crate) fn supported_by_atomic_arena(&self) -> bool {
        self.align <= 8
    }
}
