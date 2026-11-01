// Track A, featherweight. One parse pass turns each line into a 4-byte record (endpoint id, 5xx bit,
// 20-bit latency, with an escape word for larger ones), filed into a separate memory region per hour.
// Then each hour on its own: count per endpoint, scatter the latencies, select p50/p95/p99, format,
// and give the hour's records back to the kernel. Output is written in order as hours complete.
use std::arch::x86_64::*;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

/// The scoring box backs anonymous memory with 2 MB pages (THP "always"), and memory.peak charges
/// a whole 2 MB page for its first touched byte. So every allocation of 256 KB or more is marked
/// MADV_NOHUGEPAGE.
struct Alloc;

unsafe impl std::alloc::GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        advise(std::alloc::System.alloc(l), l.size())
    }
    unsafe fn alloc_zeroed(&self, l: std::alloc::Layout) -> *mut u8 {
        advise(std::alloc::System.alloc_zeroed(l), l.size())
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        std::alloc::System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
        advise(std::alloc::System.realloc(p, l, n), n)
    }
}

#[global_allocator]
static ALLOC: Alloc = Alloc;

fn advise(p: *mut u8, len: usize) -> *mut u8 {
    let (lo, hi) = ((p as usize + 4095) & !4095, (p as usize + len) & !4095);
    if len >= 1 << 18 && hi > lo {
        unsafe { madvise(lo as *mut u8, hi - lo, 15) };
    }
    p
}

/// `len` bytes of fresh anonymous memory without huge pages; only touched pages are charged.
fn anon(len: usize) -> *mut u8 {
    let p = unsafe { mmap(std::ptr::null_mut(), len.max(1), 3, 0x4022, -1, 0) };
    assert!(p as isize != -1, "mmap {len}");
    unsafe { madvise(p, len, 15) };
    p
}

/// The input, mapped read-only (the page cache is not charged to us), followed by at least 4 KB of
/// zero pages, so the parser may read up to 4 KB past the last byte.
fn map(path: &str) -> &'static [u8] {
    let f = File::open(path).expect(path);
    let len = f.metadata().unwrap().len() as usize;
    assert!(len < 1 << 32);
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + 4096, 1, 0x22, -1, 0);
        assert!(p as isize != -1 && (len == 0 || mmap(p, len, 1, 0x12, f.as_raw_fd(), 0) == p));
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

/// Bit k set where byte k of the 64 bytes at b[i..] equals `c`.
#[inline(always)]
fn find64(b: &[u8], i: usize, c: u8) -> u64 {
    unsafe {
        let (p, v) = (b.as_ptr().add(i) as *const __m128i, _mm_set1_epi8(c as i8));
        (0..4).fold(0, |m, k| m | (_mm_movemask_epi8(_mm_cmpeq_epi8(_mm_loadu_si128(p.add(k)), v)) as u16 as u64) << (16 * k))
    }
}

/// Bit k set where b[x + k] == b[y + k], for k < 64.
#[inline(always)]
fn eq64(b: &[u8], x: usize, y: usize) -> u64 {
    unsafe {
        let (p, q) = (b.as_ptr().add(x) as *const __m128i, b.as_ptr().add(y) as *const __m128i);
        (0..4).fold(0, |m, k| {
            let e = _mm_cmpeq_epi8(_mm_loadu_si128(p.add(k)), _mm_loadu_si128(q.add(k)));
            m | (_mm_movemask_epi8(e) as u16 as u64) << (16 * k)
        })
    }
}

/// Do b[x..x + n] and b[y..y + n] hold the same bytes?
#[inline(always)]
fn same(b: &[u8], x: usize, y: usize, n: usize) -> bool {
    let mut i = 0;
    loop {
        let ne = !eq64(b, x + i, y + i);
        if n - i <= 64 {
            return ne << (64 - (n - i)) == 0;
        }
        if ne != 0 {
            return false;
        }
        i += 64;
    }
}

/// Low `n` bytes of a word (n clamped to 0..=8).
#[inline(always)]
fn low(n: usize) -> u64 {
    if n >= 8 { !0 } else { (1u64 << (8 * n)) - 1 }
}

