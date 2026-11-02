//! Track A (Ingest), Featherweight. Reads `/data/requests.log` (239 MB, 3.1M lines of timestamp,
//! method, endpoint, status, latency) and writes `/out/summary.txt`: for every hour and endpoint the
//! request count, nearest-rank p50/p95/p99 latency, 5xx count and 5xx rate in ppm.
//! README.md describes the design in prose; PERF.md has every step with its board run ID.
//!
//! Pipeline (one pool of threads runs all of it, with no barrier; each takes the next task):
//! 1. Parse (`parse`): the input is cut into line-aligned chunks claimed in file order (each thread
//!    finds its chunk's line boundaries itself). Each line
//!    becomes a 4-byte record `latency(20) | 5xx(1) | endpoint id(11)` filed in a block of its
//!    hour in `Pool`; a latency of ESC or more adds an escape word (PERF.md mechanisms 1, 3, 6).
//! 2. Summarise (`summarise`), during the parse: once the parsed prefix of the file holds a line of
//!    hour h + 2, hour h is complete. Count per endpoint, scatter latencies by endpoint, select
//!    p50/p95/p99 (`pct`), keep five words per endpoint, reuse the record blocks (mechanism 10).
//! 3. Output (`write_hour`): `summary.txt` is mapped before the threads start, at a sparse upper
//!    bound. The thread whose chunk completes the parse sorts the ids by name (`Names::key`); then
//!    each hour is formatted straight into the file once it and all earlier hours are summarised.
//!
//! Data structures: `Names` is the endpoint interner shared by all threads (8,192 open-addressing
//! slots, CRC32 hash, name copies padded to 64-byte lines); `Pool` holds the record blocks, listed
//! per hour, and the per-hour statistics tables. Both are lock-free on the hot path; only a new name
//! takes a lock (1,842 times in all).
//!
//! Box-specific decisions (scoring box: i5-13500, THP "always", `/out` is a tmpfs charged as memory;
//! PERF.md "Findings specific to the scoring box"):
//! - Memory: hours are summarised and freed while the parse runs, so the 12.55 MB of records are
//!   never all alive. Board memory about 20 MB to 13.1 MB (mechanism 10, run-eaa93041cda1).
//! - THP: every big buffer is our own `mmap` with MADV_NOHUGEPAGE (`anon`), thread stacks are 256 KB
//!   and malloc keeps one arena, because a 2 MB page is charged whole for its first touched byte.
//! - Page-table work: threads give back their chunk's input pages (`zap`) and each hour's output
//!   pages as they go, and the process ends with `exit(0)` and no frees, so the one-core exit
//!   teardown is nearly empty (mechanism 8: `exit_mmap` below 0.05% of cycles).
//! - Prefetch two lines 1 KB ahead in `parse`: the hardware prefetcher stops at every 4 KB page and
//!   page-cache pages are scattered (mechanism 4: separator-load stall 13.4% to 5.8%).
//! - If a line turns up in an hour already summarised (never in generated data), the whole run
//!   starts over with no early release, so the output is exact for any line order.
use std::arch::x86_64::*;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Mutex, OnceLock};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *const u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

const HOURS: usize = 72; // 2026-10-30T18 to 2026-11-02T17, the same for every seed (spec)
const SLOTS: usize = 8192; // interner slots, a power of two well above 1,842 endpoints
// Latency fields from ESC up are escapes: they keep the latency's low 11 bits, and the next word
// (an escape word, endpoint id 0) holds the rest. Every word then stands alone: the stats loops
// step one word at a time, with no load-dependent stride.
// 2048 = 2^11: a big latency's low 11 bits stay in the record, the rest goes to the escape word.
const ESC: u32 = (1 << 20) - 2048;
const B: usize = 1024; // words per record block: one page, so each page has one writer
// 32 bounds how far claims run ahead of the oldest unparsed chunk (PERF.md mechanism 10): unbounded,
// the parse prefix lagged 31-111 chunks on a loaded laptop and the parse-phase peak reached 15-17.9 MB.
const LAG: usize = 32; // chunks that may be in work past the first one still parsed

/// `len` bytes of fresh anonymous memory without huge pages (the box has THP "always", and a 2 MB
/// page is charged whole): only the 4 KB pages that get touched are charged.
fn anon<T>(len: usize) -> *mut T {
    // SAFETY: a fresh anonymous mapping, so no existing memory is touched. 3 = PROT_READ|PROT_WRITE,
    // 0x4022 = MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE (charged only where touched).
    let p = unsafe { mmap(std::ptr::null_mut(), len.max(1), 3, 0x4022, -1, 0) };
    assert!(p as isize != -1, "mmap {len}");
    // SAFETY: p..p + len is the mapping just made. 15 = MADV_NOHUGEPAGE.
    unsafe { madvise(p, len.max(1), 15) };
    p as *mut T
}

