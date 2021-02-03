#![allow(clippy::missing_safety_doc)]

use crate::{bits, memcmp, MemchrSearcher};
#[cfg(target_arch = "x86")]
use core::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;
use core::convert::TryInto;
use core::mem;

/// Rolling hash for the simple Rabin-Karp implementation. As a hashing
/// function, the sum of all the bytes is computed.
#[derive(Clone, Copy, Default, PartialEq)]
struct ScalarHash(usize);

impl From<&[u8]> for ScalarHash {
    #[inline]
    fn from(bytes: &[u8]) -> Self {
        bytes.iter().fold(Default::default(), |mut hash, &b| {
            hash.push(b);
            hash
        })
    }
}

impl ScalarHash {
    #[inline]
    fn push(&mut self, b: u8) {
        self.0 = self.0.wrapping_add(b.into());
    }

    #[inline]
    fn pop(&mut self, b: u8) {
        self.0 = self.0.wrapping_sub(b.into());
    }
}

/// Represents an SIMD register type that is x86-specific (but could be used
/// more generically) in order to share functionality between SSE2, AVX2 and
/// possibly future implementations.
trait Vector: Copy {
    unsafe fn set1_epi8(a: i8) -> Self;

    unsafe fn loadu_si(a: *const Self) -> Self;

    unsafe fn cmpeq_epi8(a: Self, b: Self) -> Self;

    unsafe fn and_si(a: Self, b: Self) -> Self;

    unsafe fn movemask_epi8(a: Self) -> i32;
}

