//! `ChzStr` — the storage for an `Obj::Str` (M19 SSO). Short strings live **inline** in the
//! variant (no heap `Box` alloc); longer strings spill to a `Box<str>`. The `str` bench's
//! `"item-N"` parts are all ≤ `INLINE_CAP` bytes, so building 500k of them no longer touches
//! the allocator per element.
//!
//! `Deref<Target = str>` makes every existing `Obj::Str(s)` match arm (`s.chars()`, `s.len()`,
//! `s.as_bytes()`, `&**s`) compile unchanged; `From<&str>` / `From<String>` keep the
//! construction sites (incl. `"x".into()` in tests) untouched. `Clone`/`Eq`/`Hash` delegate to
//! `as_str()` so map keys, interning, and `==` stay byte-identical to the old `Box<str>`.

use std::ops::Deref;

/// Max UTF-8 byte length stored inline. Chosen so `size_of::<Obj>()` is unchanged (the `Str`
/// variant stays far smaller than `Module`/`Closure`). 22 bytes + a 1-byte length tag packs the
/// inline arm into 24 bytes.
pub const INLINE_CAP: usize = 22;

/// Storage for a Chezzi string value: inline for short strings, heap-boxed for the rest.
#[derive(Clone)]
pub enum ChzStr {
    Inline {
        len: u8,
        bytes: [u8; INLINE_CAP],
    },
    Heap(Box<str>),
    /// Heap-boxed, every byte < 0x80, so a codepoint index IS a byte index (TICKET-072).
    HeapAscii(Box<str>),
}

impl ChzStr {
    /// The string slice, regardless of storage.
    pub fn as_str(&self) -> &str {
        match self {
            ChzStr::Inline { len, bytes } => {
                // SAFETY: `bytes[..len]` is written only by `From<&str>` (the sole inline writer;
                // `From<String>`/`From<Box<str>>` delegate to it), which `copy_from_slice`s exactly
                // `len` bytes from a validated `&str`. Any new constructor MUST preserve that.
                unsafe { std::str::from_utf8_unchecked(&bytes[..*len as usize]) }
            }
            ChzStr::Heap(s) => s,
            ChzStr::HeapAscii(s) => s,
        }
    }

    /// Whether the value is stored inline (no heap allocation). Test/diagnostic helper.
    #[cfg(test)]
    pub fn is_inline(&self) -> bool {
        matches!(self, ChzStr::Inline { .. })
    }

    /// The bytes of this string IF every byte is ASCII (< 0x80), in which case a byte index is
    /// also a codepoint index. `None` for any non-ASCII string, inline or heap.
    pub fn ascii_bytes(&self) -> Option<&[u8]> {
        match self {
            ChzStr::HeapAscii(s) => Some(s.as_bytes()),
            ChzStr::Heap(_) => None,
            ChzStr::Inline { .. } => {
                let b = self.as_str().as_bytes();
                b.is_ascii().then_some(b)
            }
        }
    }

    /// Number of codepoints. O(1) for an ASCII string, O(n) otherwise.
    pub fn char_len(&self) -> usize {
        match self.ascii_bytes() {
            Some(b) => b.len(),
            None => self.as_str().chars().count(),
        }
    }
}

impl From<&str> for ChzStr {
    fn from(s: &str) -> Self {
        if s.len() <= INLINE_CAP {
            let mut bytes = [0u8; INLINE_CAP];
            bytes[..s.len()].copy_from_slice(s.as_bytes());
            ChzStr::Inline {
                len: s.len() as u8,
                bytes,
            }
        } else if s.is_ascii() {
            ChzStr::HeapAscii(Box::from(s))
        } else {
            ChzStr::Heap(Box::from(s))
        }
    }
}

impl From<String> for ChzStr {
    fn from(s: String) -> Self {
        // Reuse the `&str` selection; for the heap arm this reuses `s`'s existing allocation.
        if s.len() <= INLINE_CAP {
            ChzStr::from(s.as_str())
        } else if s.is_ascii() {
            ChzStr::HeapAscii(s.into_boxed_str())
        } else {
            ChzStr::Heap(s.into_boxed_str())
        }
    }
}

impl From<Box<str>> for ChzStr {
    fn from(s: Box<str>) -> Self {
        if s.len() <= INLINE_CAP {
            ChzStr::from(&*s) // copy into the inline buffer, drop the box
        } else if s.is_ascii() {
            ChzStr::HeapAscii(s) // reuse the existing allocation
        } else {
            ChzStr::Heap(s) // reuse the existing allocation
        }
    }
}

impl Deref for ChzStr {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Debug for ChzStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for ChzStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for ChzStr {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for ChzStr {}

impl std::hash::Hash for ChzStr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_string_is_inline() {
        let s: ChzStr = "item-499999".into(); // 11 bytes
        assert!(s.is_inline());
        assert_eq!(s.as_str(), "item-499999");
    }