/// MADV_DONTNEED the whole pages inside [p, p + len): unmapped now by this thread, not at exit.
fn zap(p: *const u8, len: usize) {
    let (x, y) = ((p as usize + 4095) & !4095, (p as usize + len) & !4095);
    if y > x {
        // SAFETY: x..y lies inside the caller's range and is whole pages. 4 = MADV_DONTNEED; callers
        // pass only data that nothing reads again (anonymous pages would read back as zeros).
        unsafe { madvise(x as *mut u8, y - x, 4) };
    }
}

/// The input, mapped read-only (the page cache is not charged to us), followed by at least 4 KB of
/// zero pages, so the parser may read past the last byte.
fn map(path: &str) -> &'static [u8] {
    let f = File::open(path).expect(path);
    let len = f.metadata().unwrap().len() as usize;
    assert!(len < 1 << 32);
// SAFETY: len + 4096 bytes of zeroed anonymous memory are reserved (PROT_READ), then the file is
// mapped over the first len bytes with MAP_FIXED (0x12 = MAP_PRIVATE|MAP_FIXED), so the 4 KB tail
// stays zero. The slice stays inside the mapping, is never written, and is unmapped only once every
// chunk is parsed and the run will not start over, when no thread reads it any more. 2 = MADV_SEQUENTIAL.
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + 4096, 1, 0x22, -1, 0);
        assert!(p as isize != -1 && (len == 0 || mmap(p, len, 1, 0x12, f.as_raw_fd(), 0) == p));
        madvise(p, len, 2); // MADV_SEQUENTIAL: unmapping skips the per-page accessed marking
        std::slice::from_raw_parts(p, len)
    }
}

/// Unaligned 8-byte load at b[i], not bounds-checked: the parser may read past the end of a chunk,
/// which is safe because the input is followed by zeros (see `map`).
#[inline(always)]
fn ld8(b: &[u8], i: usize) -> u64 {
    // SAFETY: every caller keeps i + 8 inside the mapping: a line is far shorter than the 4 KB zero tail.
    unsafe { (b.as_ptr().add(i) as *const u64).read_unaligned() }
}

/// Bit k set where b[i + k] is a space, an LF or the zero padding after the input, for k < 64
/// (a signed compare: the input is ASCII).
#[inline(always)]
fn seps(b: &[u8], i: usize) -> u64 {
    unsafe {
// SAFETY: two unaligned 32-byte loads at i..i + 64, inside the mapping by the zero-tail rule of
// `map` (the zeros count as separators, so callers never scan beyond them). AVX2 comes from the
// target features in .cargo/config.toml. 0x21: space (0x20), LF (0x0A) and 0 all compare below it.
        let (p, v) = (b.as_ptr().add(i) as *const __m256i, _mm256_set1_epi8(0x21));
        let m = |k| _mm256_movemask_epi8(_mm256_cmpgt_epi8(v, _mm256_loadu_si256(p.add(k)))) as u32;
        m(0) as u64 | (m(1) as u64) << 32
    }
}

/// Do p[..n] and q[..n] hold the same bytes? Reads whole 64-byte blocks of both.
#[inline(always)]
fn same(p: *const u8, q: *const u8, n: usize) -> bool {
    let mut i = 0;
    loop {
// SAFETY: p points at a name copy in `Names::text` (copies are padded to whole 64-byte lines, so
// the block read is in bounds) and q at the name in the input (zero tail). Bytes past n in the
// last block are shifted out by the final compare, so they never decide the result.
        let ne = unsafe {
            let (p, q) = (p.add(i) as *const __m256i, q.add(i) as *const __m256i);
            let e = |k| {
                let (x, y) = (_mm256_loadu_si256(p.add(k)), _mm256_loadu_si256(q.add(k)));
                _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32
            };
            !(e(0) as u64 | (e(1) as u64) << 32)
        };
        if n - i <= 64 {
            return ne << (64 - (n - i)) == 0;
        }
        if ne != 0 {
            return false;
        }
        i += 64;
    }
}

/// The number whose decimal digits are the low nibbles of v's bytes, most significant digit in the
/// lowest byte (empty bytes at the bottom read as leading zeros). Each multiply joins neighbouring
/// groups: 2561 = 10 << 8 | 1 pairs digits, 6553601 = 100 << 16 | 1 pairs the pairs, and
/// 42949672960001 = 10000 << 32 | 1 joins the quads into the final value.
#[inline(always)]
fn swar(v: u64) -> u32 {
    let v = v & 0x0F0F_0F0F_0F0F_0F0F;
    let v = v.wrapping_mul(2561) >> 8 & 0x00FF_00FF_00FF_00FF;
    let v = v.wrapping_mul(6553601) >> 16 & 0x0000_FFFF_0000_FFFF;
    (v.wrapping_mul(42949672960001) >> 32) as u32
}

