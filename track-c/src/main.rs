//! Track C (Join), Featherweight: little memory per unit of speed. Reads `/data/records.csv`
//! (192.6 MB, 3.32M records of a hostile CSV: quoted commas, escaped quotes, embedded LFs, mixed
//! CRLF/LF, stray byte order marks) and `/data/regions.csv`, joins each record to its country, and
//! writes `/out/aggregates.txt`: per country the count, id sum, amount sum, min and max in cents,
//! distinct names, name bytes and note bytes. README.md has the design in prose, PERF.md every
//! board run; the numbers in brackets below are PERF.md mechanism numbers.
//!
//! Pipeline (`main`, `run`; one pool of threads, started once, with barriers between phases):
//! 1. Regions: all threads parse `regions.csv` into a byte table region id -> country number (11).
//! 2. Records: byte ranges (48 per thread) are claimed from a counter; `read` pulls each in 32 KB
//!    pieces, finds records with 64-byte quote/LF bit masks (`inside` is a carry-less multiply) and
//!    `record` finds the fields by the record's shape, not by scanning. Records are batched 32 at a
//!    time and `flush` does the dictionary lookups and aggregation (1, 2, 3, 5).
//! 3. Distinct names: one bit per (country, name id) in a shared bitset; every thread then popcounts
//!    its share. Then the per-thread aggregates are merged, sorted by code, and written (6).
//!
//! Data structures: `Shared` is the name dictionary (64-byte buckets, three slots each, names
//! inline up to 16 bytes, hashed with two AES rounds) plus the bitset; `Agg` is one group's running
//! sums; `Rec` is a parsed record waiting for its lookups.
//!
//! Correctness of the split: a range starts at a guessed record boundary ("LF followed by a digit
//! or a byte order mark") and the guess is proved afterwards: each range must stop exactly where
//! the next one started. Otherwise, or if the dictionary fills, `run` is redone on one thread (10).
//!
//! Box-specific decisions (scoring box: i5-13500, THP "always", every thread faulting on one
//! address-space lock; PERF.md "Findings specific to the scoring box"):
//! - Records are `pread` into a per-thread buffer that stays in L2, not mapped: no page-table
//!   entries to fault in while parsing or to tear down at exit (8; run-6c8cfb2db4cb, 32.68 ms).
//! - Page-table lock: shared tables are mapped up front with MADV_POPULATE_WRITE, a 2 MB page by
//!   exactly one thread; the bitset is updated by queued plain stores under region locks, not by
//!   locked ORs (6, 7, 13).
//! - THP: all memory is MADV_NOHUGEPAGE (a 2 MB page is charged whole to memory.peak) except the
//!   first 6 MB of the dictionary and bitset, which are fully used anyway (9; run-be0296de8c66).
//! - One thread pool, no second round of thread starts (11). The process ends without freeing.
//! - Build flags are in .cargo/config.toml (x86-64-v3 plus PCLMULQDQ and AES; the unsafe intrinsics
//!   below rely on them).
use std::arch::x86_64::*;
use std::sync::{atomic::Ordering::*, atomic::*, Barrier, Mutex};
use std::{fs::File, os::unix::fs::FileExt, os::unix::io::AsRawFd};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

/// n zeroed values in fresh anonymous memory: charged only when touched, never freed (the process
/// ends first), and kept off 2 MB pages (MADV_NOHUGEPAGE). The scoring box backs memory with 2 MB
/// pages, and memory.peak would charge a whole one for its first touched byte.
fn zeroed<T>(n: usize) -> &'static [T] {
    let len = n * std::mem::size_of::<T>();
    // SAFETY: a fresh anonymous mapping; 3 = PROT_READ|PROT_WRITE, 0x22 = MAP_PRIVATE|MAP_ANONYMOUS.
    // Over-allocated by 2 MB so the start can be rounded up to a 2 MB boundary inside it.
    let p = unsafe { mmap(std::ptr::null_mut(), len + (2 << 20), 3, 0x22, -1, 0) }; // RW, anon
    let p = (p as usize + (2 << 20) - 1 & !((2 << 20) - 1)) as *mut u8; // 2 MB aligned
    // SAFETY: p..p + len lies inside the mapping above. 15 = MADV_NOHUGEPAGE.
    unsafe { madvise(p, len, 15) };
    // SAFETY: n zeroed T in the mapping. Used with T = u8 or atomics, for which all-zero is valid, and
    // the memory is never freed, so the 'static lifetime holds.
    unsafe { std::slice::from_raw_parts(p as *const T, n) }
}

/// Map the pages under s writable now (MADV_POPULATE_WRITE). Threads that touch a shared table
/// first would each fault on its pages, twice when a read comes first (the zero page, then a copy),
/// and queue on the same page-table lock.
fn populate<T>(s: &[T]) {
    // SAFETY: madvise only changes page mapping, never contents. 23 = MADV_POPULATE_WRITE. Failure is
    // ignored (an unaligned slice fails with EINVAL; callers pass 4 KB-aligned slices).
    unsafe { madvise(s.as_ptr() as *mut u8, std::mem::size_of_val(s), 23) };
}

