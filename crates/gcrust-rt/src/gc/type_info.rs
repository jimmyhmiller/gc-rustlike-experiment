use std::borrow::Cow;

/// Describes the shape of a heap object so the runtime can compute
/// field offsets, allocation sizes, and GC scan boundaries.
///
/// `header_size` is stored directly (from `ObjHeader::SIZE`) so that
/// all offset methods can be `const fn` without needing generic bounds.
///
/// # Memory layout
///
/// ```text
/// ┌───────────────────┐  offset 0
/// │   header          │  header_size bytes
/// ├───────────────────┤
/// │   field[0] (u64)  │  value_field_count × 8 bytes (GC-traced)
/// │   field[1] (u64)  │
/// │   ...             │
/// ├───────────────────┤
/// │   raw bytes       │  raw_byte_count bytes, padded to 8
/// ├───────────────────┤
/// │   varlen_len (u64)│  only if varlen != None
/// │   varlen[0..n]    │  n elements (Values or bytes)
/// └───────────────────┘
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeInfo {
    /// Numeric type identifier. Stored in the object header for fast type dispatch.
    /// Set by the runtime when registering types (e.g., 0=String, 1=Closure, etc.)
    pub type_id: u16,

    /// Size of the object header in bytes (from `ObjHeader::SIZE`).
    pub header_size: u16,

    /// Number of GC-traced Value slots (each 8 bytes).
    pub value_field_count: u16,

    /// Number of untraced raw bytes (after value fields).
    pub raw_byte_count: u16,

    /// Whether this object has a variable-length tail, and what kind.
    pub varlen: VarLenKind,

    /// Log2 of allocation alignment (minimum 3, i.e. 8-byte aligned).
    pub align_log2: u8,

    /// Absolute byte offsets (from the object start) of GC pointers embedded in
    /// the *raw* region — i.e. references inside flattened `#[value]` aggregates
    /// stored inline. The leading `value_field_count` slots cover ordinary
    /// pointer fields; these cover interior pointers the contiguous-slots model
    /// can't express. Empty for the common case.
    ///
    /// Static descriptors borrow their offsets. Dynamically built descriptors
    /// own them, so repeated compilation/execution releases metadata with the
    /// type table rather than leaking one allocation per shape.
    pub interior_ptrs: Cow<'static, [u16]>,

    /// For a `Bytes` tail holding inline records (an array of value
    /// aggregates): each record's size in bytes, or 0 for plain bytes. The
    /// tail's count is its number of records, so compiled code bounds-checks
    /// an index against it directly.
    pub record_stride: u16,

    /// Offsets, within each record, of its GC references.
    pub record_ptrs: Cow<'static, [u16]>,
}

/// Whether a heap object has a variable-length tail section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarLenKind {
    /// Fixed-size object — no variable-length tail.
    None,
    /// Variable-length array of GC-traced Values (each 8 bytes).
    Values,
    /// Variable-length byte array (e.g. string contents).
    Bytes,
}

/// Round `n` up to the next multiple of 8.
const fn align8(n: usize) -> usize {
    (n + 7) & !7
}

impl TypeInfo {
    /// Start building a TypeInfo for objects using a header of the given size.
    ///
    /// ```rust,ignore
    /// use ai_lang::gc::{TypeInfo, Full, Compact, ObjHeader};
    ///
    /// // A cons cell: 2 value fields, Full header
    /// const CONS: TypeInfo = TypeInfo::for_header(Full::SIZE).with_fields(2);
    ///
    /// // A string: no fixed fields, variable-length bytes, Compact header
    /// const STR: TypeInfo = TypeInfo::for_header(Compact::SIZE).with_varlen_bytes(0);
    /// ```
    pub const fn for_header(header_size: usize) -> Self {
        Self::for_header_with_interior_ptrs(header_size, &[])
    }