/// A number of 8 or more digits at b[i..], and its length.
#[cold]
fn long(b: &[u8], i: usize) -> (u32, usize) {
    let k = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let x = b[i..i + k].iter().fold(0, |x, c| x * 10 + (c & 15) as u32);
    (x, k)
}

/// Endpoint interner shared by all threads. A slot is 0 or crc << 32 | len << 24 | id << 12 | at,
/// where `at` places a copy of the name in `text` in units of 64-byte lines. Copies are padded to
/// whole lines, so a compare reads one line for most names, and never bytes another thread may be
/// writing. Ids start at 1, so no record is 0.
struct Names {
    slots: Box<[AtomicU64; SLOTS]>,
    keys: Box<[AtomicU64; 2048]>, // by id: the copy's offset in `text` << 8 | the name's length
    next: Mutex<(usize, usize)>, // ids handed out, and where the next copy goes
    text: *mut u8,
}
// SAFETY: `slots` and `keys` are atomics. `text` is written only under the `next` lock, into lines
// no reader can reach until the slot that holds their offset is published with Release (readers
// load it with Acquire), and a published copy is never modified again.
unsafe impl Sync for Names {}

impl Names {
    /// The name of endpoint `id` (an id returned by `id`), as the bytes in the input.
    fn name(&self, id: usize) -> &[u8] {
        let k = self.keys[id].load(Acquire) as usize;
        // SAFETY: keys[id] (Acquire) pairs with the Release store in `add`: the copy is fully written.
        unsafe { std::slice::from_raw_parts(self.text.add(k >> 8), k & 255) }
    }

    /// A sort key in the byte order of the names: the first 32 bytes of the zero-padded copy (zero
    /// is below every endpoint byte, so a proper prefix still sorts first), then the whole name.
    fn key(&self, id: usize) -> (u128, u128, &[u8]) {
        let k = self.keys[id].load(Acquire) as usize;
        // SAFETY: as in `name`; a copy is padded to whole 64-byte lines, so 32 bytes are in bounds.
        let p = unsafe { self.text.add(k >> 8) as *const [u8; 16] };
        let be = |i| u128::from_be_bytes(unsafe { *p.add(i) });
        (be(0), be(1), self.name(id))
    }

    /// Id (1..2047) of the endpoint name q[..n] with CRC32 `crc`, adding it if new. Linear probing
    /// from slot `crc`; the hash and length in the slot reject nearly every other name before the compare.
    #[inline(always)]
    fn id(&self, q: *const u8, n: usize, crc: u32) -> u32 {
        let (want, mut i) = ((crc as u64) << 32 | (n as u64) << 24, crc as usize);
        loop {
            let v = self.slots[i % SLOTS].load(Acquire);
            // SAFETY: `at` is 12 bits of 64-byte lines, inside `text`; an empty slot (0) points at line 0,
            // which `same` reads only if the slot matched `want`, and an empty slot never does.
            let t = unsafe { self.text.add((v as usize & 4095) << 6) };
            if v & !0xFF_FFFF == want && same(t, q, n) {
                return (v >> 12) as u32 & 2047;
            }
            if v == 0 {
                return self.add(q, n, want, i);
            }
            i += 1;
        }
    }

    /// Slow path of `id`: under the lock, probe on from slot i (another thread may have added the name
    /// meanwhile), else copy the name into `text` and publish it. Returns its id.
    #[cold]
    fn add(&self, q: *const u8, n: usize, want: u64, mut i: usize) -> u32 {
        let mut next = self.next.lock().unwrap();
        loop {
            let v = self.slots[i % SLOTS].load(Acquire);
            if v == 0 {
                let (id, at) = (next.0 + 1, next.1);
                assert!(id < 2048, "more than 2047 endpoints");
                // SAFETY: we hold the `next` lock, so no one else writes; `at` is a fresh line-aligned offset.
                // In bounds: the spec caps an endpoint at 125 bytes (`"/" *124echar`), so each padded copy
                // takes at most 128 bytes, and the assert above keeps ids below 2048: 2047 x 128 < 2048 x 128.
                unsafe { std::ptr::copy_nonoverlapping(q, self.text.add(at), n) };
                self.keys[id].store((at << 8 | n) as u64, Release);
                *next = (id, at + (n + 63) / 64 * 64);
                self.slots[i % SLOTS].store(want | (id << 12 | at >> 6) as u64, Release);
                return id as u32;
            }
            // SAFETY: as in `id`.
            let t = unsafe { self.text.add((v as usize & 4095) << 6) };
            if v & !0xFF_FFFF == want && same(t, q, n) {
                return (v >> 12) as u32 & 2047;
            }
            i += 1;
        }
    }
}