/// A file mapped read-only over zeroed memory, so loads may run up to 4 KB past its end. The page
/// cache is not charged to us; a copy would be.
fn map(path: &str) -> &'static [u8] {
    let f = File::open(path).unwrap();
    let len = f.metadata().unwrap().len() as usize;
    let p = zeroed::<u8>(len + 8192).as_ptr() as *mut u8;
    // SAFETY: p is our own zeroed mapping of len + 8192 bytes; MAP_FIXED replaces its first len bytes
    // with the file, leaving the zero tail. The slice below covers only those len bytes.
    let q = unsafe { mmap(p, len, 1, 0x12, f.as_raw_fd(), 0) }; // read-only, private, fixed
    assert!(len == 0 || p == q);
    unsafe { std::slice::from_raw_parts(p, len) }
}

/// Unaligned 8-byte load at b[i], not bounds-checked: the parser may look past a record, which is safe
/// because buffers have zero padding (64 bytes in front in `read`, at least 4 KB behind in `map`).
fn word(b: &[u8], i: usize) -> u64 {
    // SAFETY: callers stay within the padding described above (a record looks back at most 64 bytes).
    unsafe { (b.as_ptr().add(i) as *const u64).read_unaligned() }
}

/// The byte at b[i], read through `word` (so with the same padding rule).
fn at(b: &[u8], i: usize) -> u8 {
    word(b, i) as u8
}

/// Position of the first byte c in b[i..], or b.len().
fn find(b: &[u8], i: usize, c: u8) -> usize {
    i + b[i..].iter().position(|&x| x == c).unwrap_or(b.len() - i)
}

/// The 32 bytes of b from i.
fn load(b: &[u8], i: usize) -> __m256i {
    // SAFETY: 32 bytes inside the padded buffer (see `word`); AVX2 comes from .cargo/config.toml.
    unsafe { _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i) }
}

/// Bit k is set where byte k of v is c.
fn eq(v: __m256i, c: u8) -> u64 {
    // SAFETY: register-only AVX2 (enabled by the build flags).
    unsafe { _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32 as u64 }
}

/// Prefix XOR of the quote bits q (a carry-less multiply by all ones): bit k is set when an odd
/// number of quotes sit at or before k, that is inside a quoted field or on its opening quote. An
/// escaped `""` toggles twice and changes nothing.
fn inside(q: u64) -> u64 {
    // SAFETY: register-only SSE2 and PCLMULQDQ (target features in .cargo/config.toml).
    let (q, ones) = unsafe { (_mm_set_epi64x(0, q as i64), _mm_set1_epi8(-1)) };
    unsafe { _mm_cvtsi128_si64(_mm_clmulepi64_si128(q, ones, 0)) as u64 }
}

/// The numbers whose ASCII digits are the top n[k] bytes of w[k] (1 to 8 of them), all in one
/// vector: the bytes below the digits are masked off, then digits are joined in pairs, the pairs
/// in fours, and the fours in eights. Lane 3 is unused.
fn numbers(w: [u64; 3], n: [usize; 3]) -> [i64; 4] {
// SAFETY: register-only AVX2; the final transmute is __m256i to [i64; 4], the same size.
// Constants: 0x010A are the digit weights (10, 1) of `maddubs`, 0x0001_0064 the weights (100, 1)
// of `madd`, and 10000 joins two 4-digit halves; the first digit is the lowest byte of the lane.
    unsafe {
        let v = _mm256_set_epi64x(0, w[2] as i64, w[1] as i64, w[0] as i64);
        let n = _mm256_set_epi64x(0, n[2] as i64, n[1] as i64, n[0] as i64);
        let below = _mm256_sub_epi64(_mm256_set1_epi64x(64), _mm256_slli_epi64(n, 3));
        let keep = _mm256_sllv_epi64(_mm256_set1_epi8(15), below);
        let v = _mm256_maddubs_epi16(_mm256_and_si256(v, keep), _mm256_set1_epi16(0x010A));
        let v = _mm256_madd_epi16(v, _mm256_set1_epi32(0x0001_0064));
        let hi = _mm256_mul_epu32(v, _mm256_set1_epi64x(10000));
        std::mem::transmute(_mm256_add_epi64(hi, _mm256_srli_epi64(v, 32)))
    }
}

/// Word j of the name b[s..s + n], zero-padded past its end.
fn kw(b: &[u8], s: usize, n: usize, j: usize) -> u64 {
    let len = (8 * n.saturating_sub(8 * j)).min(64) as u32;
    // SAFETY: BMI2 (build flags); bzhi with len = 64 keeps all bits.
    unsafe { _bzhi_u64(word(b, s + 8 * j), len) }
}

