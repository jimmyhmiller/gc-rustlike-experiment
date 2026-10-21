# GC allocator-owner backport

Base: hashbrown 0.16.1, the unchanged crates.io package except the patch below.
Crate archive SHA-256: `841d1cc9bed7f9236f321df977030373f4a4163ae1a7dbfe1a51a2c1a51d9100`. Original Apache-2.0/MIT licenses are retained.

Patch: `gc-allocator-owner.patch` changes only `RawTable::into_allocation` and its ManuallyDrop import.
It moves the allocator out under ManuallyDrop before checking the empty singleton.
An unallocated table drops the allocator locally; allocated storage moves the
allocator into the owning iterator as before. No leak-check suppression, table
layout assumptions, iteration workaround or dependency API migration is used.

Semantic backport of the upstream correction inspected at revision
`f7c44c608517ec79e0134d5747458aeb0aa8b86e`:
https://raw.githubusercontent.com/rust-lang/hashbrown/f7c44c608517ec79e0134d5747458aeb0aa8b86e/src/raw.rs
Upstream source SHA-256: `fc20094ab11331b6d53b1a077ffe86d2bc459d1f99f71364d4d0c0101228b424`.
The 0.16 backport keeps its existing buckets/layout calculation and uses
`ptr::read(&this.alloc)` for compatibility with its original MSRV syntax.

Runtime regressions cover empty, populated, partially consumed and exhausted
HashMap/HashSet iterators, counter release and final Arc-owner destruction.
See `gc-hashbrown-empty-allocator-leak-2026-10-19` in the project pad for the
standalone reproduction and original Miri leak. Reevaluate this patch when an
upstream release containing the correction can replace it.