    /// Construct a static descriptor with borrowed interior offsets.
    pub const fn for_header_with_interior_ptrs(
        header_size: usize,
        offsets: &'static [u16],
    ) -> Self {
        assert!(
            header_size >= 8 && header_size % 8 == 0 && header_size <= u16::MAX as usize,
            "header size must be representable whole eight-byte words"
        );
        Self {
            type_id: 0, // set by runtime when registering types
            header_size: header_size as u16,
            value_field_count: 0,
            raw_byte_count: 0,
            varlen: VarLenKind::None,
            align_log2: 3, // 8-byte alignment minimum
            interior_ptrs: Cow::Borrowed(offsets),
            record_stride: 0,
            record_ptrs: Cow::Borrowed(&[]),
        }
    }

    /// Set the type_id for this TypeInfo.
    pub const fn with_type_id(mut self, id: u16) -> Self {
        self.type_id = id;
        self
    }

    /// Set the number of GC-traced Value fields (each 8 bytes).
    pub const fn with_fields(mut self, count: u16) -> Self {
        self.value_field_count = count;
        self
    }

    /// Set the number of untraced raw bytes.
    pub const fn with_raw_bytes(mut self, count: u16) -> Self {
        self.raw_byte_count = count;
        self
    }

    /// Add a variable-length Values tail (GC-traced).
    /// `fixed_fields` sets the number of fixed Value fields before the varlen section.
    pub const fn with_varlen_values(mut self, fixed_fields: u16) -> Self {
        self.value_field_count = fixed_fields;
        self.varlen = VarLenKind::Values;
        self
    }

    /// Add a variable-length byte tail.
    /// `fixed_fields` sets the number of fixed Value fields before the varlen section.
    pub const fn with_varlen_bytes(mut self, fixed_fields: u16) -> Self {
        self.value_field_count = fixed_fields;
        self.varlen = VarLenKind::Bytes;
        self
    }

    /// Add a variable-length tail of inline records, `stride` bytes each, with
    /// GC references at `ptrs` within each record. The tail's count is in
    /// bytes.
    pub fn with_varlen_records(mut self, fixed_fields: u16, stride: u16, ptrs: Vec<u16>) -> Self {
        self.value_field_count = fixed_fields;
        self.varlen = VarLenKind::Bytes;
        self.record_stride = stride;
        self.record_ptrs = Cow::Owned(ptrs);
        self
    }

    /// Set the interior-pointer offsets (GC refs embedded in flattened value
    /// fields). Offsets are absolute from the object start (header included).
    pub fn with_interior_ptrs(mut self, offsets: &'static [u16]) -> Self {
        self.interior_ptrs = Cow::Borrowed(offsets);
        self
    }

    /// Own dynamically generated interior offsets for this descriptor's lifetime.
    pub fn with_owned_interior_ptrs(mut self, offsets: Vec<u16>) -> Self {
        self.interior_ptrs = Cow::Owned(offsets);
        self
    }

    /// Set the alignment (as log2). Minimum is 3 (8-byte aligned).
    pub const fn with_align_log2(mut self, log2: u8) -> Self {
        assert!(log2 >= 3, "alignment must be at least 8 bytes (log2 >= 3)");
        self.align_log2 = log2;
        self
    }

    /// Byte offset of value field `index` from the start of the object.
    pub const fn value_field_offset(&self, index: u16) -> usize {
        self.header_size as usize + (index as usize) * 8
    }

    /// Byte offset of the raw data section from the start of the object.
    pub const fn raw_data_offset(&self) -> usize {
        self.header_size as usize + (self.value_field_count as usize) * 8
    }

    /// Byte offset of the varlen count word from the start of the object.
    /// Only meaningful when `varlen != VarLenKind::None`.
    pub const fn varlen_count_offset(&self) -> usize {
        align8(self.raw_data_offset() + self.raw_byte_count as usize)
    }