/// The end of the text field that starts at p (the first comma outside quotes) and the number of
/// quotes in the field. This works for quoted and unquoted fields alike; a name nearly always fits
/// in the first 32-byte window.
fn text(b: &[u8], p: usize) -> (usize, u32) {
    let (mut i, mut carry, mut quotes) = (p, 0u64, 0);
    loop {
        let v = load(b, i);
        let (q, x) = (eq(v, b'"'), inside(eq(v, b'"')) ^ carry);
        let stop = (eq(v, b',') & !x).trailing_zeros();
        // SAFETY: BMI2 (build flags); stop <= 64, and bzhi keeps the quotes before the stop position.
        let k = quotes + unsafe { _bzhi_u64(q, stop) }.count_ones();
        if stop < 32 {
            return (i + stop as usize, k);
        }
        (i, carry, quotes) = (i + 32, ((x as i64) >> 63) as u64, k);
    }
}

/// Running aggregates of one group: count, id sum, amount sum, min, max, name bytes, note bytes,
/// distinct names (bits this thread counted at the end).
type Agg = [i64; 8];
const NEW: Agg = [0, 0, 0, i64::MAX, i64::MIN, 0, 0, 0];
type Words = &'static [AtomicU64];

/// The state threads share in one run: tab (region id -> group: 0 is _UNMATCHED, countries are
/// 1..); the name dictionary (buckets; words of names over 16 bytes, with the next free word; the
/// next free block of 64 ids); and one bit per (group, name id), with a lock per region of it.
///
/// A dictionary bucket is one cache line with three slots: three 32-bit headers `tag | (id + 1)
/// << 14` (0 = empty, BUSY while being filled; tag = 13 bits of the name's hash and a set bit 13)
/// and three 16-byte keys. A key is the name's first 16 bytes, zero-padded; a name over 16 bytes
/// keeps its length and the rest of its words in `long`, and its key's second word is LONG plus
/// their offset (a name byte is ASCII, so bit 63 is never set by one). A lookup reads one line
/// (prefetched by the parser) and finds its slot with one compare of the three headers; one name
/// in seven has to go on to the next bucket. The raw (still escaped) name is the key: escaping is
/// a bijection, and `""` and an empty unquoted field both give the empty key.
///
/// The bits live in pages of 4096 names: [page][group][64 words], so a group's bits for one page
/// are contiguous and pages past the last id are never touched. No locked instruction per bit: a
/// locked OR waits for its line, which another core has usually just written, and blocks every
/// later load meanwhile. Each thread queues (word, bit) pairs per region of the array (64 regions)
/// and applies a full queue with plain loads and stores under that region's lock. A word always
/// maps to the same lock, so no bit is lost. When all are on, the threads count them by page share.
struct Shared<'a>(&'a [AtomicU8], &'a [Bucket], Words, Ids, Words, Locks);
type Ids = (AtomicUsize, AtomicU32);
type Locks = Vec<Mutex<()>>;

/// Headers in two words (slots 0 and 1, then slot 2 and an always-empty fourth), then the keys.
#[repr(C, align(64))]
struct Bucket([AtomicU64; 2], [[AtomicU64; 2]; 3]);

// A header whose tag bit 13 is clear never occurs for a real name (tags always set it), so this
// value marks a slot being filled by another thread; 0 marks an empty slot.
const BUSY: u32 = 0xFFFF_C000;
// Bit 63 of a key's second word: the name is over 16 bytes and the low bits hold its offset in `long`.
const LONG: u64 = 1 << 63;

/// One thread's state: aggregates per group, its block of free ids, its bit queues (64 regions x 64
/// pairs `word << 6 | bit`, and their lengths), and its read buffer.
type Local = ([Agg; 256], (u32, u32), Pend, *mut u8);
type Pend = ([[u32; 64]; 64], [usize; 64]);