impl Vector for __m128i {
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn set1_epi8(a: i8) -> Self {
        _mm_set1_epi8(a)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn loadu_si(a: *const Self) -> Self {
        _mm_loadu_si128(a)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn cmpeq_epi8(a: Self, b: Self) -> Self {
        _mm_cmpeq_epi8(a, b)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn and_si(a: Self, b: Self) -> Self {
        _mm_and_si128(a, b)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn movemask_epi8(a: Self) -> i32 {
        _mm_movemask_epi8(a)
    }
}

impl Vector for __m256i {
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn set1_epi8(a: i8) -> Self {
        _mm256_set1_epi8(a)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn loadu_si(a: *const Self) -> Self {
        _mm256_loadu_si256(a)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn cmpeq_epi8(a: Self, b: Self) -> Self {
        _mm256_cmpeq_epi8(a, b)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn and_si(a: Self, b: Self) -> Self {
        _mm256_and_si256(a, b)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn movemask_epi8(a: Self) -> i32 {
        _mm256_movemask_epi8(a)
    }
}

/// Hash of the first and "last" bytes in the needle for use with the SIMD
/// algorithm implemented by `Avx2Searcher::vector_search_in`. As explained, any
/// byte can be chosen to represent the "last" byte of the hash to prevent
/// worst-case attacks.
struct VectorHash<V: Vector> {
    first: V,
    last: V,
}

impl<V: Vector> VectorHash<V> {
    #[target_feature(enable = "avx2")]
    unsafe fn new(first: u8, last: u8) -> Self {
        Self {
            first: Vector::set1_epi8(first as i8),
            last: Vector::set1_epi8(last as i8),
        }
    }
}

/// Single-substring searcher using an AVX2 algorithm based on the
/// "Generic SIMD" algorithm [presented by Wojciech
/// Muła](http://0x80.pl/articles/simd-strfind.html).
///
/// It is similar to the Rabin-Karp algorithm, except that the hash is
/// not rolling and is calculated for several lanes at once. It begins
/// by picking the first byte in the needle and checking at which
/// positions in the haystack it occurs. Any position where it does not
/// can be immediately discounted as a potential match.
///
/// We then repeat this idea with a second byte in the needle (where the
/// haystack is suitably offset) and take a bitwise AND to further limit
/// the possible positions the needle can match in. Any remaining
/// positions are fully evaluated using an equality comparison with the
/// needle.
///
/// Originally, the algorithm always used the last byte for this second
/// byte. Whilst this is often the most efficient option, it is
/// vulnerable to a worst-case attack and so this implementation instead
/// allows any byte (including a random one) to be chosen.
///
/// In the case where the needle is not a multiple of the number of SIMD
/// lanes, the last chunk is made up of a partial overlap with the
/// penultimate chunk to avoid reading random memory, differing from the
/// original implementation. In this case, a mask is used to prevent
/// performing an equality comparison on the same position twice.
///
/// When the haystack is too short for an AVX2 register, a similar SSE2
/// fallback is used instead. Finally, for very short haystacks there is
/// a scalar Rabin-Karp implementation.
pub struct Avx2Searcher<N: AsRef<[u8]>> {
    needle: N,
    position: usize,
    scalar_hash: ScalarHash,
    sse2_hash: VectorHash<__m128i>,
    avx2_hash: VectorHash<__m256i>,
}

impl<N: AsRef<[u8]>> Avx2Searcher<N> {
    /// Creates a new searcher for `needle`. By default, `position` is
    /// set to the last character in the needle.
    #[target_feature(enable = "avx2")]
    pub unsafe fn new(needle: N) -> Self {
        let needle_bytes = needle.as_ref();

        assert!(!needle_bytes.is_empty());

        let position = needle_bytes.len() - 1;
        Self::with_position(needle, position)
    }

    /// Same as `new` but allows additionally specifying the `position`
    /// to use.
    #[target_feature(enable = "avx2")]
    pub unsafe fn with_position(needle: N, position: usize) -> Self {
        let needle_bytes = needle.as_ref();

        assert!(!needle_bytes.is_empty());
        assert!(position < needle_bytes.len());

        let scalar_hash = ScalarHash::from(needle_bytes);
        let sse2_hash = VectorHash::new(needle_bytes[0], needle_bytes[position]);
        let avx2_hash = VectorHash::new(needle_bytes[0], needle_bytes[position]);

        Self {
            needle,
            position,
            scalar_hash,
            sse2_hash,
            avx2_hash,
        }
    }

    #[inline(always)]
    fn size(&self) -> usize {
        self.needle().len()
    }

    #[inline(always)]
    fn needle(&self) -> &[u8] {
        self.needle.as_ref()
    }

    #[inline]
    fn scalar_search_in(&self, haystack: &[u8]) -> bool {
        debug_assert!(haystack.len() >= self.size());

        let mut end = self.size() - 1;
        let mut hash = ScalarHash::from(&haystack[..end]);

        while end < haystack.len() {
            hash.push(*unsafe { haystack.get_unchecked(end) });
            end += 1;

            let start = end - self.size();
            if hash == self.scalar_hash && haystack[start..end] == self.needle()[..] {
                return true;
            }

            hash.pop(*unsafe { haystack.get_unchecked(start) });
        }

        false
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn vector_search_in_chunk<V: Vector>(
        &self,
        haystack: &[u8],
        hash: &VectorHash<V>,
        start: *const u8,
        mask: i32,
    ) -> bool {
        let first = Vector::loadu_si(start.cast());
        let last = Vector::loadu_si(start.add(self.position).cast());

        let eq_first = Vector::cmpeq_epi8(hash.first, first);
        let eq_last = Vector::cmpeq_epi8(hash.last, last);

        let eq = Vector::and_si(eq_first, eq_last);
        let mut eq = (Vector::movemask_epi8(eq) & mask) as u32;

        let start = start as usize - haystack.as_ptr() as usize;
        let chunk = haystack.as_ptr().add(start + 1);
        let needle = self.needle().as_ptr().add(1);

        while eq != 0 {
            let chunk = chunk.add(eq.trailing_zeros() as usize);

            let comparison = match self.size() {
                2 => memcmp::memcmp1(chunk, needle),
                3 => memcmp::memcmp2(chunk, needle),
                4 => memcmp::memcmp3(chunk, needle),
                5 => memcmp::memcmp4(chunk, needle),
                6 => memcmp::memcmp5(chunk, needle),
                7 => memcmp::memcmp6(chunk, needle),
                8 => memcmp::memcmp7(chunk, needle),
                9 => memcmp::memcmp8(chunk, needle),
                10 => memcmp::memcmp9(chunk, needle),
                11 => memcmp::memcmp10(chunk, needle),
                12 => memcmp::memcmp11(chunk, needle),
                13 => memcmp::memcmp12(chunk, needle),
                n => memcmp::memcmp(chunk, needle, n - 1),
            };

            if comparison {
                return true;
            }

            eq = bits::clear_leftmost_set(eq);
        }

        false
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn vector_search_in<V: Vector>(
        &self,
        haystack: &[u8],
        hash: &VectorHash<V>,
        next: unsafe fn(&Self, &[u8]) -> bool,
    ) -> bool {
        debug_assert!(haystack.len() >= self.size());

        let lanes = mem::size_of::<V>();
        let end = haystack.len() - self.size() + 1;

        if end < lanes {
            return next(self, haystack);
        }

        let mut chunks = haystack[..end].chunks_exact(lanes);
        while let Some(chunk) = chunks.next() {
            if self.vector_search_in_chunk(haystack, hash, chunk.as_ptr(), -1) {
                return true;
            }
        }

        let remainder = chunks.remainder().len();
        if remainder > 0 {
            let start = haystack.as_ptr().add(end - lanes);
            let mask = -1 << (lanes - remainder);

            if self.vector_search_in_chunk(haystack, hash, start, mask) {
                return true;
            }
        }

        false
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn sse2_search_in(&self, haystack: &[u8]) -> bool {
        self.vector_search_in(haystack, &self.sse2_hash, Self::scalar_search_in)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn avx2_search_in(&self, haystack: &[u8]) -> bool {
        self.vector_search_in(haystack, &self.avx2_hash, Self::sse2_search_in)
    }

    /// Inlined version of `search_in` for hot call sites.
    #[inline]
    #[target_feature(enable = "avx2")]
    pub unsafe fn inlined_search_in(&self, haystack: &[u8]) -> bool {
        if haystack.len() < self.size() {
            return false;
        }

        self.avx2_search_in(haystack)
    }

    /// Performs a substring search for the `needle` within `haystack`.
    #[target_feature(enable = "avx2")]
    pub unsafe fn search_in(&self, haystack: &[u8]) -> bool {
        self.inlined_search_in(haystack)
    }
}

/// Single-substring searcher based on `Avx2Searcher` but with dynamic algorithm
/// selection.
///
/// It has specialized cases for zero-length needles, which are found in all
/// haystacks, and one-length needles, which uses `MemchrSearcher`. For needles
/// up to a length of thirteen it uses specialized versions of `Avx2Searcher`,
/// finally falling back to the generic version of `Avx2Searcher` for longer
/// needles.
pub enum DynamicAvx2Searcher<'a> {
    /// Specialization for needles with length 0.
    N0,
    /// Specialization for needles with length 1.
    N1(MemchrSearcher),
    /// Specialization for needles with length 2.
    N2(Avx2Searcher<[u8; 2]>),
    /// Specialization for needles with length 3.
    N3(Avx2Searcher<[u8; 3]>),
    /// Specialization for needles with length 4.
    N4(Avx2Searcher<[u8; 4]>),
    /// Specialization for needles with length 5.
    N5(Avx2Searcher<[u8; 5]>),
    /// Specialization for needles with length 6.
    N6(Avx2Searcher<[u8; 6]>),
    /// Specialization for needles with length 7.
    N7(Avx2Searcher<[u8; 7]>),
    /// Specialization for needles with length 8.
    N8(Avx2Searcher<[u8; 8]>),
    /// Specialization for needles with length 9.
    N9(Avx2Searcher<[u8; 9]>),
    /// Specialization for needles with length 10.
    N10(Avx2Searcher<[u8; 10]>),
    /// Specialization for needles with length 11.
    N11(Avx2Searcher<[u8; 11]>),
    /// Specialization for needles with length 12.
    N12(Avx2Searcher<[u8; 12]>),
    /// Specialization for needles with length 13.
    N13(Avx2Searcher<[u8; 13]>),
    /// Fallback implementation for needles of any size.
    N(Avx2Searcher<&'a [u8]>),
}

impl<'a> DynamicAvx2Searcher<'a> {
    /// Creates a new searcher for `needle`. By default, `position` is set to
    /// the last character in the needle.
    #[target_feature(enable = "avx2")]
    pub unsafe fn new(needle: &'a [u8]) -> Self {
        let position = needle.len() - 1;
        Self::with_position(needle, position)
    }

    /// Same as `new` but allows additionally specifying the `position` to use.
    #[target_feature(enable = "avx2")]
    pub unsafe fn with_position(needle: &'a [u8], position: usize) -> Self {
        assert!(!needle.is_empty());
        assert!(position < needle.len());

        match needle.len() {
            0 => Self::N0,
            1 => Self::N1(MemchrSearcher::new(needle[0])),
            2 => Self::N2(Avx2Searcher::new(needle.try_into().unwrap())),
            3 => Self::N3(Avx2Searcher::new(needle.try_into().unwrap())),
            4 => Self::N4(Avx2Searcher::new(needle.try_into().unwrap())),
            5 => Self::N5(Avx2Searcher::new(needle.try_into().unwrap())),
            6 => Self::N6(Avx2Searcher::new(needle.try_into().unwrap())),
            7 => Self::N7(Avx2Searcher::new(needle.try_into().unwrap())),
            8 => Self::N8(Avx2Searcher::new(needle.try_into().unwrap())),
            9 => Self::N9(Avx2Searcher::new(needle.try_into().unwrap())),
            10 => Self::N10(Avx2Searcher::new(needle.try_into().unwrap())),
            11 => Self::N11(Avx2Searcher::new(needle.try_into().unwrap())),
            12 => Self::N12(Avx2Searcher::new(needle.try_into().unwrap())),
            13 => Self::N13(Avx2Searcher::new(needle.try_into().unwrap())),
            _ => Self::N(Avx2Searcher::new(needle)),
        }
    }

    /// Inlined version of `search_in` for hot call sites.
    #[inline]
    #[target_feature(enable = "avx2")]
    pub unsafe fn inlined_search_in(&self, haystack: &[u8]) -> bool {
        match self {
            Self::N0 => true,
            Self::N1(searcher) => searcher.inlined_search_in(haystack),
            Self::N2(searcher) => searcher.inlined_search_in(haystack),
            Self::N3(searcher) => searcher.inlined_search_in(haystack),
            Self::N4(searcher) => searcher.inlined_search_in(haystack),
            Self::N5(searcher) => searcher.inlined_search_in(haystack),
            Self::N6(searcher) => searcher.inlined_search_in(haystack),
            Self::N7(searcher) => searcher.inlined_search_in(haystack),
            Self::N8(searcher) => searcher.inlined_search_in(haystack),
            Self::N9(searcher) => searcher.inlined_search_in(haystack),
            Self::N10(searcher) => searcher.inlined_search_in(haystack),
            Self::N11(searcher) => searcher.inlined_search_in(haystack),
            Self::N12(searcher) => searcher.inlined_search_in(haystack),
            Self::N13(searcher) => searcher.inlined_search_in(haystack),
            Self::N(searcher) => searcher.inlined_search_in(haystack),
        }
    }

    /// Performs a substring search for the `needle` within `haystack`.
    #[target_feature(enable = "avx2")]
    pub unsafe fn search_in(&self, haystack: &[u8]) -> bool {
        self.inlined_search_in(haystack)
    }
}

#[cfg(test)]
mod tests {
    use super::Avx2Searcher;

    fn avx2_search(haystack: &[u8], needle: &[u8]) -> bool {
        let search = |position| unsafe {
            Avx2Searcher::with_position(needle.to_owned().into_boxed_slice(), position)
                .search_in(haystack)
        };

        let result = search(0);
        for position in 1..needle.len() {
            assert_eq!(search(position), result);
        }

        result
    }

    #[test]
    fn avx2_search_same() {
        assert!(avx2_search(b"foo", b"foo"));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit",
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit"
        ));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus"
        ));
    }

    #[test]
    fn avx2_search_different() {
        assert!(!avx2_search(b"bar", b"foo"));

        assert!(!avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit",
            b"foo"
        ));

        assert!(!avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"foo"
        ));

        assert!(!avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"foo bar baz qux quux quuz corge grault garply waldo fred plugh xyzzy thud"
        ));
    }

    #[test]
    fn avx2_search_prefix() {
        assert!(avx2_search(b"foobar", b"foo"));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit",
            b"Lorem"
        ));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"Lorem"
        ));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit"
        ));
    }

    #[test]
    fn avx2_search_suffix() {
        assert!(avx2_search(b"foobar", b"bar"));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit",
            b"elit"
        ));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"purus"
        ));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"Aliquam iaculis fringilla mi, nec aliquet purus"
        ));
    }

    #[test]
    fn avx2_search_mutiple() {
        assert!(avx2_search(b"foobarfoo", b"foo"));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit",
            b"it"
        ));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"conse"
        ));
    }

    #[test]
    fn avx2_search_middle() {
        assert!(avx2_search(b"foobarfoo", b"bar"));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit",
            b"consectetur"
        ));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"orci"
        ));

        assert!(avx2_search(
            b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas commodo posuere orci a consectetur. Ut mattis turpis ut auctor consequat. Aliquam iaculis fringilla mi, nec aliquet purus",
            b"Maecenas commodo posuere orci a consectetur"
        ));
    }
}