    /// Byte offset of varlen element `index` from the start of the object.
    /// For `Values`, each element is 8 bytes. For `Bytes`, each is 1 byte.
    pub const fn varlen_element_offset(&self, index: usize) -> usize {
        let base = self.varlen_count_offset() + 8; // skip the count word
        match self.varlen {
            VarLenKind::None => panic!("no varlen section"),
            VarLenKind::Values => base + index * 8,
            VarLenKind::Bytes => base + index,
        }
    }

    /// Total allocation size in bytes for an object with `varlen_len` variable-length elements.
    /// Check arithmetic and the Rust pointer-offset limit before accepting an
    /// untrusted length. Keep this consistent with `allocation_size` below.
    pub fn checked_allocation_size(&self, varlen_len: usize) -> Option<usize> {
        if self.align_log2 < 3 || self.header_size < 8 || self.header_size % 8 != 0 {
            return None;
        }
        // Interior references must be aligned complete words inside the raw
        // section, not the header, fixed pointer fields, or variable tail.
        let raw_start = self.raw_data_offset();
        let raw_end = raw_start.checked_add(self.raw_byte_count as usize)?;
        if self.interior_ptrs.iter().any(|&off| {
            let off = off as usize;
            off % 8 != 0 || off < raw_start || off.checked_add(8).is_none_or(|end| end > raw_end)
        }) {
            return None;
        }
        // Records live in a byte tail; each reference is an aligned word
        // inside its record.
        if (self.record_stride != 0 || !self.record_ptrs.is_empty())
            && (self.varlen != VarLenKind::Bytes || self.record_stride % 8 != 0 || self.record_stride == 0
                || self.record_ptrs.iter().any(|&off| off % 8 != 0 || off as usize + 8 > self.record_stride as usize))
        {
            return None;
        }
        self.checked_size_for_valid_layout(varlen_len)
    }

    /// Checked request geometry after validating immutable descriptor metadata.
    /// This only computes a size; callers must validate the layout before using
    /// the result to allocate or scan. Heap does so at table installation.
    #[inline]
    pub(crate) fn checked_size_for_valid_layout(&self, varlen_len: usize) -> Option<usize> {
        let raw_end = self
            .raw_data_offset()
            .checked_add(self.raw_byte_count as usize)?;
        let size = match self.varlen {
            VarLenKind::None => raw_end.checked_add(7)? & !7,
            VarLenKind::Values => self
                .varlen_count_offset()
                .checked_add(8)?
                .checked_add(varlen_len.checked_mul(8)?)?,
            VarLenKind::Bytes => {
                let bytes = if self.record_stride != 0 { varlen_len.checked_mul(self.record_stride as usize)? } else { varlen_len };
                self.varlen_count_offset()
                    .checked_add(8)?
                    .checked_add(bytes)?
                    .checked_add(7)?
                    & !7
            }
        };
        let align = 1usize.checked_shl(self.align_log2 as u32)?;
        let total = size.checked_add(align - 1)? & !(align - 1);
        (total <= isize::MAX as usize).then_some(total)
    }

    /// Total allocation size for a length validated by checked_allocation_size.
    /// Result is aligned to the object's alignment requirement.
    pub const fn allocation_size(&self, varlen_len: usize) -> usize {
        let size = match self.varlen {
            VarLenKind::None => align8(self.raw_data_offset() + self.raw_byte_count as usize),
            VarLenKind::Values => {
                // count word + n×8 bytes
                self.varlen_count_offset() + 8 + varlen_len * 8
            }
            VarLenKind::Bytes => {
                // count word + n bytes (n records of `record_stride` bytes
                // for a record tail), aligned up
                let bytes = if self.record_stride != 0 { varlen_len * self.record_stride as usize } else { varlen_len };
                align8(self.varlen_count_offset() + 8 + bytes)
            }
        };
        let align = 1usize << self.align_log2;
        (size + align - 1) & !(align - 1)
    }
}

#[cfg(test)]
mod allocation_boundary_tests {
    use super::*;

