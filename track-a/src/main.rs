// Track A (Ingest), featherweight.
//
// One parse pass turns every line into a 4-byte record (endpoint id, 5xx bit, 20-bit latency; an
// escape word follows the rare larger latency) and files it in a memory region of its own hour.
// Then each hour on its own: count per endpoint, scatter the latencies by endpoint, select p50, p95
// and p99, and keep six words per group where the records were, giving the rest back to the kernel.
// Last, the output file is sized, mapped, and every thread formats whole hours straight into it.
// /out is a tmpfs, so the output is charged as memory: records, statistics and output are never
// all alive at once. One pool of threads runs the three phases, with a barrier between them.
use std::arch::x86_64::*;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Barrier, Mutex, OnceLock};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

const HOURS: usize = 72; // 2026-10-30T18 to 2026-11-02T17, the same for every seed (spec)
const SLOTS: usize = 8192; // interner slots, a power of two well above 1,842 endpoints
const ESC: u32 = (1 << 20) - 1; // latency field of a record whose latency is in the next word
const B: usize = 128; // words per record block

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

/// Split `b` into `k` byte ranges that start and end on line boundaries.
fn chunks(b: &[u8], k: usize) -> Vec<(usize, usize)> {
    let mut cut = vec![0];
    for j in 1..k {
        let mut p = (b.len() * j / k).max(*cut.last().unwrap());
        while p > 0 && p < b.len() && b[p - 1] != b'\n' {
            p += 1;
        }
        cut.push(p);
    }
    cut.push(b.len());
    cut.windows(2).map(|w| (w[0], w[1])).collect()
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
            let e = |k| _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(k)), _mm256_loadu_si256(q.add(k)))) as u32;
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
    (b[i..i + k].iter().fold(0, |x, &c| x * 10 + (c - b'0') as u32), k)
}

/// Endpoint interner shared by all threads. A slot is 0 or crc << 32 | len << 24 | id << 12 | at,
/// where `at` places a copy of the name in `text` in units of 64-byte lines. Copies are padded to
/// whole lines, so a compare reads one line for most names, and never bytes another thread may be
/// writing. Ids start at 1, so no record is 0.
struct Names {
    slots: Box<[AtomicU64; SLOTS]>,
    keys: Mutex<Vec<(usize, usize)>>,
    text: *mut u8,
}
unsafe impl Sync for Names {}