/// Record words in blocks of B words, listed per hour and reused once their hour is summarised (no
/// page is faulted in, cleared and given back per hour); and the statistics, one table per hour of
/// five words per endpoint id: count, errors, p50, p95, p99.
struct Pool {
    base: *mut u32, // cap blocks, then per hour a list of cap block indices (the blocks it was given)
    cap: usize,
    spare: Mutex<(usize, Vec<u32>)>, // blocks handed out so far, and blocks given back (zeroed)
    used: Vec<AtomicUsize>,          // per hour, blocks in its list
    stats: *mut [[u32; 5]; 2048],
}
// SAFETY: `base` and `stats` point into mappings that live until the last summary. A block has one
// owner at a time (fresh and spare ones are handed out under the `spare` lock), so one writer; an hour's list entries and records are written before its chunks
// join the parsed prefix and read by its summariser only after that; an hour's stats table has one
// writer (its summariser) and is read only after that hour's `bytes` entry is published.
unsafe impl Sync for Pool {}

impl Pool {
    /// A zeroed block of B words, added to hour h's list.
    #[cold]
    fn block(&self, h: usize) -> *mut u32 {
        let mut g = self.spare.lock().unwrap();
        let i = g.1.pop().map_or(g.0, |i| i as usize); // spare blocks are all below g.0
        g.0 += (i == g.0) as usize;
        let k = self.used[h].fetch_add(1, Relaxed);
        assert!(i < self.cap && k < self.cap);
        // SAFETY: i < cap blocks and k < cap list entries (asserted above); entry k is hour h's alone.
        unsafe { *self.base.add((B + h) * self.cap + k) = i as u32 };
        unsafe { self.base.add(i * B) }
    }

    /// Hour h's blocks so far (indices into `base`); call it when no thread is still filing hour h.
    fn list(&self, h: usize) -> &[u32] {
        let n = self.used[h].load(Relaxed);
        // SAFETY: entries below used[h] were written by `block` before the hour's summary.
        unsafe { std::slice::from_raw_parts(self.base.add((B + h) * self.cap), n) }
    }

    /// The words of block i, used by one thread at a time (see the Sync note).
    fn blk(&self, i: u32) -> &mut [u32] {
        // SAFETY: i < cap, so the block lies inside `base`.
        unsafe { std::slice::from_raw_parts_mut(self.base.add(i as usize * B), B) }
    }
}

/// Parse the lines in b[p..] into records (b ends with a chunk; the bytes after it stay mapped), and
/// return the first and last hour seen. cur[h] is where hour h's next record goes; it sits in a
/// B-word block, and a new block is taken when fewer than two words are left.
fn parse(b: &[u8], mut p: usize, names: &Names, pool: &Pool, cur: &mut [*mut u32]) -> [usize; 2] {
    // Lines come in long runs of one hour: keep that hour's cursor in `c` while "MM-DDTHH" repeats.
    let (mut last, mut h, mut span, mut c) = (0, 0, [HOURS, 0], cur[0]);
    while p < b.len() {
        let w = ld8(b, p + 5);
        if w != last {
// w is "MM-DDTHH" (at offset 5 of the line). Below, h = 24 * DD + HH + 744 * [month is 11] - 738:
// 738 = 24 * 30 + 18 puts hour 0 at 10-30T18, and 744 = 31 days of hours carries into November.
            // The digit pairs combine in one multiply (byte k + 10 * byte k-1).
            let x = (w >> 24 & 0xF_0F00_0F0F).wrapping_mul(2561);
            (cur[h], last) = (c, w);
            h = (24 * (x >> 8 & 0xFF) + (x >> 32 & 0xFF) + 744 * (w >> 8 & 1) - 738) as usize;
            c = cur[h];
            span = [span[0].min(h), span[1].max(h)];
        }
        // The hardware prefetcher stops at every 4 KB page (page cache pages are scattered), so ask
        // for the bytes 1 KB ahead now: two cache lines per line, as lines average 77 bytes. A
        // prefetch never faults, even past the end of the mapping.
        let ahead = b.as_ptr().wrapping_add(p + 1024) as *const i8;
        // SAFETY: a prefetch is only a hint and never faults.
        unsafe { _mm_prefetch::<_MM_HINT_T0>(ahead) };
        // SAFETY: as above.
        unsafe { _mm_prefetch::<_MM_HINT_T0>(ahead.wrapping_add(64)) };
// A line is "2026-10-30T18:00:00.100676Z GET /path 200 39234": the timestamp is 27 bytes, so p + 31
// is where the space after the shortest (3-letter) method can sit; later bytes are the endpoint.
        // Separators from the method's third byte on: the method ends at the first, the endpoint
        // at the second.
        let (mut q, mut m) = (p + 31, seps(b, p + 31));
        let s = q + 1 + m.trailing_zeros() as usize;
        m &= m - 1;
        while m == 0 {
            q += 64;
            m = seps(b, q);
        }
        let e = q + m.trailing_zeros() as usize;
        let n = e - s;
// e is the space after the endpoint: status at e + 1..e + 4, latency from e + 5. XOR 0x30 turns
// digits into 0..9; adding 0x76 sets bit 7 of a byte exactly when it is 10 or more (not a digit).
        // The latency's digits run to the first byte that is not a digit (the LF).
        let d = ld8(b, e + 5) ^ 0x3030_3030_3030_3030;
        let k = d.wrapping_add(0x7676_7676_7676_7676) & 0x8080_8080_8080_8080;
        let k = k.trailing_zeros() as usize / 8;
        let (lat, k) = if k < 8 { (swar(d << (64 - 8 * k)), k) } else { long(b, e + 5) };
        p = e + 6 + k;
        // Hash the first 16 and last 8 bytes of the endpoint (all of it when shorter).
        let a = ld8(b, s) & if n >= 8 { !0 } else { (1 << (8 * n)) - 1 };
        let y = if n >= 16 { ld8(b, s + 8) } else { 0 };
        let z = if n >= 8 { ld8(b, e - 8) } else { 0 };
// SAFETY: pure arithmetic; SSE4.2 comes from the target features in .cargo/config.toml.
        let crc = unsafe { _mm_crc32_u64(_mm_crc32_u64(_mm_crc32_u64(n as u64, a), y), z) };
        let id = names.id(b.as_ptr().wrapping_add(s), n, crc as u32);
        let err = (ld8(b, e + 1) as u8 == b'5') as u32;
        if c as usize % (4 * B) >= 4 * B - 8 {
            c = pool.block(h);
        }
// SAFETY: c points into a B-word block with at least two free words (a new block is taken above
// when fewer than two are left), so both stores and the advance stay inside it; the block has one
// writer. The second store is the escape word, overwritten by the next record unless `big`.
        unsafe {
            let big = lat >= ESC;
            *c = id | err << 11 | (lat | if big { ESC } else { 0 }) << 12; // << 12 drops bit 20 up
            *c.add(1) = lat & !2047; // the escape word, kept only if needed
            c = c.add(1 + big as usize);
        }
    }
    cur[h] = c;
    span
}