impl Shared<'_> {
    /// Dense id of the name of r (raw bytes b[r.1..r.1 + r.2]). An overfull dictionary sets the id
    /// counter past every limit, and the run is redone.
    fn id(&self, b: &[u8], r: &Rec, ids: &mut (u32, u32)) -> u32 {
        let (buckets, tag, mut i) = (self.1, r.3 as u32, (r.3 >> 32) as usize);
        loop {
            let k = &buckets[i];
            let h = [k.0[0].load(Acquire), k.0[1].load(Acquire)];
            let [mut hit, busy, empty] = lanes(h, tag);
            while hit != 0 {
                let j = hit.trailing_zeros() as usize;
                if self.same(b, r, &k.1[j]) {
                    return ((h[j / 2] >> (32 * (j % 2))) as u32 >> 14) - 1;
                }
                hit &= hit - 1;
            }
            if busy != 0 {
                std::hint::spin_loop(); // another thread is filling a slot here: look again
            } else if empty == 0 {
                i = if i + 1 < buckets.len() { i + 1 } else { 0 };
            } else if let Some(id) = self.insert(b, r, k, empty.trailing_zeros() as usize, ids) {
                return id;
            }
        }
    }

    /// Whether the key at k is the name of r.
    fn same(&self, b: &[u8], r: &Rec, k: &[AtomicU64; 2]) -> bool {
        let (k0, k1, n) = (k[0].load(Relaxed), k[1].load(Relaxed), r.2);
        if n <= 16 {
            return k0 == r.0[0] && k1 == r.0[1];
        }
        let o = (k1 & !LONG) as usize;
        let at = |j: usize| self.2.get(o + j).map(|x| x.load(Relaxed));
        let rest = (1..(n + 7) / 8).all(|j| at(j) == Some(kw(b, r.1, n, j)));
        k0 == r.0[0] && k1 & LONG != 0 && at(0) == Some(n as u64) && rest
    }

    /// Claim empty slot j of bucket k for the name of r and give it the next id; None if another
    /// thread changed the bucket first.
    #[cold]
    fn insert(&self, b: &[u8], r: &Rec, k: &Bucket, j: usize, ids: &mut (u32, u32)) -> Option<u32> {
        let Shared(_, buckets, long, (top, next), ..) = self;
        let (w, shift, n, nw) = (&k.0[j / 2], 32 * (j % 2), r.2, (r.2 + 7) / 8);
        let old = w.load(Relaxed) & !(0xFFFF_FFFF << shift); // slot j empty, or the exchange fails
        let busy = old | (BUSY as u64) << shift;
        w.compare_exchange(old, busy, Acquire, Relaxed).ok()?;
        if ids.0 == ids.1 {
            let first = next.fetch_add(64, Relaxed);
            *ids = (first, first + 64);
            let page = self.4.chunks(1 << 14).nth(first as usize >> 12); // bits of 4096 ids
            page.filter(|_| first % 4096 == 0).map(populate); // mapped by its first id's owner
        }
        let o = (n > 16).then(|| top.fetch_add(nw, Relaxed)).unwrap_or(0);
        if ids.0 as usize >= buckets.len() * 3 / 8 * 7 || o + nw > long.len() {
            next.store(u32::MAX / 2, Relaxed);
            w.fetch_xor((BUSY as u64) << shift, Release);
            return Some(0);
        }
        k.1[j][0].store(r.0[0], Relaxed);
        k.1[j][1].store(if n > 16 { LONG | o as u64 } else { r.0[1] }, Relaxed);
        if n > 16 {
            long[o].store(n as u64, Relaxed);
            (1..nw).for_each(|i| long[o + i].store(kw(b, r.1, n, i), Relaxed));
        }
        let hdr = r.3 as u32 | (ids.0 + 1) << 14;
        w.fetch_xor(((BUSY ^ hdr) as u64) << shift, Release);
        ids.0 += 1;
        Some(ids.0 - 1)
    }

    /// The queued words are fetched for writing first, so their cache misses overlap.
    fn apply(&self, q: &[u32; 64], len: &mut usize, r: usize) {
        for &x in &q[..*len] {
            let p = self.4.as_ptr().wrapping_add(x as usize >> 6) as *const i8;
            // SAFETY: a prefetch for write is only a hint and never faults.
            unsafe { _mm_prefetch(p, _MM_HINT_ET0) };
        }
        let _lock = self.5[r].lock().unwrap();
        for &x in &q[..*len] {
            let a = &self.4[(x >> 6) as usize];
            a.store(a.load(Relaxed) | 1 << (x & 63), Relaxed);
        }
        *len = 0;
    }
}

/// The slots of a bucket with headers h whose tag is `tag`, that are BUSY, and that are empty, as
/// 3-bit masks (the fourth 32-bit lane of the two header words is not a slot and is masked off).
fn lanes(h: [u64; 2], tag: u32) -> [u32; 3] {
// SAFETY: register-only SSE2.
    unsafe {
        let h = _mm_set_epi64x(h[1] as i64, h[0] as i64); // the fourth header is always 0
        let is = |x, v: u32| _mm_cmpeq_epi32(x, _mm_set1_epi32(v as i32));
        let mask = |m| _mm_movemask_ps(_mm_castsi128_ps(m)) as u32 & 7;
        let tags = _mm_and_si128(h, _mm_set1_epi32(0x3FFF));
        [mask(is(tags, tag)), mask(is(h, BUSY)), mask(is(h, 0))]
    }
}

/// A parsed record waiting for its lookups: the name's first 16 bytes (zero-padded), start, length,
/// and bucket << 32 | tag; the region, id and amount; the name's and the note's bytes unescaped.
#[derive(Clone, Copy, Default)]
struct Rec([u64; 2], usize, usize, u64, usize, i64, i64, i64, i64);