impl Names {
    fn text(&self, (at, n): (usize, usize)) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.text.add(at), n) }
    }

    #[inline(always)]
    fn id(&self, q: *const u8, n: usize, crc: u32) -> u32 {
        let (want, mut i) = ((crc as u64) << 32 | (n as u64) << 24, crc as usize);
        loop {
            let v = self.slots[i % SLOTS].load(Acquire);
            if v & !0xFF_FFFF == want && same(unsafe { self.text.add((v as usize & 4095) << 6) }, q, n) {
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
        let mut keys = self.keys.lock().unwrap();
        loop {
            let v = self.slots[i % SLOTS].load(Acquire);
            if v == 0 {
                let at = keys.last().map_or(0, |&(o, n)| o + (n + 63) / 64 * 64);
                unsafe { std::ptr::copy_nonoverlapping(q, self.text.add(at), n) };
                keys.push((at, n));
                assert!(keys.len() < 2048, "more than 2047 endpoints");
                self.slots[i % SLOTS].store(want | (keys.len() << 12 | at >> 6) as u64, Release);
                return keys.len() as u32;
            }
            if v & !0xFF_FFFF == want && same(unsafe { self.text.add((v as usize & 4095) << 6) }, q, n) {
                return (v >> 12) as u32 & 2047;
            }
            i += 1;
        }
    }
}

/// Record words, one region per hour; each region is handed out in blocks of B words.
struct Pool {
    base: *mut u32,
    region: usize,
    used: Vec<AtomicUsize>,
}
unsafe impl Sync for Pool {}

impl Pool {
    fn block(&self, h: usize) -> *mut u32 {
        let k = self.used[h].fetch_add(1, Relaxed);
        assert!((k + 1) * B <= self.region);
        unsafe { self.base.add(h * self.region + k * B) }
    }

    fn words(&self, h: usize) -> &[u32] {
        unsafe { std::slice::from_raw_parts(self.base.add(h * self.region), self.used[h].load(Relaxed) * B) }
    }

    /// Hour h's region seen as group statistics (it holds them once its records are consumed).
    fn stats(&self, h: usize) -> &mut [[u32; 6]] {
        unsafe { std::slice::from_raw_parts_mut(self.base.add(h * self.region) as *mut [u32; 6], 2048) }
    }

    /// Give bytes [lo, hi) of hour h's region, widened to whole pages at the top, back to the kernel.
    fn free(&self, h: usize, lo: usize, hi: usize) {
        let (lo, hi) = ((lo + 4095) & !4095, (hi + 4095) & !4095);
        if hi > lo {
            unsafe { madvise((self.base.add(h * self.region) as *mut u8).add(lo), hi - lo, 4) };
        }
    }
}

/// Parse the lines in b[lo..hi] into records. cur[h] is where hour h's next record goes; it sits
/// in a B-word block, and a new block is taken when fewer than two words are left.
fn parse(b: &[u8], lo: usize, hi: usize, names: &Names, pool: &Pool, cur: &mut [*mut u32; HOURS]) {
    let mut p = lo;
    while p < hi {
        // "MM-DDTHH": the digit pairs combine in one multiply (byte k + 10 * byte k-1).
        let w = ld8(b, p + 5);
        let x = (w >> 24 & 0xF_0F00_0F0F).wrapping_mul(2561);
        let h = (24 * (x >> 8 & 0xFF) + (x >> 32 & 0xFF) + 744 * (w >> 8 & 1) - 738) as usize;
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
        let k = (d.wrapping_add(0x7676_7676_7676_7676) & 0x8080_8080_8080_8080).trailing_zeros() as usize / 8;
        let (lat, k) = if k < 8 { (swar(d << (64 - 8 * k)), k) } else { long(b, e + 5) };
        p = e + 6 + k;
        // Hash the first 16 and last 8 bytes of the endpoint (all of it when shorter).
        let (a, z) = if n >= 8 { (ld8(b, s), ld8(b, e - 8)) } else { (ld8(b, s) & ((1 << (8 * n)) - 1), 0) };
        let c = if n >= 16 { ld8(b, s + 8) } else { 0 };
        let crc = unsafe { _mm_crc32_u64(_mm_crc32_u64(_mm_crc32_u64(n as u64, a), c), z) };
        let id = names.id(b.as_ptr().wrapping_add(s), n, crc as u32);
        let err = (ld8(b, e + 1) as u8 == b'5') as u32;
        let c = &mut cur[h];
        if *c as usize % (4 * B) >= 4 * B - 8 {
            *c = pool.block(h);
        }
        unsafe {
            **c = id | err << 11 | lat.min(ESC) << 12;
            *c = c.add(1);
            if lat >= ESC {
                **c = lat;
                *c = c.add(1);
            }
        }
    }
}

fn ppm(e: u32, n: u32) -> u32 {
    (e as u64 * 1_000_000 / n as u64) as u32
}

fn digits(v: u32) -> usize {
    v.checked_ilog10().map_or(1, |l| l as usize + 1)
}

/// "00" to "99".
const PAIRS: [u8; 200] = {
    let (mut t, mut i) = ([0; 200], 0);
    while i < 200 {
        t[i] = b'0' + (i / 20) as u8;
        t[i + 1] = b'0' + (i / 2 % 10) as u8;
        i += 2;
    }
    t
};

/// Write " v" at o[*i..] and advance.
#[inline(always)]
fn put(o: &mut [u8], i: &mut usize, mut v: u32) {
    let mut j = *i + 1 + digits(v);
    o[*i] = b' ';
    *i = j;
    while v >= 100 {
        let r = (v % 100) as usize * 2;
        o[j - 2..j].copy_from_slice(&PAIRS[r..r + 2]);
        (j, v) = (j - 2, v / 100);
    }
    if v >= 10 {
        o[j - 2..j].copy_from_slice(&PAIRS[v as usize * 2..v as usize * 2 + 2]);
    } else {
        o[j - 1] = b'0' + v as u8;
    }
}

/// Nearest-rank p50, p95, p99 of v (reorders v).
fn pct(v: &mut [u32]) -> [u32; 3] {
    let n = v.len();
    let r = [50, 95, 99].map(|q| (q * n + 99) / 100 - 1);
    if n <= SMALL {
        v.sort_unstable();
        return r.map(|i| v[i]);
    }
    let (mut out, mut lo) = ([0u32; 3], 0);
    for k in 0..3 {
        out[k] = if r[k] >= lo { *v[lo..].select_nth_unstable(r[k] - lo).1 } else { out[k - 1] };
        lo = lo.max(r[k] + 1);
    }
    out
}
const SMALL: usize = 32;

/// A pointer that threads share, writing disjoint ranges.
struct Ptr(*mut u8);
unsafe impl Send for Ptr {}
unsafe impl Sync for Ptr {}

fn main() {
    // Small thread stacks and one malloc arena: with THP "always" either could cost a 2 MB page.
    std::env::set_var("RUST_MIN_STACK", "262144");
    unsafe { mallopt(-8, 1) };
    let threads = std::env::var("FW_THREADS").ok().and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |p| p.get()));
    let b = map("/data/requests.log");
    let names = Names { slots: Box::new([const { AtomicU64::new(0) }; SLOTS]), keys: Mutex::new(Vec::new()), text: anon(2048 * 128) };
    // Any hour may hold every line (two words each at worst) plus a partly filled block per thread,
    // and later 2,048 groups of six words.
    let region = (b.len() / 19 + (threads + 1) * B + 6 * 2048 + 1023) & !1023;
    let pool = Pool { base: anon(HOURS * region * 4), region, used: (0..HOURS).map(|_| AtomicUsize::new(0)).collect() };
    let parts = chunks(b, threads * 16);
    let next: [AtomicUsize; 3] = Default::default();
    let (bytes, groups): (Vec<AtomicUsize>, Vec<AtomicUsize>) = (0..HOURS).map(|_| Default::default()).unzip();
    let (bar, order, out) = (Barrier::new(threads), OnceLock::new(), OnceLock::new());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut cur = [(4 * B - 4) as *mut u32; HOURS];
                while let Some(&(lo, hi)) = parts.get(next[0].fetch_add(1, Relaxed)) {
                    parse(b, lo, hi, &names, &pool, &mut cur);
                    // Unmap the chunk's whole pages now, on this thread, not all at exit on one.
                    zap(b[lo..].as_ptr(), hi - lo);
                }
                if bar.wait().is_leader() {
                    // Endpoint ids in byte order of their names.
                    let keys = names.keys.lock().unwrap();
                    let mut ids: Vec<usize> = (1..=keys.len()).collect();
                    ids.sort_unstable_by_key(|&id| names.text(keys[id - 1]));
                    // Hours largest first, so the last ones to start are short.
                    let mut hrs: Vec<usize> = (0..HOURS).collect();
                    hrs.sort_by_key(|&h| std::cmp::Reverse(pool.used[h].load(Relaxed)));
                    assert!(order.set((ids, keys.clone(), hrs)).is_ok());
                }
                bar.wait();
                let (order, keys, hrs) = order.get().unwrap();

                // Each hour: count per endpoint, scatter the latencies in name order, select.
                let most = B * pool.used.iter().fold(0, |m, u| m.max(u.load(Relaxed)));
                let lat = unsafe { std::slice::from_raw_parts_mut(anon::<u32>(most * 4), most) };
                let (mut cnt, mut at) = ([0u64; 2048], [0u32; 2048]); // count | errors << 32, next slot
                while let Some(&h) = hrs.get(next[1].fetch_add(1, Relaxed)) {
                    let words = pool.words(h);
                    cnt.fill(0);
                    let mut i = 0;
                    while i < words.len() {
                        let w = words[i];
                        i += 1 + (w >> 12 == ESC) as usize;
                        cnt[(w & 2047) as usize] += 1 + ((w as u64 & 0x800) << 21);
                    }
                    let mut total = 0;
                    for &id in order {
                        at[id] = total;
                        total += cnt[id] as u32;
                    }
                    // Zero words (the unused tails of blocks) are id 0: they land after the groups.
                    at[0] = total;
                    let mut i = 0;
                    while i < words.len() {
                        let (w, big) = (words[i], words[i] >> 12 == ESC);
                        let c = &mut at[(w & 2047) as usize];
                        lat[*c as usize] = if big { words[i + 1] } else { w >> 12 };
                        *c += 1;
                        i += 1 + big as usize;
                    }
                    let nw = words.len();
                    let st = pool.stats(h);
                    let (mut g, mut lo, mut size) = (0, 0, 0);
                    for &id in order.iter().filter(|&&id| cnt[id] as u32 > 0) {
                        let (n, e) = (cnt[id] as u32, (cnt[id] >> 32) as u32);
                        let [p50, p95, p99] = pct(&mut lat[lo..lo + n as usize]);
                        lo += n as usize;
                        st[g] = [id as u32, n, e, p50, p95, p99];
                        let line = [n, p50, p95, p99, e, ppm(e, n)].map(digits);
                        size += 28 + keys[id - 1].1 + line.iter().sum::<usize>();
                        g += 1;
                    }
                    pool.free(h, g * 24, nw * 4);
                    (bytes[h].store(size, Relaxed), groups[h].store(g, Relaxed));
                }
                unsafe { madvise(lat.as_mut_ptr() as *mut u8, most * 4, 4) };

                if bar.wait().is_leader() {
                    // The output size is now known: map the file (/out is a tmpfs on the box; a
                    // filesystem that cannot map it gets one write() at the end).
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
                    let shared = p as isize != -1;
                    let o = Ptr(if shared { p } else { anon(len) });
                    assert!(out.set((o, off, f, shared)).is_ok());
                }
                bar.wait();
                let (o, off, _, shared) = out.get().unwrap();

                // Each hour: format its groups into its range of the output.
                while let Some(&h) = hrs.get(next[2].fetch_add(1, Relaxed)) {
                    let o = unsafe { std::slice::from_raw_parts_mut(o.0.add(off[h]), off[h + 1] - off[h]) };
                    let t = h + 18 + 30 * 24;
                    let (d, hh) = (t / 24, t % 24);
                    let (mo, d) = if d > 31 { (11, d - 31) } else { (10, d) };
                    let stamp = format!("2026-{mo}-{d:02}T{hh:02}:00:00Z ");
                    let mut i = 0;
                    for &[id, n, e, p50, p95, p99] in &pool.stats(h)[..groups[h].load(Relaxed)] {
                        let k = names.text(keys[id as usize - 1]);
                        o[i..i + 21].copy_from_slice(stamp.as_bytes());
                        o[i + 21..i + 21 + k.len()].copy_from_slice(k);
                        i += 21 + k.len();
                        for v in [n, p50, p95, p99, e, ppm(e, n)] {
                            put(o, &mut i, v);
                        }
                        o[i] = b'\n';
                        i += 1;
                    }
                    pool.free(h, 0, groups[h].load(Relaxed) * 24);
                    // Drop this hour's whole output pages from our page tables now, in parallel; the
                    // data stays in the file. Exit would otherwise unmap them all on one core.
                    if *shared {
                        zap(o.as_ptr(), o.len());
                    }
                }
            });
        }
    });
    let (o, off, mut f, shared) = out.into_inner().unwrap();
    if !shared {
        f.write_all(unsafe { std::slice::from_raw_parts(o.0, off[HOURS]) }).expect("write summary.txt");
    }
    // The output is complete; the kernel releases everything else at exit.
    std::process::exit(0);
}