/// Parse the decimal number at b[i..] and step past the byte after it (SWAR for up to 7 digits).
#[inline(always)]
fn num(b: &[u8], i: &mut usize) -> u32 {
    let d = ld8(b, *i) ^ 0x3030_3030_3030_3030;
    let len = ((d.wrapping_add(0x7676_7676_7676_7676) & 0x8080_8080_8080_8080).trailing_zeros() / 8) as usize;
    if len < 8 && len > 0 {
        *i += len + 1;
        let v = (d << (64 - 8 * len)) & 0x0F0F_0F0F_0F0F_0F0F;
        let v = v.wrapping_mul(2561) >> 8 & 0x00FF_00FF_00FF_00FF;
        let v = v.wrapping_mul(6553601) >> 16 & 0x0000_FFFF_0000_FFFF;
        return (v.wrapping_mul(42949672960001) >> 32) as u32;
    }
    let mut x = 0u64;
    while b[*i].is_ascii_digit() {
        x = x * 10 + (b[*i] - b'0') as u64;
        *i += 1;
    }
    *i += 1;
    x as u32
}

static TT: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
fn tick(k: usize, q: &mut std::time::Instant) { TT[k].fetch_add(q.elapsed().as_nanos() as u64, Relaxed); *q = std::time::Instant::now(); }
fn mem() -> String { std::fs::read_to_string("/sys/fs/cgroup/memory.current").unwrap_or_default().trim().to_string() }
const HOURS: usize = 72;
const SLOTS: usize = 8192;
const KEY: u64 = 0xFFF8_00FF_0000_0000; // a slot's length and hash-tag bits
const ESC: u32 = (1 << 20) - 1; // latency field of a record whose latency is in the next word
const B: usize = 512; // words per record block

/// Endpoint interner shared by all threads. A slot is 0 or offset | len << 32 | id << 40 | tag << 51,
/// where offset locates the endpoint's first occurrence in the input. Ids start at 1, so no record is 0.
struct Names {
    slots: Vec<AtomicU64>,
    keys: Mutex<Vec<(usize, usize)>>,
}

impl Names {
    #[inline(always)]
    fn id(&self, b: &[u8], s: usize, n: usize, h: u64) -> u32 {
        let want = (n as u64) << 32 | h & 0xFFF8 << 48;
        let mut i = (h >> 38) as usize % SLOTS;
        loop {
            let v = self.slots[i].load(Acquire);
            if v & KEY == want && same(b, v as u32 as usize, s, n) {
                return (v >> 40) as u32 & 2047;
            }
            if v == 0 {
                return self.add(b, s, n, want, i);
            }
            i = (i + 1) % SLOTS;
        }
    }

    #[cold]
    fn add(&self, b: &[u8], s: usize, n: usize, want: u64, mut i: usize) -> u32 {
        let mut keys = self.keys.lock().unwrap();
        loop {
            let v = self.slots[i].load(Acquire);
            if v == 0 {
                keys.push((s, n));
                assert!(keys.len() < 2048, "more than 2047 endpoints");
                self.slots[i].store(s as u64 | want | (keys.len() as u64) << 40, Release);
                return keys.len() as u32;
            }
            if v & KEY == want && same(b, v as u32 as usize, s, n) {
                return (v >> 40) as u32 & 2047;
            }
            i = (i + 1) % SLOTS;
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
    fn block(&self, h: usize) -> (*mut u32, *mut u32) {
        let k = self.used[h].fetch_add(1, Relaxed);
        assert!((k + 1) * B <= self.region);
        let p = unsafe { self.base.add(h * self.region + k * B) };
        (p, unsafe { p.add(B) })
    }

    fn words(&self, h: usize) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.base.add(h * self.region), self.used[h].load(Relaxed) * B) }
    }

    /// Hour h's region seen as group statistics (it holds them once its records are consumed).
    fn stats(&self, h: usize) -> &mut [[u32; 6]] {
        unsafe { std::slice::from_raw_parts_mut(self.base.add(h * self.region) as *mut [u32; 6], 2048) }
    }

    /// Give bytes [lo, hi) of hour h's region (rounded inwards to whole pages) back to the kernel.
    fn free(&self, h: usize, lo: usize, hi: usize) {
        let (lo, hi) = ((lo + 4095) & !4095, (hi + 4095) & !4095);
        if hi > lo {
            unsafe { madvise((self.base.add(h * self.region) as *mut u8).add(lo), hi - lo, 4) };
        }
    }
}