/// The record b[rs..=lf] (lf is its terminating LF; `quotes` quotes lie in between). Its fields
/// are found by the shape of a record, `[BOM] id , name , note , region , amount [CR] LF`:
/// - the id's digits from the front (a digit has bit 4 set, a comma does not);
/// - the name's end with a 32-byte quote mask (`text`);
/// - the amount and region from the back: neither holds a comma, so they lie after the last two
///   commas before the terminator;
/// - the note is what remains, and its quotes are the record's minus the name's.
fn record(b: &[u8], rs: usize, lf: usize, quotes: u32, sh: &Shared) -> Rec {
    let p = rs + 3 * (at(b, rs) == 0xEF) as usize; // a byte order mark
// 0xEF starts the 3-byte UTF-8 byte order mark. 0x1010...: a digit (0x30-0x39) has bit 4 set and
// the comma (0x2C) does not, so the first clear bit marks the end of the id.
    let c0 = p + (!word(b, p) & 0x1010_1010_1010_1010).trailing_zeros() as usize / 8;
    let (c1, nq) = text(b, c0 + 1);
    let e = lf - (at(b, lf - 1) == b'\r') as usize;
    let m = eq(load(b, e - 32), b',') as u32;
    let c3 = e - 1 - m.leading_zeros() as usize;
    // c3 is the last comma before the terminator, c2 the one before it (both within 32 bytes of e).
    // SAFETY (bzhi): BMI2 (build flags); c3 + 32 - e < 32 as c3 < e, so only the bits below c3 stay.
    let c2 = e - 1 - unsafe { _bzhi_u32(m, (c3 + 32 - e) as u32) }.leading_zeros() as usize;
// The last 8 bytes before e end in ".dd": the integer digits move up one byte over the dot and
// the two cents digits stay in bytes 6-7, so `numbers` reads one run of digits.
    // The amount `-?d+.dd` is read as cents, with its dot squeezed out.
    let (neg, x) = ((at(b, c3 + 1) == b'-') as usize, word(b, e - 8));
    let x = (x & 0xFF_FFFF_FFFF) << 8 | x & 0xFFFF << 48;
    let w = [word(b, c0 - 8), word(b, c3 - 8), x];
    let [id, region, v, _] = numbers(w, [c0 - p, c3 - c2 - 1, e - c3 - 2 - neg]);
    let quoted = (at(b, c0 + 1) == b'"') as usize;
    let s = c0 + 1 + quoted;
    let (n, tq) = (c1 - s - quoted, (quotes - nq) as usize); // tq: the note's quotes
    // The name's first 16 bytes, zero-padded, and a hash of them and n (two AES rounds).
// SAFETY: a 16-byte load at s stays in the padded buffer; the rest is register-only AES/SSE2.
// The AES round keys are the first hex digits of pi. Bytes past n are masked to zero.
    let (key, h) = unsafe {
        let iota = _mm_set_epi8(15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0);
        let v = _mm_loadu_si128(b.as_ptr().add(s) as *const __m128i);
        let k = _mm_and_si128(v, _mm_cmpgt_epi8(_mm_set1_epi8(n.min(16) as i8), iota));
        let r = _mm_set_epi64x(0x243F_6A88_85A3_08D3, 0x1319_8A2E_0370_7344);
        let h = _mm_aesenc_si128(_mm_xor_si128(k, _mm_cvtsi64_si128(n as i64)), r);
        let h = _mm_cvtsi128_si64(_mm_aesenc_si128(h, r)) as u64;
        (std::mem::transmute(k), h)
    };
    // bucket = (high 32 bits of the hash * bucket count) >> 32, kept in bits 32..; tag = 13 hash bits
    // plus a set bit 13, so that no header is 0 (empty) or BUSY.
    let h = (h >> 32) * sh.1.len() as u64 & !0xFFFF_FFFF | h & 0x1FFF | 0x2000; // bucket, tag
    let (k, region) = (h as usize >> 32, region as usize);
    // SAFETY: k < sh.1.len() by the range reduction above, and a prefetch never faults anyway.
    unsafe { _mm_prefetch(sh.1.as_ptr().add(k) as *const i8, _MM_HINT_T0) }; // the name's bucket
    // SAFETY: a prefetch never faults, even for a region id past the table (wrapping_add).
    unsafe { _mm_prefetch(sh.0.as_ptr().wrapping_add(region) as *const i8, _MM_HINT_T0) };
    let nb = (n + quoted - nq as usize / 2) as i64;
    let tb = (c2 - c1 - 1 - tq / 2 - (tq > 0) as usize) as i64;
    let cents = if neg > 0 { -v } else { v };
    Rec(key, s, n, h, region, id, cents, nb, tb)
}

/// Bytes of records.csv read at a time into a thread's buffer, which stays in its cache. The parse
/// does not map the file: no page-table entries to fault in while reading, none to tear down at
/// exit, and the kernel copies from the page cache in big strides. 32 KB costs no time against 64
/// KB and 0.4 MB less memory; 16 KB costs kernel time.
// The per-thread buffer is PIECE + 192 bytes: 64 in front, the piece, and 128 zero bytes behind.
const PIECE: usize = 1 << 15;

