//! Copy a byte-indexed range into independently owned, validated UTF-8 text.
//! Work is proportional to the copied range, never the surrounding source.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RangeError {
    Boundary,
    InvalidUtf8,
}

pub(super) fn checked_range(
    bytes: &[u8],
    start: i64,
    end: i64,
) -> Result<std::ops::Range<usize>, RangeError> {
    let len = bytes.len() as i64;
    let lo = start.clamp(0, len) as usize;
    let hi = end.clamp(lo as i64, len) as usize;
    // Reject empty cuts inside code points as well as nonempty partial cuts.
    let boundary = |index: usize| index == bytes.len() || bytes[index] & 0xc0 != 0x80;
    if !boundary(lo) || !boundary(hi) {
        return Err(RangeError::Boundary);
    }
    Ok(lo..hi)
}

#[cfg(test)]
pub(super) fn copy_utf8_range(bytes: &[u8], start: i64, end: i64) -> Result<String, RangeError> {
    let range = checked_range(bytes, start, end)?;
    String::from_utf8(bytes[range].to_vec()).map_err(|_| RangeError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_string_range_matches_unicode_reference_and_clamped_bounds() {
        for text in ["", "abc", "é🦀", "aλ中🦀z", "\0a\n\r\t"] {
            let positions: Vec<_> = [i64::MIN, -1]
                .into_iter()
                .chain(0..=text.len() as i64 + 2)
                .chain([i64::MAX])
                .collect();
            for &start in &positions {
                for &end in &positions {
                    let lo = (start as i128).max(0).min(text.len() as i128) as usize;
                    let hi = (end as i128).max(lo as i128).min(text.len() as i128) as usize;
                    let expected = text
                        .get(lo..hi)
                        .map(str::to_owned)
                        .ok_or(RangeError::Boundary);
                    assert_eq!(
                        copy_utf8_range(text.as_bytes(), start, end),
                        expected,
                        "{text:?} {start}..{end}"
                    );
                }
            }
        }
    }

    #[test]
    fn collector_string_range_validates_copied_text_not_unrelated_bytes() {
        assert_eq!(copy_utf8_range(&[b'a', 0xff], 0, 1), Ok("a".into()));
        assert_eq!(copy_utf8_range(&[0xff, b'b'], 1, 2), Ok("b".into()));
        for bytes in [
            &[0xff][..],
            &[0xc0, 0x80],
            &[0xed, 0xa0, 0x80],
            &[0xf4, 0x90, 0x80, 0x80],
        ] {
            assert_eq!(
                copy_utf8_range(bytes, 0, bytes.len() as i64),
                Err(RangeError::InvalidUtf8)
            );
        }
        assert_eq!(
            copy_utf8_range("🦀".as_bytes(), 1, 1),
            Err(RangeError::Boundary)
        );
    }

    #[test]
    fn collector_string_range_owned_copy_survives_moving_collection() {
        use crate::{
            gc::{Full, ObjHeader, TypeInfo},
            runtime::{
                RuntimeContext, ai_gc_alloc_varlen, ai_str_concat, ai_str_join, ai_str_substring,
                str_bytes,
            },
        };
        let text = "prefix λ中🦀 suffix";
        for generational in [false, true] {
            let info = TypeInfo::for_header(Full::SIZE).with_varlen_bytes(0);
            let mut runtime = if generational {
                RuntimeContext::new_generational(
                    1024,
                    16384,
                    vec![
                        info,
                        TypeInfo::for_header(Full::SIZE)
                            .with_type_id(1)
                            .with_varlen_values(0),
                    ],
                )
            } else {
                RuntimeContext::new(
                    16384,
                    vec![
                        info,
                        TypeInfo::for_header(Full::SIZE)
                            .with_type_id(1)
                            .with_varlen_values(0),
                    ],
                )
            };
            let heap = runtime.heap().clone();
            heap.set_gc_every_alloc(true);
            runtime.enter(|thread| unsafe {
                let source = ai_gc_alloc_varlen(thread, 0, text.len() as u64, 0);
                source
                    .add(Full::SIZE + 8)
                    .copy_from_nonoverlapping(text.as_ptr(), text.len());
                let root = heap.globals.add(source as u64);
                let mut moved = false;
                for _ in 0..16 {
                    let source = heap.globals.get(root) as *const u8;
                    let result = ai_str_substring(thread, 0, source, 7, 16);
                    moved |= heap.globals.get(root) != source as u64;
                    assert_eq!(std::str::from_utf8(str_bytes(result)).unwrap(), "λ中🦀");
                }
                assert!(moved, "source must have moved");
                let source = heap.globals.get(root) as *const u8;
                let joined = ai_str_concat(thread, 0, source, source);
                assert_eq!(
                    std::str::from_utf8(str_bytes(joined)).unwrap(),
                    text.repeat(2)
                );
                let array = ai_gc_alloc_varlen(thread, 1, 2, 0);
                heap.globals.add(array as u64);
                let source = heap.globals.get(root) as *mut u8;
                for index in 0..2 {
                    super::super::slots::ai_slot_ref_store(thread, array, 24 + index * 8, source);
                }
                let joined = ai_str_join(thread, 0, array, 2, source);
                assert_eq!(
                    std::str::from_utf8(str_bytes(joined)).unwrap(),
                    text.repeat(3)
                );
            });
        }
    }
}