/// 5xx rate in parts per million, rounded down. n > 0.
fn ppm(e: u32, n: u32) -> u32 {
    (e as u64 * 1_000_000 / n as u64) as u32
}

/// Number of decimal digits of v (1 for 0).
fn digits(v: u32) -> usize {
    v.checked_ilog10().map_or(1, |l| l as usize + 1)
}

/// "00" to "99", as the two bytes of a little-endian u16.
const PAIRS: [u16; 100] = {
    let (mut t, mut i) = ([0; 100], 0);
    while i < 100 {
        t[i] = (b'0' as usize + i / 10 | (b'0' as usize + i % 10) << 8) as u16;
        i += 1;
    }
    t
};

/// Write " v" at o + i, two digits at a time from the right, and return where it ends.
/// SAFETY: the caller sized o's range for exactly these lines (`summarise` counts the same `digits`).
#[inline(always)]
unsafe fn put(o: *mut u8, i: usize, mut v: u32) -> usize {
    let (end, mut j) = (i + 1 + digits(v), i + 1 + digits(v));
    *o.add(i) = b' ';
    while v >= 10 {
        j -= 2;
        (o.add(j) as *mut u16).write_unaligned(PAIRS[(v % 100) as usize]);
        v /= 100;
    }
    if j > i + 1 {
        *o.add(i + 1) = b'0' + v as u8;
    }
    end
}

/// Nearest-rank p50, p95, p99 of v (reorders v).
fn pct(v: &mut [u32]) -> [u32; 3] {
    let n = v.len();
    let r = [50, 95, 99].map(|q| (q * n + 99) / 100 - 1);
    match n {
        0..=8 => return ranks::<1>(v, r),
        9..=16 => return ranks::<2>(v, r),
        17..=32 => return ranks::<4>(v, r),
        _ => {}
    }
    // From n = 33 on the three ranks differ: each select works above the previous one.
    let a = *v.select_nth_unstable(r[0]).1;
    let b = *v[r[0] + 1..].select_nth_unstable(r[1] - r[0] - 1).1;
    [a, b, *v[r[1] + 1..].select_nth_unstable(r[2] - r[1] - 1).1]
}