/// Parse the records of f (len bytes) that start in [from, to), one piece at a time through the
/// thread's buffer: 64 bytes in front (a record looks back up to 32), the piece, then zero bytes.
/// Parsing starts at the first line start at or after `from` (`from` itself for the first record
/// of the file) and stops at the first record start at or after `to`, or at `len`; both are
/// returned. In a piece, each 64-byte block gives bit masks of quotes and LFs, and the LFs outside
/// quotes end the records. A record cut by the end of a piece is read again at the start of the
/// next, and the batch is looked up before the buffer is reused.
fn read(f: &File, len: usize, from: usize, to: usize, sh: &Shared, me: &mut Local) -> [usize; 2] {
    let (mut pos, mut start, buf) = (from - 1, None, me.3);
    let (mut batch, mut nb) = ([Rec::default(); 32], 0);
    loop {
        let n = PIECE.min(len - pos);
        // SAFETY: buf is this thread's own buffer of PIECE + 192 bytes (see `run`); n <= PIECE, so
        // 64..64 + n + 64 is inside it, and no other thread has a reference to it.
        let piece = unsafe { std::slice::from_raw_parts_mut(buf.add(64), n + 64) };
        f.read_exact_at(&mut piece[..n], pos as u64).unwrap();
        piece[n..].fill(0); // no stale LF after the data
        // SAFETY: the whole buffer, still only this thread's; the piece above is not used past this point.
        let b = unsafe { std::slice::from_raw_parts(buf, PIECE + 192) };
        let find = || (65..64 + n).find(|&i| line_start(b, i)).unwrap_or(64 + n);
        let first = if start.is_none() { find() } else { 64 };
        start = start.or(Some(pos + first - 64));
        let (end, mut base, mut rs) = (to + 64 - pos, first, first);
        let (mut carry, mut quotes, mut qrs) = (0, 0, 0);
        while rs < end && base < 64 + n {
            let (lo, hi) = (load(b, base), load(b, base + 32));
            let q = eq(lo, b'"') | eq(hi, b'"') << 32;
            let x = inside(q) ^ carry;
            carry = ((x as i64) >> 63) as u64;
            let mut lfs = (eq(lo, b'\n') | eq(hi, b'\n') << 32) & !x;
            while lfs != 0 && rs < end {
                let lf = base + lfs.trailing_zeros() as usize;
                // SAFETY: BMI2 (build flags); lf - base < 64, so bzhi keeps the quotes before the LF.
                let qlf = quotes + unsafe { _bzhi_u64(q, (lf - base) as u32) }.count_ones();
                batch[nb] = record(b, rs, lf, qlf - qrs, sh);
                (nb, qrs, rs, lfs) = (nb + 1, qlf, lf + 1, lfs & (lfs - 1));
                if nb == batch.len() {
                    flush(b, &batch, sh, me);
                    nb = 0;
                }
            }
            (quotes, base) = (quotes + q.count_ones(), base + 64);
        }
        flush(b, &batch[..nb], sh, me);
        (nb, pos) = (0, pos + rs - 64);
        if pos >= to || n < PIECE {
            return [start.unwrap(), pos];
        }
        assert!(rs > first, "a record longer than a piece");
    }
}

/// Lookups for a batch: its region bytes and dictionary buckets were prefetched while it was parsed.
/// The name's bit is queued for its region of the bitset, which is applied when its queue is full.
fn flush(b: &[u8], batch: &[Rec], sh: &Shared, (agg, ids, (q, len), _): &mut Local) {
    for r @ &Rec(.., region, id, cents, nb, tb) in batch {
        let g = sh.0.get(region).map_or(0, |g| g.load(Relaxed)) as usize;
        let name = sh.id(b, r, ids);
        let a = &mut agg[g];
        (a[0], a[1], a[2], a[3]) = (a[0] + 1, a[1] + id, a[2] + cents, a[3].min(cents));
        (a[4], a[5], a[6]) = (a[4].max(cents), a[5] + nb, a[6] + tb);
// word << 6 | bit of the bitset [page][group][64 words]: page << 20 | group << 12 | name & 0xFFF.
// The region (lock) is a function of the word alone, so one word is always updated under one lock.
        let r = (name as usize >> 6 ^ g) & 63; // the region of the name's word
        q[r][len[r]] = (name >> 12) << 20 | (g as u32) << 12 | name & 0xFFF; // word << 6 | bit
        len[r] += 1;
        (len[r] == 64).then(|| sh.apply(&q[r], &mut len[r], r));
    }
}

/// Whether a line starts at b[i], by the shape of the data: a LF followed by a digit or a byte
/// order mark. Inside quotes a LF is always followed by a letter or a quote (spec 2.2), and `run`
/// still proves every record start found this way.
fn line_start(b: &[u8], i: usize) -> bool {
    at(b, i - 1) == b'\n' && (at(b, i).is_ascii_digit() || at(b, i) == 0xEF)
}

