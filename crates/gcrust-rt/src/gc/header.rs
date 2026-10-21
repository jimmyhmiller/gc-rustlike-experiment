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
/// native-endian `u16` type-ID slot at `TYPE_ID_OFFSET` wholly within the
/// header. `new(id)` must initialize that slot to `id` and every byte read by
/// the collector.
/// The first eight-byte word is reserved for forwarding during collection;
/// newly initialized headers must leave its forwarding bit (bit 63) clear.
/// The same requirement applies to `HEAP_FORWARDING_OFFSET`, used by Heap.
/// Implementations must not require an address-dependent representation.
pub unsafe trait ObjHeader: Copy + 'static {
    /// Size of this header in bytes.
    const SIZE: usize;

    /// Byte offset of the `type_id` field within this header.
    /// Used by the heap walker to recover the type from any object.
    const TYPE_ID_OFFSET: usize;

    /// Aligned eight-byte word reserved for Heap forwarding. Independent of the
    /// type-ID field; the default uses the first word, as SemiSpace does.
    const HEAP_FORWARDING_OFFSET: usize = 0;

    /// Initialize a header for a newly allocated object.
    fn new(type_id: u16) -> Self;

    /// Get the type ID (index into the runtime's TypeInfo table).
    fn type_id(&self) -> u16;
}

// Store IDs in the numeric low 16 bits, leaving the forwarding marker clear
// on both byte orders. A native u16 view is at the start on little-endian and
// at the end of the word on big-endian machines.
const LOW_U16_OFFSET: usize = if cfg!(target_endian = "big") { 6 } else { 0 };

// Compact header: numeric type-ID word (8 bytes total).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Compact {
    type_id_word: u64,
}

unsafe impl ObjHeader for Compact {
    const SIZE: usize = core::mem::size_of::<Compact>();
    const TYPE_ID_OFFSET: usize = core::mem::offset_of!(Compact, type_id_word) + LOW_U16_OFFSET;

    #[inline(always)]
    fn new(type_id: u16) -> Self {
        Compact {
            type_id_word: u64::from(type_id),
        }
    }

    #[inline(always)]
    fn type_id(&self) -> u16 {
        self.type_id_word as u16
    }
}

// Full header: GC word + numeric type-ID word (16 bytes total).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Full {
    gc_word: u64,
    type_id_word: u64,
}

unsafe impl ObjHeader for Full {
    const HEAP_FORWARDING_OFFSET: usize = 8;
    const SIZE: usize = core::mem::size_of::<Full>();
    const TYPE_ID_OFFSET: usize = core::mem::offset_of!(Full, type_id_word) + LOW_U16_OFFSET;

    #[inline(always)]
    fn new(type_id: u16) -> Self {
        Full {
            gc_word: 0,
            type_id_word: u64::from(type_id),
        }
    }

    #[inline(always)]
    fn type_id(&self) -> u16 {
        self.type_id_word as u16
    }
}

impl Full {
    /// A live reference-enum tag occupies the low 32 bits of the first word.
    /// Copying collectors preserve this word in the destination before using
    /// the source for forwarding. Bit 63 remains reserved for forwarding.
    pub const ENUM_TAG_OFFSET: usize =
        core::mem::offset_of!(Full, gc_word) + if cfg!(target_endian = "big") { 4 } else { 0 };

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
    pub(crate) forwarding_offset: usize,
}

impl HeaderLayout {
    pub(crate) fn of<H: ObjHeader>() -> Self {
        let layout = Self {
            size: core::mem::size_of::<H>(),
            align: core::mem::align_of::<H>(),
            type_id_offset: H::TYPE_ID_OFFSET,
            forwarding_offset: H::HEAP_FORWARDING_OFFSET,
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
        assert!(
            layout.forwarding_offset % 8 == 0
                && layout.forwarding_offset.checked_add(8).is_some_and(|end| end <= layout.size),
            "header forwarding-word offset is invalid"
        );
        layout
    }

    /// Recover the ID without reading bytes concurrently replaced by a claim.
    /// Outside the forwarding word the immutable ID remains readable; inside it
    /// use the captured original bytes, respecting native byte order.
    pub(crate) unsafe fn type_id_from_word(&self, object: *const u8, word: u64) -> u16 {
        if self.type_id_offset >= self.forwarding_offset
            && self.type_id_offset < self.forwarding_offset + 8
        {
            let index = self.type_id_offset - self.forwarding_offset;
            let bytes = word.to_ne_bytes();
            u16::from_ne_bytes([bytes[index], bytes[index + 1]])
        } else {
            unsafe { object.add(self.type_id_offset).cast::<u16>().read() }
        }
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


#[cfg(test)]
mod extraction_tests {
    use super::*;

    #[test]
    fn collector_header_id_extraction_preserves_captured_word_and_separate_slots() {
        #[repr(align(8))]
        struct Bytes([u8; 16]);
        for forwarding_offset in [0, 8] {
            for type_id_offset in (0..16).step_by(2) {
                for id in [0u16, 1, 513, 32767, 65535] {
                    let mut object = Bytes([0x25; 16]);
                    object.0[type_id_offset..type_id_offset + 2]
                        .copy_from_slice(&id.to_ne_bytes());
                    let layout = HeaderLayout {
                        size: 16, align: 8, type_id_offset, forwarding_offset,
                    };
                    let pointer = object.0.as_mut_ptr();
                    let captured = unsafe { pointer.add(forwarding_offset).cast::<u64>().read() };
                    // An exclusive claim may replace the whole source word.
                    // Overlapping IDs must come from captured bytes; separate
                    // immutable IDs must still come from their actual slot.
                    unsafe { pointer.add(forwarding_offset).cast::<u64>().write(u64::MAX); }
                    assert_eq!(unsafe { layout.type_id_from_word(pointer, captured) }, id,
                        "ID offset {type_id_offset}, forwarding offset {forwarding_offset}");
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "default_header_tests.rs"]
mod default_header_tests;