    #[test]
    fn empty_string_is_inline() {
        let s: ChzStr = "".into();
        assert!(s.is_inline());
        assert_eq!(s.as_str(), "");
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn boundary_cap_minus_one_inline() {
        let raw = "a".repeat(INLINE_CAP - 1);
        let s: ChzStr = raw.as_str().into();
        assert!(s.is_inline());
        assert_eq!(s.as_str(), raw);
    }

    #[test]
    fn boundary_exactly_cap_inline() {
        let raw = "a".repeat(INLINE_CAP);
        let s: ChzStr = raw.as_str().into();
        assert!(s.is_inline());
        assert_eq!(s.as_str(), raw);
    }

    #[test]
    fn boundary_cap_plus_one_heap() {
        let raw = "a".repeat(INLINE_CAP + 1);
        let s: ChzStr = raw.as_str().into();
        assert!(!s.is_inline());
        assert_eq!(s.as_str(), raw);
    }

    #[test]
    fn from_string_owned_selects_same() {
        let inline: ChzStr = String::from("hi").into();
        let heap: ChzStr = "x".repeat(INLINE_CAP + 5).into();
        assert!(inline.is_inline());
        assert!(!heap.is_inline());
    }

    #[test]
    fn multibyte_utf8_counts_bytes_not_chars() {
        // 8 emoji × 4 bytes = 32 bytes > CAP, even though only 8 chars.
        let raw = "😀".repeat(8);
        assert!(raw.len() > INLINE_CAP);
        let s: ChzStr = raw.as_str().into();
        assert!(!s.is_inline());
        assert_eq!(s.as_str(), raw);
    }

    #[test]
    fn multibyte_utf8_short_is_inline_and_roundtrips() {
        let raw = "héllo"; // 6 bytes (é is 2), ≤ CAP
        let s: ChzStr = raw.into();
        assert!(s.is_inline());
        assert_eq!(s.as_str(), raw);
        assert_eq!(s.chars().count(), 5);
    }

    #[test]
    fn eq_and_hash_match_across_storage() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let a: ChzStr = "same".into();
        let b: ChzStr = "same".into();
        assert_eq!(a, b);
        let h = |s: &ChzStr| {
            let mut hh = DefaultHasher::new();
            s.hash(&mut hh);
            hh.finish()
        };
        assert_eq!(h(&a), h(&b));
        // Hash must equal that of the underlying &str (content semantics).
        let mut sh = DefaultHasher::new();
        "same".hash(&mut sh);
        assert_eq!(h(&a), sh.finish());
    }

    #[test]
    fn obj_size_unchanged_by_sso() {
        // SSO must not grow `Obj` (the heap-slot footprint). `Box<str>`→`ChzStr` adds 8 bytes to
        // the `Str` variant, but `MapData`/`SetData` set the cap at 64B (56B of payload + the 8B enum
        // discriminant), so the total is unchanged. (M19 lever #3 shrank `Closure` from 88B to 64B —
        // captures moved from a `HashMap<String,Value>`(48) to a positional `Vec<Value>`(24); lever #2
        // shrank `Enum` — two per-instance `Box<str>`(32) replaced by one `variant_id: u32`;
        // `Module`'s payload is now boxed (`Box<ModuleData>`), so it no longer sets the cap.)
        assert_eq!(std::mem::size_of::<crate::vm::heap::Obj>(), 64);
    }

    #[test]
    fn deref_gives_str_methods() {
        let s: ChzStr = "Hello, World".into();
        assert!(s.starts_with("Hello"));
        assert_eq!(&s[..5], "Hello");
        assert_eq!(s.to_uppercase(), "HELLO, WORLD");
    }

    #[test]
    fn heap_ascii_string_exposes_its_bytes() {
        let s: ChzStr = "a".repeat(100).into();
        assert_eq!(s.ascii_bytes().map(<[u8]>::len), Some(100));
        assert_eq!(s.char_len(), 100);
    }

    #[test]
    fn heap_non_ascii_string_has_no_ascii_bytes() {
        let s: ChzStr = "é".repeat(100).into();
        assert!(s.ascii_bytes().is_none());
        assert_eq!(s.char_len(), 100);
        assert_eq!(s.as_str().chars().count(), 100);
    }

    #[test]
    fn inline_non_ascii_has_no_ascii_bytes() {
        let s: ChzStr = "héllo".into();
        assert!(s.is_inline());
        assert!(s.ascii_bytes().is_none());
        assert_eq!(s.char_len(), 5);
    }

    #[test]
    fn chzstr_layout_is_unchanged_by_the_ascii_split() {
        assert_eq!(std::mem::size_of::<ChzStr>(), 24);
    }
}