/// Run `f` on `threads` threads; the scope waits for them all.
fn par(threads: usize, f: impl Fn() + Sync) {
    std::thread::scope(|s| (0..threads).for_each(|_| drop(s.spawn(&f))));
}

/// Job numbers below n, handed out one at a time.
fn jobs(next: &AtomicUsize, n: usize) -> impl Iterator<Item = usize> + '_ {
    std::iter::repeat_with(|| next.fetch_add(1, Relaxed)).take_while(move |&j| j < n)
}

/// Aggregates and distinct-name counts per group. Round 0 uses all threads; a later round
/// (never needed on generated data) uses one thread and a dictionary 4 times larger. None if a cut
/// was not a record boundary or the dictionary filled up.
///
/// Why a start is proved: job 0 starts right after the header (the first line start after it), a
/// true record start in the outside-quotes state, so its parse is exact and stops at the first
/// record boundary at or after cut(1). If that equals where job 1 started (by the shape of the
/// data), job 1 started at a true record start too, and so on by induction. A start inside a quoted
/// field shows up as a job whose predecessor stopped elsewhere. Jobs find their starts in parallel.
///
/// The threads first run `pre` (the regions jobs; none are left in a later round) and map their
/// share of the dictionary, then wait for each other: one round of thread starts, not two.
fn run<P: Fn() + Sync>(tab: &[AtomicU8], pre: &P, round: u32, threads: usize) -> Option<Vec<Agg>> {
    let threads = if round > 0 { 1 } else { threads };
// cap buckets: 3 << 14 = 49,152 (3.1 MB) in the parallel round, 4 times larger in a redo.
    let cap = 3 << (14 + 2 * round);
    (round > 0).then(|| eprintln!("warning: parallel run rejected; attempt {round} on 1 thread"));
// 48 ranges per thread balance the tail (PERF.md mechanism 10: 8 per thread cost 51.6 against 49.4 ms).
    let k = if threads > 1 { threads * 48 } else { 1 };
    // records.csv is read, not mapped (see `read`); its records start after the header line.
    let (f, mut head) = (File::open("/data/records.csv").unwrap(), [0; 64]);
    let n = f.read_at(&mut head, 0).unwrap();
    let len = f.metadata().unwrap().len() as usize;
    let from = find(&head[..n], 0, b'\n') + 1;
    let cut = |j: usize| from + (len - from) * j / k; // job j's records start at or after cut(j)
    // The dictionary, then the bits (a page per 4096 ids), in one mapping. Its first 6 MB are on 2 MB
    // pages, so random reads there need no page walks; at full scale all of it is used (the
    // dictionary, and the bits of the first 98,304 names), so they cost no memory.
    let n = (cap * 3 / 8 * 7 / 4096 + 1) * 256 * 64;
    let hot = zeroed::<AtomicU64>(cap * 8 + n);
    // SAFETY: the first 6 MB of `hot` (three 2 MB pages) are inside the mapping.
    unsafe { madvise(hot.as_ptr() as *mut u8, 6 << 20, 14) }; // MADV_HUGEPAGE
    let (buckets, bits) = hot.split_at(cap * 8);
    // SAFETY: `Bucket` is 64 bytes of atomics (8 u64), cap * 8 words were split off above, the region
    // is 2 MB aligned (so 64-byte aligned), and zero is a valid (empty) bucket.
    let buckets = unsafe { std::slice::from_raw_parts(buckets.as_ptr() as *const Bucket, cap) };
    let locks = (0..64).map(|_| Mutex::new(())).collect();
    let dict = (buckets, zeroed(1 << 23), (0.into(), 0.into()));
    let sh = Shared(tab, dict.0, dict.1, dict.2, bits, locks);
    let ends: Vec<[AtomicUsize; 2]> = (0..k).map(|_| Default::default()).collect();
    let bufs = zeroed::<u8>(threads * (PIECE + 192)); // one buffer per thread (see `read`)
    let (next, me) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let (wait, total) = (Barrier::new(threads), Mutex::new([NEW; 256])); // total: what threads add
    par(threads, || {
        let t = me.fetch_add(1, Relaxed); // this thread's buffer, and 2 MB page to map
        // One thread maps each 2 MB page: threads racing to fault one in would each clear and charge
        // a 2 MB page, and all but one would then free theirs.
// Threads 0, 1, 2 take one 2 MB page each (1 << 18 words); the others get an empty range.
        populate(&hot[t.min(3) << 18..(t + 1).min(3) << 18]);
        pre();
        wait.wait();
        // Disjoint per-thread slice of `bufs`, which only this thread reads or writes from here on.
        let buf = bufs[t * (PIECE + 192)..].as_ptr() as *mut u8;
        let mut me: Local = ([NEW; 256], (0, 0), ([[0; 64]; 64], [0; 64]), buf);
        for j in jobs(&next, k) {
            let [start, stop] = read(&f, len, cut(j), cut(j + 1), &sh, &mut me);
            ends[j][0].store(start, Relaxed);
            ends[j][1].store(stop, Relaxed);
        }
        (0..64).for_each(|r| sh.apply(&me.2 .0[r], &mut me.2 .1[r], r));
        wait.wait(); // all bits are on: each thread counts them in its share of the used pages
        let w = (sh.3 .1.load(Relaxed) as usize + 4095 >> 12).min(bits.len() >> 14) << 14;
        for i in w * t / threads..w * (t + 1) / threads {
            me.0[i >> 6 & 255][7] += bits[i].load(Relaxed).count_ones() as i64;
        }
        for (s, x) in total.lock().unwrap().iter_mut().zip(&me.0) {
            (s[0], s[1], s[2], s[3]) = (s[0] + x[0], s[1] + x[1], s[2] + x[2], s[3].min(x[3]));
            (s[4], s[5], s[6], s[7]) = (s[4].max(x[4]), s[5] + x[5], s[6] + x[6], s[7] + x[7]);
        }
    });
    let overfull = sh.3 .1.load(Relaxed) as usize >= cap * 3 / 8 * 7;
    let proved = (1..k).all(|j| ends[j - 1][1].load(Relaxed) == ends[j][0].load(Relaxed));
    (proved && !overfull).then(|| total.into_inner().unwrap().to_vec())
}