    #[test]
    fn checked_sizes_match_layouts_and_reject_unrepresentable_tails() {
        for header in [8, 16] {
            for alignment in [3, 4, 6] {
                for fields in [0, 3] {
                    for raw in [0, 7, 32] {
                        let base = TypeInfo::for_header(header)
                            .with_fields(fields)
                            .with_raw_bytes(raw)
                            .with_align_log2(alignment);
                        for info in [
                            base.clone(),
                            base.clone().with_varlen_bytes(fields),
                            base.with_varlen_values(fields),
                        ] {
                            for length in [0, 1, 7, 1001] {
                                assert_eq!(
                                    info.checked_allocation_size(length),
                                    Some(info.allocation_size(length))
                                );
                            }
                            if info.varlen != VarLenKind::None {
                                for length in [isize::MAX as usize, usize::MAX - 7, usize::MAX] {
                                    assert_eq!(info.checked_allocation_size(length), None);
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut under_aligned = TypeInfo::for_header(16);
        under_aligned.align_log2 = 2;
        assert_eq!(under_aligned.checked_allocation_size(0), None);
        let invalid_alignment = TypeInfo::for_header(16).with_align_log2(255);
        assert_eq!(invalid_alignment.checked_allocation_size(0), None);
    }
}

#[cfg(test)]
mod descriptor_contract_tests {
    use super::*;

    #[test]
    fn malformed_headers_and_interior_references_are_rejected() {
        let valid = TypeInfo::for_header(16).with_fields(1).with_raw_bytes(16);
        for header_size in [0, 7, 9, 15] {
            assert_eq!(
                TypeInfo {
                    header_size,
                    ..valid.clone()
                }
                .checked_allocation_size(0),
                None
            );
        }
        for offsets in [&[0][..], &[16][..], &[25][..], &[40][..], &[65528][..]] {
            assert_eq!(
                valid
                    .clone()
                    .with_owned_interior_ptrs(offsets.to_vec())
                    .checked_allocation_size(0),
                None
            );
        }
        assert_eq!(
            valid
                .with_interior_ptrs(&[24, 32])
                .checked_allocation_size(0),
            Some(40)
        );
        assert!(std::panic::catch_unwind(|| TypeInfo::for_header(65536 + 16)).is_err());
    }
}

#[cfg(test)]
mod validated_geometry_tests {
    use super::*;
    #[test]
    fn collector_validated_geometry_matches_full_checks_at_tail_boundaries() {
        for fields in [0, 1, 32] {
            for raw in [0, 1, 7, 8, 255, 4096] {
                for kind in [VarLenKind::None, VarLenKind::Bytes, VarLenKind::Values] {
                    let mut info = TypeInfo::for_header(16)
                        .with_fields(fields)
                        .with_raw_bytes(raw);
                    info.varlen = kind;
                    if raw >= 8 {
                        info = info.with_owned_interior_ptrs(vec![(16 + fields * 8)]);
                    }
                    for length in [
                        0,
                        1,
                        7,
                        8,
                        8192,
                        isize::MAX as usize / 8,
                        isize::MAX as usize,
                        usize::MAX,
                    ] {
                        // An independent wide-integer oracle never overflows
                        // at native usize boundaries before applying the limit.
                        let raw_end = 16u128 + u128::from(fields) * 8 + u128::from(raw);
                        let tail = (raw_end + 7) / 8 * 8 + 8;
                        let wide = match kind {
                            VarLenKind::None => (raw_end + 7) / 8 * 8,
                            VarLenKind::Bytes => (tail + length as u128 + 7) / 8 * 8,
                            VarLenKind::Values => tail + length as u128 * 8,
                        };
                        let expected = (wide <= isize::MAX as u128).then_some(wide as usize);
                        assert_eq!(info.checked_size_for_valid_layout(length), expected);
                        assert_eq!(info.checked_allocation_size(length), expected);
                    }
                }
            }
        }
    }
}
