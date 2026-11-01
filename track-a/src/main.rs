// Track A (Ingest), featherweight.
//
// One parse pass turns every line into a 4-byte record (endpoint id, 5xx bit, 20-bit latency; an
// escape word follows the rare larger latency) and files it in a memory region of its own hour.
// Threads take the input in file order, and an hour is summarised while the parse goes on, as soon
// as the parsed prefix of the file has moved two hours past it: count per endpoint, scatter the
// latencies by endpoint, select p50, p95 and p99, and keep five words per endpoint where the
// records were, giving the rest back to the kernel. So only a few hours of records are ever alive.
// The input promises no bound on disorder: if a line turns up in an hour that was already
// summarised, the run starts over and summarises every hour after the whole parse.
// Last, the output file is sized, mapped, and every thread formats whole hours straight into it.
// /out is a tmpfs, so the output is charged as memory: records, statistics and output are never
// all alive at once. One pool of threads runs the phases, with a barrier between them.
use std::arch::x86_64::*;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Barrier, Mutex, OnceLock};

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
const ESC: u32 = (1 << 20) - 2048;
const B: usize = 1024; // words per record block: one page, so each page has one writer
const LAG: usize = 32; // chunks that may be in work past the first one still parsed

/// `len` bytes of fresh anonymous memory without huge pages (the box has THP "always", and a 2 MB
/// page is charged whole): only the 4 KB pages that get touched are charged.
fn anon<T>(len: usize) -> *mut T {
    let p = unsafe { mmap(std::ptr::null_mut(), len.max(1), 3, 0x4022, -1, 0) };
    assert!(p as isize != -1, "mmap {len}");
    unsafe { madvise(p, len.max(1), 15) };
    p as *mut T
}

/// MADV_DONTNEED the whole pages inside [p, p + len): unmapped now by this thread, not at exit.
fn zap(p: *const u8, len: usize) {
    let (x, y) = ((p as usize + 4095) & !4095, (p as usize + len) & !4095);
    if y > x {
        unsafe { madvise(x as *mut u8, y - x, 4) };
    }
}

/// The input, mapped read-only (the page cache is not charged to us), followed by at least 4 KB of
/// zero pages, so the parser may read past the last byte.
fn map(path: &str) -> &'static [u8] {
    let f = File::open(path).expect(path);
    let len = f.metadata().unwrap().len() as usize;
    assert!(len < 1 << 32);
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + 4096, 1, 0x22, -1, 0);
        assert!(p as isize != -1 && (len == 0 || mmap(p, len, 1, 0x12, f.as_raw_fd(), 0) == p));
        madvise(p, len, 2); // MADV_SEQUENTIAL: unmapping skips the per-page accessed marking
        std::slice::from_raw_parts(p, len)
    }
}

#[inline(always)]
fn ld8(b: &[u8], i: usize) -> u64 {
    unsafe { (b.as_ptr().add(i) as *const u64).read_unaligned() }
}