/// Cents as "[-]d.dd" with at least one integer digit.
fn money(c: i64) -> String {
    let sign = if c < 0 { "-" } else { "" };
    format!("{sign}{}.{:02}", c.abs() / 100, c.abs() % 100)
}

fn main() {
    std::env::set_var("RUST_MIN_STACK", "262144"); // small thread stacks, never on a 2 MB page
    let cores = std::thread::available_parallelism().map_or(1, |p| p.get());
    let threads = std::env::var("FW_THREADS").map_or(cores, |v| v.parse().unwrap());

    // regions.csv: a header, then "id,CCC" lines. tab maps a region id to its group: 0 is
    // _UNMATCHED, and countries are numbered 1.. in order of first sight (codes[g]). Ids ascend, so
    // the last line holds the largest, and ids above it are unknown without a lookup.
    let rg = map("/data/regions.csv");
// The last line is "id,CCC\n": 5 bytes from the comma to the end (the spec ends it with an LF).
    let c = rg.len() - 5; // the comma of the last line
    let p = rg[..c].iter().rposition(|&x| x == b'\n').unwrap() + 1;
    let tab: &[AtomicU8] = zeroed(numbers([word(rg, c - 8), 0, 0], [c - p, 0, 0])[0] as usize + 1);
    let (codes, seen) = (Mutex::new(vec![[255u8; 3]]), zeroed::<AtomicU8>(1 << 15));
    let (from, k, next) = (find(rg, 0, b'\n') + 1, threads * 4, AtomicUsize::new(0));
    let cut = |j: usize| from + (rg.len() - from) * j / k; // job j's lines start at or after cut(j)
    let regions = || {
        for j in jobs(&next, k) {
            let line = (cut(j)..rg.len()).find(|&i| line_start(rg, i));
            let mut p = line.unwrap_or(rg.len());
            while p < cut(j + 1) {
                let c = find(rg, p, b',');
                // SAFETY: BMI2 (build flags). pext packs the low 5 bits of the three letters into a 15-bit code.
                let code = unsafe { _pext_u64(word(rg, c + 1), 0x1F1F1F) } as usize;
                if seen[code].load(Acquire) == 0 {
                    let mut list = codes.lock().unwrap();
                    if seen[code].load(Relaxed) == 0 {
                        list.push(rg[c + 1..c + 4].try_into().unwrap());
                        assert!(list.len() < 256, "more than 255 countries");
                        seen[code].store(list.len() as u8 - 1, Release);
                    }
                }
                let region = numbers([word(rg, c - 8), 0, 0], [c - p, 0, 0])[0] as usize;
                tab[region].store(seen[code].load(Acquire), Relaxed);
                p = c + 5;
            }
        }
    };
    let total = (0..).find_map(|k| run(tab, &regions, k, threads)).unwrap();
    let codes = codes.into_inner().unwrap();
    let mut order: Vec<usize> = (0..codes.len()).filter(|&g| total[g][0] > 0).collect();
    order.sort_by_key(|&g| codes[g]); // _UNMATCHED is [255; 3]: last
    let mut text = String::new();
    for g in order {
        let [n, ids, sum, min, max, nb, tb, d] = total[g];
        let key = std::str::from_utf8(&codes[g]).unwrap_or("_UNMATCHED");
        let (sum, min, max) = (money(sum), money(min), money(max));
        text += &format!("{key} {n} {ids} {sum} {min} {max} {d} {nb} {tb}\n");
    }
    std::fs::write("/out/aggregates.txt", text).unwrap();
}