/// The values at 0-based ranks r of v (at most 8K values) with no branch on the data: in AVX2
/// lanes each value counts the values below (lt) and above (gt) it, and the value at rank r is one
/// with lt <= r and gt <= n - 1 - r. Padding is u32::MAX, which can only match a maximum.
#[inline(always)]
fn ranks<const K: usize>(v: &[u32], r: [usize; 3]) -> [u32; 3] {
    let mut x = [i32::MAX; 32]; // values with the sign bit flipped: signed compares order them
    for (x, &a) in x.iter_mut().zip(v) {
        *x = (a ^ 1 << 31) as i32;
    }
// SAFETY: x holds 32 i32 and the K vectors read from it cover at most 8 * K <= 32 lanes (K is 1,
// 2 or 4 by the callers). AVX2 comes from the target features in .cargo/config.toml.
    unsafe {
        let (z, y) = (_mm256_setzero_si256(), x.as_ptr() as *const __m256i);
        let y: [__m256i; K] = std::array::from_fn(|q| _mm256_loadu_si256(y.add(q)));
        let (mut lt, mut gt) = ([z; K], [z; K]);
        for &a in &x[..v.len()] {
            let a = _mm256_set1_epi32(a);
            for q in 0..K {
                lt[q] = _mm256_sub_epi32(lt[q], _mm256_cmpgt_epi32(y[q], a));
                gt[q] = _mm256_sub_epi32(gt[q], _mm256_cmpgt_epi32(a, y[q]));
            }
        }
        r.map(|r| {
            let a = _mm256_set1_epi32(r as i32);
            let b = _mm256_set1_epi32((v.len() - 1 - r) as i32);
            let miss = |q: usize| _mm256_or_si256(_mm256_cmpgt_epi32(lt[q], a), _mm256_cmpgt_epi32(gt[q], b));
            let m = (0..K).fold(0, |m, q| m | (_mm256_movemask_ps(_mm256_castsi256_ps(miss(q))) as u32) << 8 * q);
            x[(!m).trailing_zeros() as usize] as u32 ^ 1 << 31
        })
    }
}

/// Summarise hour h: count its records per endpoint, scatter their latencies into `lat` grouped
/// by endpoint, select p50, p95 and p99 per group, and zero its blocks for reuse. Returns
/// the size of the hour's output lines.
fn summarise(pool: &Pool, names: &Names, h: usize, lat: &mut [u32]) -> usize {
    // SAFETY: h < HOURS, and each hour is summarised by exactly one thread, which is the only writer
    // of its stats table.
    let (list, st) = (pool.list(h), unsafe { &mut *pool.stats.add(h) });
    // Per endpoint: count | errors << 32, and the next slot. They sit in front of `lat`, not on the
    // thread's stack, so they are unmapped with it before the output grows. 6144 words: cnt is
    // 2048 u64 (4096 words) and at is 2048 u32. SAFETY (the two casts below): cnt covers words
    // 0..4096 and at 4096..6144, so they do not overlap; the scratch is page-aligned.
    let (tmp, lat) = lat.split_at_mut(6144);
    let cnt = unsafe { &mut *(tmp.as_mut_ptr() as *mut [u64; 2048]) };
    let at = unsafe { &mut *(tmp.as_mut_ptr().add(4096) as *mut [u32; 2048]) };
    let (_, mut total) = (cnt.fill(0), 0);
    for &i in list {
        for &w in pool.blk(i).iter() {
            cnt[(w & 2047) as usize] += 1 + ((w as u64 & 0x800) << 21);
        }
    }
    for id in 1..2048 {
        at[id] = total;
        total += cnt[id] as u32;
    }
    // Words of id 0 (escape words, the unused tails of blocks) land after the groups.
    at[0] = total;
    for &k in list {
        let w = pool.blk(k);
        for i in 0..B {
            // The escape word follows its record in its block: a record always has two words of
            // room, so the last word of a block is never a big record and its x goes unused.
            let (v, x) = (w[i] >> 12, w[(i + 1) % B]);
            let c = &mut at[(w[i] & 2047) as usize];
            let v = if v >= ESC { x | v & 2047 } else { v };
            // SAFETY: c < list.len() * B <= lat.len(): each word has its own slot after the prefix sums.
            unsafe { *lat.get_unchecked_mut(*c as usize) = v };
            *c += 1;
        }
        w.fill(0); // ready for reuse
    }
    let (mut lo, mut size) = (0, 0);
    for id in (1..2048).filter(|&id| cnt[id] > 0) {
        let (n, e) = (cnt[id] as u32, (cnt[id] >> 32) as u32);
        let [p50, p95, p99] = pct(&mut lat[lo..lo + n as usize]);
        lo += n as usize;
        st[id] = [n, e, p50, p95, p99];
        let line = [n, p50, p95, p99, e, ppm(e, n)].map(digits);
        size += 28 + names.name(id).len() + line.iter().sum::<usize>();
    }
    pool.spare.lock().unwrap().1.extend_from_slice(list);
    if pool.used[HOURS].fetch_add(1, AcqRel) == HOURS - 1 {
        // The last summary: the records' page tables go too, before the output is complete.
        // SAFETY: the counter says all HOURS summaries are done, so nothing reads the records any more.
        unsafe { munmap(pool.base as *const u8, (B + HOURS) * pool.cap * 4) };
    }
    size
}