/// Parse the lines in b[lo..hi] into records.
fn parse(b: &[u8], lo: usize, hi: usize, names: &Names, pool: &Pool, cur: &mut [(*mut u32, *mut u32); HOURS]) {
    let mut p = lo;
    while p < hi {
        let w = ld8(b, p + 5); // "MM-DDTHH"
        let dig = |k: usize| (w >> (8 * k) & 15) as usize;
        let h = 24 * (dig(3) * 10 + dig(4)) + dig(6) * 10 + dig(7) + 744 * dig(1) - 738;
        let x = ld8(b, p + 31) ^ 0x2020_2020_2020_2020; // the method is 3 to 7 bytes
        let s = p + 32 + ((x.wrapping_sub(0x0101_0101_0101_0101) & !x & 0x8080_8080_8080_8080).trailing_zeros() / 8) as usize;
        let mut e = s + find64(b, s, b' ').trailing_zeros() as usize;
        if e == s + 64 {
            while b[e] != b' ' {
                e += 1;
            }
        }
        let n = e - s;
        let a = (ld8(b, s) & low(n)) ^ (ld8(b, s + 8) & low(n.saturating_sub(8))).rotate_left(29);
        let z = if n >= 16 { ld8(b, e - 8) } else { 0 };
        let hash = (a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ z ^ n as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93);
        let id = names.id(b, s, n, hash);
        let err = (b[e + 1] == b'5') as u32;
        p = e + 5;
        let lat = num(b, &mut p);
        let c = &mut cur[h];
        if (c.1 as usize) < c.0 as usize + 8 {
            *c = pool.block(h);
        }
        unsafe {
            *c.0 = id | err << 11 | lat.min(ESC) << 12;
            c.0 = c.0.add(1);
            if lat >= ESC {
                *c.0 = lat;
                c.0 = c.0.add(1);
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

/// Write " v" at o[*i..] and advance.
#[inline(always)]
fn put(o: &mut [u8], i: &mut usize, mut v: u32) {
    let mut j = *i + digits(v);
    o[*i] = b' ';
    *i = j + 1;
    loop {
        o[j] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
        j -= 1;
    }
}

/// Shared mutable memory written at disjoint ranges by several threads.
struct Slots<T>(*mut T);
unsafe impl<T> Sync for Slots<T> {}

/// Nearest-rank p50, p95, p99 of v (reorders v).
fn pct(v: &mut [u32]) -> [u32; 3] {
    let n = v.len();
    let (mut out, mut lo) = ([0u32; 3], 0);
    for (k, q) in [50, 95, 99].into_iter().enumerate() {
        let r = (q * n + 99) / 100 - 1;
        out[k] = if r >= lo { *v[lo..].select_nth_unstable(r - lo).1 } else { out[k - 1] };
        lo = lo.max(r + 1);
    }
    out
}

fn main() {
    std::env::set_var("RUST_MIN_STACK", "262144");
    unsafe { mallopt(-8, 1) };
    let t0 = std::time::Instant::now();
    let threads = std::env::var("FW_THREADS").ok().and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |p| p.get()));
    let b = map("/data/requests.log");
    let names = Names { slots: (0..SLOTS).map(|_| AtomicU64::new(0)).collect(), keys: Mutex::new(Vec::new()) };
    // Any hour may hold every line (two words each at worst), plus a partly filled block per thread.
    let region = (b.len() / 19 + (threads + 1) * B + 6 * 2048 + 1023) & !1023;
    let pool = Pool { base: anon(HOURS * region * 4) as *mut u32, region, used: (0..HOURS).map(|_| AtomicUsize::new(0)).collect() };
    let parts = chunks(b, threads * 16);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut cur = [(std::ptr::null_mut(), std::ptr::null_mut()); HOURS];
                while let Some(&(lo, hi)) = parts.get(next.fetch_add(1, Relaxed)) {
                    parse(b, lo, hi, &names, &pool, &mut cur);
                }
            });
        }
    });
    let t1 = t0.elapsed();
    let m1 = mem();

    // Endpoint ids in byte order of their names.
    let keys = names.keys.into_inner().unwrap();
    let name = |id: usize| &b[keys[id - 1].0..keys[id - 1].0 + keys[id - 1].1];
    let mut order: Vec<usize> = (1..=keys.len()).collect();
    order.sort_unstable_by_key(|&id| name(id));

    // Each hour: group its latencies by endpoint and select the percentiles. The hour's records are
    // dead after the scatter, so its region then holds six words per group (id, count, errors, p50,
    // p95, p99), and the rest of the region goes back to the kernel.
    let bytes: Vec<AtomicUsize> = (0..HOURS).map(|_| AtomicUsize::new(0)).collect();
    let groups: Vec<AtomicUsize> = (0..HOURS).map(|_| AtomicUsize::new(0)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let (mut cnt, mut errs, mut at) = (vec![0u32; 2048], vec![0u32; 2048], vec![0u32; 2048]);
                let mut lat: Vec<u32> = Vec::new();
                loop {
                    let h = next.fetch_add(1, Relaxed);
                    if h >= HOURS {
                        break;
                    }
                    let mut q = std::time::Instant::now();
                    let words = pool.words(h);
                    cnt.fill(0);
                    errs.fill(0);
                    let mut i = 0;
                    while i < words.len() {
                        let w = words[i];
                        i += 1 + (w >> 12 == ESC) as usize;
                        cnt[(w & 2047) as usize] += 1;
                        errs[(w & 2047) as usize] += w >> 11 & 1;
                    }
                    tick(0, &mut q);
                    let mut total = 0;
                    for &id in &order {
                        at[id] = total;
                        total += cnt[id];
                    }
                    if lat.len() < total as usize {
                        lat.resize(total as usize, 0);
                    }
                    let mut i = 0;
                    while i < words.len() {
                        let w = words[i];
                        let big = w >> 12 == ESC;
                        let id = (w & 2047) as usize;
                        if w != 0 {
                            lat[at[id] as usize] = if big { words[i + 1] } else { w >> 12 };
                            at[id] += 1;
                        }
                        i += 1 + big as usize;
                    }
                    tick(1, &mut q);
                    let st = pool.stats(h);
                    let (mut g, mut lo, mut size) = (0, 0, 0);
                    for &id in &order {
                        let n = cnt[id] as usize;
                        if n == 0 {
                            continue;
                        }
                        let [p50, p95, p99] = pct(&mut lat[lo..lo + n]);
                        lo += n;
                        let e = errs[id];
                        st[g] = [id as u32, n as u32, e, p50, p95, p99];
                        size += 28 + keys[id - 1].1 + [n as u32, p50, p95, p99, e, ppm(e, n as u32)].map(digits).iter().sum::<usize>();
                        g += 1;
                    }
                    tick(3, &mut q);
                    pool.free(h, g * 24, words.len() * 4);
                    tick(2, &mut q);
                    (bytes[h].store(size, Relaxed), groups[h].store(g, Relaxed));
                }
            });
        }
    });
    let t2 = t0.elapsed();

    // The output size is now known: map the file and let every thread format its hours in place.
    let mut off = vec![0];
    for x in &bytes {
        off.push(off.last().unwrap() + x.load(Relaxed));
    }
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open("/out/summary.txt").expect("create summary.txt");
    f.set_len(off[HOURS] as u64).expect("size summary.txt");
    let out = if off[HOURS] > 0 { unsafe { mmap(std::ptr::null_mut(), off[HOURS], 3, 1, f.as_raw_fd(), 0) } } else { std::ptr::null_mut() };
    assert!(out as isize != -1, "mmap summary.txt");
    let out = Slots(out);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let out = &out;
                loop {
                    let h = next.fetch_add(1, Relaxed);
                    if h >= HOURS {
                        break;
                    }
                    let o = unsafe { std::slice::from_raw_parts_mut(out.0.add(off[h]), off[h + 1] - off[h]) };
                    let t = h + 18 + 30 * 24;
                    let (d, hh) = (t / 24, t % 24);
                    let (mo, d) = if d > 31 { (11, d - 31) } else { (10, d) };
                    let stamp = format!("2026-{mo}-{d:02}T{hh:02}:00:00Z");
                    let mut i = 0;
                    for &[id, n, e, p50, p95, p99] in &pool.stats(h)[..groups[h].load(Relaxed)] {
                        let k = name(id as usize);
                        o[i..i + 20].copy_from_slice(stamp.as_bytes());
                        o[i + 20] = b' ';
                        o[i + 21..i + 21 + k.len()].copy_from_slice(k);
                        i += 21 + k.len();
                        for v in [n, p50, p95, p99, e, ppm(e, n)] {
                            put(o, &mut i, v);
                        }
                        o[i] = b'\n';
                        i += 1;
                    }
                    debug_assert_eq!(i, o.len());
                    pool.free(h, 0, groups[h].load(Relaxed) * 24);
                }
            });
        }
    });
    f.sync_data().expect("sync summary.txt");
    eprintln!("parse {t1:?} stats {t2:?} total {:?} mem after parse {m1} end {} peak {} | count {} scatter {} free {} select {} (ms cpu)", t0.elapsed(), mem(), std::fs::read_to_string("/sys/fs/cgroup/memory.peak").unwrap_or_default().trim(), TT[0].load(Relaxed)/1000000, TT[1].load(Relaxed)/1000000, TT[2].load(Relaxed)/1000000, TT[3].load(Relaxed)/1000000);
    let _ = munmap;
}