/// Bit k set where b[i + k] is a space, an LF or the zero padding after the input, for k < 64
/// (a signed compare: the input is ASCII).
#[inline(always)]
fn seps(b: &[u8], i: usize) -> u64 {
    unsafe {
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
/// lowest byte (empty bytes at the bottom read as leading zeros).
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
    next: Mutex<(usize, usize)>,  // ids handed out, and where the next copy goes
    text: *mut u8,
}
unsafe impl Sync for Names {}

impl Names {
    fn name(&self, id: usize) -> &[u8] {
        let k = self.keys[id].load(Acquire) as usize;
        unsafe { std::slice::from_raw_parts(self.text.add(k >> 8), k & 255) }
    }

    #[inline(always)]
    fn id(&self, q: *const u8, n: usize, crc: u32) -> u32 {
        let (want, mut i) = ((crc as u64) << 32 | (n as u64) << 24, crc as usize);
        loop {
            let v = self.slots[i % SLOTS].load(Acquire);
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

    #[cold]
    fn add(&self, q: *const u8, n: usize, want: u64, mut i: usize) -> u32 {
        let mut next = self.next.lock().unwrap();
        loop {
            let v = self.slots[i % SLOTS].load(Acquire);
            if v == 0 {
                let (id, at) = (next.0 + 1, next.1);
                assert!(id < 2048, "more than 2047 endpoints");
                unsafe { std::ptr::copy_nonoverlapping(q, self.text.add(at), n) };
                self.keys[id].store((at << 8 | n) as u64, Release);
                *next = (id, at + (n + 63) / 64 * 64);
                self.slots[i % SLOTS].store(want | (id << 12 | at >> 6) as u64, Release);
                return id as u32;
            }
            let t = unsafe { self.text.add((v as usize & 4095) << 6) };
            if v & !0xFF_FFFF == want && same(t, q, n) {
                return (v >> 12) as u32 & 2047;
            }
            i += 1;
        }
    }
}

/// Record words, one region per hour, each handed out in blocks of B words; and the statistics,
/// one table per hour of five words per endpoint id: count, errors, p50, p95, p99.
struct Pool {
    base: *mut u32,
    region: usize,
    used: Vec<AtomicUsize>,
    stats: *mut [[u32; 5]; 2048],
}
unsafe impl Sync for Pool {}

impl Pool {
    fn block(&self, h: usize) -> *mut u32 {
        let k = self.used[h].fetch_add(1, Relaxed);
        assert!((k + 1) * B < self.region); // a spare word: the scatter reads one word ahead
        unsafe { self.base.add(h * self.region + k * B) }
    }

    fn words(&self, h: usize) -> &[u32] {
        let n = self.used[h].load(Relaxed) * B;
        unsafe { std::slice::from_raw_parts(self.base.add(h * self.region), n) }
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
        unsafe { _mm_prefetch::<_MM_HINT_T0>(ahead) };
        unsafe { _mm_prefetch::<_MM_HINT_T0>(ahead.wrapping_add(64)) };
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
        let crc = unsafe { _mm_crc32_u64(_mm_crc32_u64(_mm_crc32_u64(n as u64, a), y), z) };
        let id = names.id(b.as_ptr().wrapping_add(s), n, crc as u32);
        let err = (ld8(b, e + 1) as u8 == b'5') as u32;
        if c as usize % (4 * B) >= 4 * B - 8 {
            c = pool.block(h);
        }
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

fn ppm(e: u32, n: u32) -> u32 {
    (e as u64 * 1_000_000 / n as u64) as u32
}

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
/// SAFETY: the caller sized o's range for exactly these lines.
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
/// by endpoint, select p50, p95 and p99 per group, and give the records back to the kernel. Returns
/// the size of the hour's output lines.
fn summarise(pool: &Pool, names: &Names, h: usize, lat: &mut [u32]) -> usize {
    let (words, st) = (pool.words(h), unsafe { &mut *pool.stats.add(h) });
    // Per endpoint: count | errors << 32, and the next slot. They sit in front of `lat`, not on the
    // thread's stack, so they are unmapped with it before the output is charged.
    let (tmp, lat) = lat.split_at_mut(6144);
    let cnt = unsafe { &mut *(tmp.as_mut_ptr() as *mut [u64; 2048]) };
    let at = unsafe { &mut *(tmp.as_mut_ptr().add(4096) as *mut [u32; 2048]) };
    let (_, mut total) = (cnt.fill(0), 0);
    for &w in words {
        cnt[(w & 2047) as usize] += 1 + ((w as u64 & 0x800) << 21);
    }
    for id in 1..2048 {
        at[id] = total;
        total += cnt[id] as u32;
    }
    // Words of id 0 (escape words, the unused tails of blocks) land after the groups.
    at[0] = total;
    for (i, &w) in words.iter().enumerate() {
        let (v, x) = (w >> 12, unsafe { *words.as_ptr().add(i + 1) });
        let c = &mut at[(w & 2047) as usize];
        let v = if v >= ESC { x | v & 2047 } else { v };
        // c < words.len() <= lat.len(): every word has a slot.
        unsafe { *lat.get_unchecked_mut(*c as usize) = v };
        *c += 1;
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
    zap(words.as_ptr() as *const u8, words.len() * 4 + 4095); // regions are page-aligned
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
    unsafe { mallopt(-8, 1) };
    let threads = std::thread::available_parallelism().map_or(1, |p| p.get());
    let threads = std::env::var("FW_THREADS").map_or(threads, |v| v.parse().unwrap());
    let b = map("/data/requests.log");
    let slots = Box::new([const { AtomicU64::new(0) }; SLOTS]);
    let keys = Box::new([const { AtomicU64::new(0) }; 2048]);
    let names = Names { slots, keys, next: Mutex::new((0, 0)), text: anon(2048 * 128) };
    // Any hour may hold every line (two words each at worst) plus a partly filled block per thread.
    let region = (b.len() / 19 + (threads + 1) * B + 1023) & !1023;
    // Line-aligned chunks, cut just after every 2 MB mark: threads working on neighbouring chunks
    // then fault in different page-table pages of the input.
    let mut cut = vec![0];
    for mut p in (1 << 21..b.len()).step_by(1 << 21) {
        while b[p - 1] != b'\n' {
            p += 1;
        }
        cut.push(p);
    }
    cut.push(b.len());
    let parts: Vec<_> = cut.windows(2).map(|w| (w[0], w[1])).collect();
    // An hour is complete once the parsed prefix of the file holds a line `wait` hours later. With
    // wait = 2 hours are summarised during the parse; if a line then turns up in an hour already
    // summarised, the run starts over with wait = HOURS: no hour is complete before the whole parse.
    for wait in [2, HOURS] {
        let used = (0..HOURS).map(|_| AtomicUsize::new(0)).collect();
        let pool = Pool { base: anon(HOURS * region * 4), region, used, stats: anon(HOURS * 40960) };
        // Hours below `fin` are complete; `pre` chunks from the start are parsed. `prog`: for each
        // chunk its first and last hour once parsed; `pre`; one past the last hour in those; whether
        // a line came too late for its hour.
        let (next, fin, pre): ([AtomicUsize; 3], AtomicUsize, AtomicUsize) = Default::default();
        let prog = Mutex::new((vec![None; parts.len()], 0, 0, false));
        let bytes: Vec<AtomicUsize> = (0..HOURS).map(|_| Default::default()).collect();
        let (bar, order, out) = (Barrier::new(threads), OnceLock::new(), OnceLock::new());
        let work = || {
            let mut cur = [(4 * B - 4) as *mut u32; HOURS];
            // Twice an hour's worst case (a line that comes too late may race a summary), after
            // `summarise`'s two tables.
            let n = region * 2 + 6144;
            let lat = unsafe { std::slice::from_raw_parts_mut(anon(n * 4), n) };
            loop {
                // A complete hour first, to give its records back; else the next chunk, but not far
                // past the first one still in work: the records in between would pile up.
                let (h, j) = (next[1].load(Relaxed), next[0].load(Relaxed));
                if h < fin.load(Acquire) {
                    if next[1].compare_exchange(h, h + 1, Relaxed, Relaxed).is_ok() {
                        bytes[h].store(summarise(&pool, &names, h, lat), Relaxed);
                    }
                } else if j == parts.len() {
                    break;
                } else if j >= pre.load(Relaxed) + LAG {
                    std::thread::sleep(std::time::Duration::from_micros(50));
                } else if next[0].compare_exchange(j, j + 1, Relaxed, Relaxed).is_ok() {
                    let (lo, hi) = parts[j];
                    let span = parse(&b[..hi], lo, &names, &pool, &mut cur);
                    // Unmap the chunk's whole pages now, on this thread, not all at exit on one.
                    zap(b[lo..].as_ptr(), hi - lo);
                    let mut g = prog.lock().unwrap();
                    g.0[j] = Some(span);
                    while let Some(&Some([a, z])) = g.0.get(g.1) {
                        // Lines of an hour that was given out before this chunk joined the prefix:
                        // its summary may have missed them.
                        g.3 |= a < fin.load(Relaxed);
                        (g.1, g.2) = (g.1 + 1, g.2.max(z + 1));
                    }
                    let f = g.2.max(wait) - wait;
                    pre.store(g.1, Relaxed);
                    fin.store(if g.1 < parts.len() { f } else { HOURS }, Release);
                }
            }
            if bar.wait().is_leader() {
                // Endpoint ids in byte order of their names.
                let mut ids: Vec<usize> = (1..=names.next.lock().unwrap().0).collect();
                ids.sort_unstable_by_key(|&id| names.name(id));
                assert!(order.set(ids).is_ok());
            }
            if prog.lock().unwrap().3 {
                return; // start over
            }
            while let h @ 0..HOURS = next[1].fetch_add(1, Relaxed) {
                bytes[h].store(summarise(&pool, &names, h, lat), Relaxed);
            }
            unsafe { munmap(lat.as_ptr() as *const u8, n * 4) };

            if bar.wait().is_leader() {
                // The page tables of the input and of the records go too, before the output is
                // charged. The output size is now known: map the file (/out is a tmpfs on the box).
                unsafe { munmap(b.as_ptr(), b.len() + 4096) };
                unsafe { munmap(pool.base as *const u8, HOURS * region * 4) };
                let mut off = vec![0];
                for x in &bytes {
                    off.push(off.last().unwrap() + x.load(Relaxed));
                }
                let mut f = OpenOptions::new();
                f.read(true).write(true).create(true).truncate(true);
                let f = f.open("/out/summary.txt").expect("create summary.txt");
                let len = off[HOURS];
                f.set_len(len as u64).expect("size summary.txt");
                let p = unsafe { mmap(std::ptr::null_mut(), len.max(1), 3, 1, f.as_raw_fd(), 0) };
                assert!(p as isize != -1, "map summary.txt");
                assert!(out.set((p as usize, off)).is_ok());
            }
            bar.wait();
            let ((o, off), order) = (out.get().unwrap(), order.get().unwrap());
            // Each hour: format its lines into its range of the output.
            while let h @ 0..HOURS = next[2].fetch_add(1, Relaxed) {
                let (p, len) = ((o + off[h]) as *mut u8, off[h + 1] - off[h]);
                unsafe { write_hour(&pool, &names, order, h, p) };
                zap(pool.stats.wrapping_add(h) as *const u8, 40960);
                // Drop this hour's whole output pages from our page tables now, in parallel; the
                // data stays in the file. Exit would otherwise unmap them all on one core.
                zap(p, len);
            }
        };
        std::thread::scope(|s| (0..threads).for_each(|_| drop(s.spawn(&work))));
        if !prog.into_inner().unwrap().3 {
            std::process::exit(0); // the output is complete; the kernel releases everything else
        }
    }
}