/// Write hour h's lines, endpoints in byte order, at p.
/// SAFETY: p has room for exactly these lines (their size came from `summarise`).
unsafe fn write_hour(pool: &Pool, names: &Names, order: &[usize], h: usize, p: *mut u8) {
    let (d, hh) = ((h + 18) / 24 + 30, (h + 18) % 24);
    let (mo, d) = if d > 31 { (11, d - 31) } else { (10, d) };
    let (st, mut i) = (&*pool.stats.add(h), 0);
    let stamp = format!("2026-{mo}-{d:02}T{hh:02}:00:00Z ");
    for &id in order.iter().filter(|&&id| st[id][0] > 0) {
        let ([n, e, p50, p95, p99], k) = (st[id], names.name(id));
        p.add(i).copy_from_nonoverlapping(stamp.as_ptr(), 21);
        p.add(i + 21).copy_from_nonoverlapping(k.as_ptr(), k.len());
        i += 21 + k.len();
        for v in [n, p50, p95, p99, e, ppm(e, n)] {
            i = put(p, i, v);
        }
        *p.add(i) = b'\n';
        i += 1;
    }
}

fn main() {
    // Small thread stacks and one malloc arena: with THP "always" either could cost a 2 MB page.
    std::env::set_var("RUST_MIN_STACK", "262144");
// SAFETY: runs before any thread starts. -8 = M_ARENA_MAX, set to 1 (one malloc arena).
    unsafe { mallopt(-8, 1) };
    let threads = std::thread::available_parallelism().map_or(1, |p| p.get());
    let threads = std::env::var("FW_THREADS").map_or(threads, |v| v.parse().unwrap());
    let b = map("/data/requests.log");
    let slots = Box::new([const { AtomicU64::new(0) }; SLOTS]);
    let keys = Box::new([const { AtomicU64::new(0) }; 2048]);
    // `text` holds the name copies: 128 bytes per possible id, the padded size of a 125-byte endpoint.
    let names = Names { slots, keys, next: Mutex::new((0, 0)), text: anon(2048 * 128) };
    // Blocks for every line (two words each at worst; a line has at least 40 bytes) plus a partly
    // filled block per thread and hour; any one hour may hold them all.
    let cap = b.len() / 19 / B + threads * HOURS + 2;
    // Chunks of 2 MB: threads working on neighbouring chunks then fault in different page-table
    // pages of the input. The last 24 MB go in 512 KB chunks, so threads finish together. Chunk k
    // starts at the first line start at or after edge k; the thread that parses a chunk finds its
    // edges itself (here they would cost a page fault each before any thread starts).
    let big = b.len().saturating_sub(24 << 20) >> 21;
    let chunks = big + ((b.len() - (big << 21) + (1 << 19) - 1) >> 19).max(1);
    let edge = |k: usize| {
        let mut p = ((k.min(big) << 21) + (k.saturating_sub(big) << 19)).min(b.len());
        while p > 0 && p < b.len() && b[p - 1] != b'\n' {
            p += 1;
        }
        p
    };
    // The output is mapped before the threads start (/out is a tmpfs on the box), with room for
    // every hour at its worst (2,047 lines of at most 213 bytes); it is cut to size at the end.
    let cap = HOURS * 2047 * 213;
    let mut f = OpenOptions::new();
    f.read(true).write(true).create(true).truncate(true);
    let f = f.open("/out/summary.txt").expect("create summary.txt");
    f.set_len(cap as u64).expect("size summary.txt");
    // SAFETY: maps the file just sized to cap bytes shared and writable (3, 1 = PROT_READ|PROT_WRITE,
    // MAP_SHARED); it lives until exit, and hours write disjoint ranges of it.
    let o = unsafe { mmap(std::ptr::null_mut(), cap, 3, 1, f.as_raw_fd(), 0) } as usize;
    assert!(o as isize != -1, "map summary.txt");
    // An hour is complete once the parsed prefix of the file holds a line `wait` hours later. With
    // wait = 2 hours are summarised during the parse; if a line then turns up in an hour already
    // summarised, the run starts over with wait = HOURS: no hour is complete before the whole parse.
    for wait in [2, HOURS] {
        let used = (0..=HOURS).map(|_| AtomicUsize::new(0)).collect(); // and hours summarised
        // 40960 = 2048 ids x 5 u32 words x 4 bytes: one stats table per hour.
        let (base, stats) = (anon((B + HOURS) * cap * 4), anon(HOURS * 40960));
        let pool = Pool { base, cap, spare: Default::default(), used, stats };
        // Hours below `fin` are complete; `pre` chunks from the start are parsed. `prog`: for each
        // chunk its first and last hour once parsed; `pre`; one past the last hour in those; whether
        // a line came too late for its hour; chunks whose input pages are given back.
        let (next, fin, pre): ([AtomicUsize; 3], AtomicUsize, AtomicUsize) = Default::default();
        let prog = Mutex::new((vec![None; chunks], 0, 0, false, 0));
        // Each hour's output size, usize::MAX until it is summarised.
        let bytes: Vec<AtomicUsize> = (0..HOURS).map(|_| AtomicUsize::new(usize::MAX)).collect();
        let order: OnceLock<Vec<usize>> = OnceLock::new();
        let work = || {
            let mut cur = [(4 * B - 4) as *mut u32; HOURS];
            // An hour's worst case, after `summarise`'s two tables.
            let n = cap * B + 6144;
            // SAFETY: a private zeroed mapping of n u32, used only by this thread.
            let mut lat = unsafe { std::slice::from_raw_parts_mut(anon(n * 4), n) };
            loop {
                // A complete hour first, to free its record blocks; then the next hour to format, once
                // it and every hour before it are summarised; else the next chunk, but not far past the
                // first one still in work: the records in between would pile up.
                let [j, h, k] = next.each_ref().map(|x| x.load(Relaxed));
                if h >= HOURS && !lat.is_empty() {
                    // Every summary is taken: this thread's scratch goes before the output grows.
                    // SAFETY: the mapping from `anon` above, not used again.
                    unsafe { munmap(std::mem::take(&mut lat).as_ptr() as *const u8, n * 4) };
                }
                if h < fin.load(Acquire) {
                    if next[1].compare_exchange(h, h + 1, Relaxed, Relaxed).is_ok() {
                        bytes[h].store(summarise(&pool, &names, h, lat), Release);
                    }
                } else if k == HOURS {
                    break;
                } else if order.get().is_some() && bytes[..=k].iter().all(|x| x.load(Acquire) != usize::MAX) {
                    if next[2].compare_exchange(k, k + 1, Relaxed, Relaxed).is_ok() {
                        let off: usize = bytes[..k].iter().map(|x| x.load(Relaxed)).sum();
                        let (p, order) = ((o + off) as *mut u8, order.get().unwrap());
                        // SAFETY: bytes[k] is hour k's exact output size, and the range at offset
                        // sum(bytes[..k]) lies inside the mapping (cap covers every hour at its worst).
                        unsafe { write_hour(&pool, &names, order, k, p) };
                        zap(pool.stats.wrapping_add(k) as *const u8, 40960);
                        // Drop this hour's whole output pages from our page tables now, in parallel;
                        // the data stays in the file. Exit would otherwise unmap them all on one core.
                        zap(p, bytes[k].load(Relaxed));
                    }
                } else if j == chunks {
                    std::thread::sleep(std::time::Duration::from_micros(50)); // the last hours are being summarised
                } else if j >= pre.load(Relaxed) + LAG {
                    std::thread::sleep(std::time::Duration::from_micros(50));
                } else if next[0].compare_exchange(j, j + 1, Relaxed, Relaxed).is_ok() {
                    let (lo, hi) = (edge(j), edge(j + 1));
                    let span = parse(&b[..hi], lo, &names, &pool, &mut cur);
                    let mut g = prog.lock().unwrap();
                    g.0[j] = Some(span);
                    while let Some(&Some([a, z])) = g.0.get(g.1) {
                        // Lines of an hour that was given out before this chunk joined the prefix:
                        // its summary may have missed them.
                        g.3 |= a < fin.load(Relaxed);
                        (g.1, g.2) = (g.1 + 1, g.2.max(z + 1));
                    }
                    let (f, z) = (g.2.max(wait) - wait, g.4);
                    // Unmap the parsed input's whole pages 8 chunks at a time (fewer TLB shootdowns),
                    // on the threads, not all at exit on one.
                    if g.1 >= z + 8 {
                        zap(b[edge(z)..].as_ptr(), edge(g.1) - edge(z));
                        g.4 = g.1;
                    }
                    pre.store(g.1, Relaxed);
                    fin.store(if g.1 < chunks { f } else { HOURS }, Release);
                    if g.1 == chunks {
                        if !g.3 {
                            // SAFETY: every chunk is parsed and the run will not start over, so nothing
                            // reads the input again; its page tables go before the output is complete.
                            unsafe { munmap(b.as_ptr(), b.len() + 4096) };
                        }
                        // Every endpoint is named: endpoint ids in byte order of their names, while
                        // the last hours are summarised; hours are formatted once it is set.
                        let mut ids: Vec<usize> = (1..=names.next.lock().unwrap().0).collect();
                        ids.sort_unstable_by_key(|&id| names.key(id));
                        assert!(order.set(ids).is_ok());
                    }
                }
            }
        };
        std::thread::scope(|s| (0..threads).for_each(|_| drop(s.spawn(&work))));
        if !prog.into_inner().unwrap().3 {
            let len: usize = bytes.iter().map(|x| x.load(Relaxed)).sum();
            f.set_len(len as u64).expect("size summary.txt");
            std::process::exit(0); // the output is complete; the kernel releases everything else
        }
    }
}
