// Track C, featherweight. One shared name dictionary and one bit per (country, name) replace
// per-thread hash sets; threads parse byte ranges of records.csv that start at a guessed record
// boundary, and the guess is proved afterwards (each range must end exactly where the next begins).
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::sync::Mutex;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
    fn getrusage(who: i32, usage: *mut [i64; 18]) -> i32;
}

/// The scoring box backs anonymous memory with 2 MB pages (THP "always"), and memory.peak charges
/// a whole 2 MB page for its first touched byte. So every allocation of 256 KB or more is marked
/// MADV_NOHUGEPAGE: the bitsets and the arena are sparse, and only their touched 4 KB pages count.
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
        unsafe { madvise(lo as *mut u8, hi - lo, 15) }; // MADV_NOHUGEPAGE
    }
    p
}

/// Read-only private mapping of a whole file followed by at least 4 KB of zeros (an anonymous
/// mapping underneath), so 8- and 16-byte loads may run past the end. The page cache is not
/// charged to us; a heap copy would be.
fn map(path: &str) -> &'static [u8] {
    let f = File::open(path).expect(path);
    let len = f.metadata().unwrap().len() as usize;
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + 8192, 1, 0x22, -1, 0); // PRIVATE | ANONYMOUS
        assert!(len == 0 || mmap(p, len, 1, 0x12, f.as_raw_fd(), 0) == p, "mmap {path}"); // PRIVATE | FIXED
        std::slice::from_raw_parts(p, len)
    }
}

/// n zeroed values in fresh anonymous pages (page-aligned, charged only when touched, never
/// freed: the process ends first). T is an atomic integer.
fn zeroed<T>(n: usize) -> &'static [T] {
    let len = (n * std::mem::size_of::<T>()).max(1);
    let p = unsafe { mmap(std::ptr::null_mut(), len, 3, 0x22, -1, 0) }; // READ | WRITE, PRIVATE | ANONYMOUS
    assert!(p as isize != -1, "mmap");
    unsafe { std::slice::from_raw_parts(advise(p, len) as *const T, n) }
}

/// Byte at i, which may be up to 4 KB past the end of b (the zero padding of `map`).
#[inline(always)]
fn at(b: &[u8], i: usize) -> u8 {
    unsafe { *b.as_ptr().add(i) }
}

#[inline(always)]
fn word(b: &[u8], i: usize) -> u64 {
    unsafe { (b.as_ptr().add(i) as *const u64).read_unaligned() }
}

/// Parse the decimal number at b[i..] and step past the one byte after it (`,` or `.`). Up to 7
/// digits in one go: load 8 bytes, find the first non-digit, shift the digits to the top (the
/// empty low bytes act as leading zeros) and combine pairs, quads and octets with three multiplies.
#[inline(always)]
fn num(b: &[u8], i: &mut usize) -> u64 {
    let d = word(b, *i) ^ 0x3030_3030_3030_3030;
    let stop = d.wrapping_add(0x7676_7676_7676_7676) & 0x8080_8080_8080_8080; // bytes >= 10: not a digit
    let len = stop.trailing_zeros() as usize / 8;
    if len < 8 {
        *i += len + 1;
        let v = if len == 0 { 0 } else { (d << (64 - 8 * len)) & 0x0F0F_0F0F_0F0F_0F0F };
        let v = v.wrapping_mul(2561) >> 8 & 0x00FF_00FF_00FF_00FF;
        let v = v.wrapping_mul(6553601) >> 16 & 0x0000_FFFF_0000_FFFF;
        return v.wrapping_mul(42949672960001) >> 32;
    }
    let mut x = 0u64;
    while at(b, *i).is_ascii_digit() {
        x = x.wrapping_mul(10).wrapping_add((at(b, *i) - b'0') as u64);
        *i += 1;
    }
    *i += 1;
    x
}

/// Bit masks of `"`, `,` and LF in the 64 bytes at b[i..].
#[inline(always)]
fn masks(b: &[u8], i: usize) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    unsafe {
        let p = b.as_ptr().add(i) as *const __m256i;
        let (lo, hi) = (_mm256_loadu_si256(p), _mm256_loadu_si256(p.add(1)));
        let m = |c: u8| {
            let c = _mm256_set1_epi8(c as i8);
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, c)) as u32 as u64 | (_mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, c)) as u64) << 32
        };
        (m(b'"'), m(b','), m(b'\n'))
    }
}

/// Prefix XOR of the quote mask (carry-less multiply by all ones): bit i is set when an odd number
/// of quotes sit at or before i, i.e. byte i is inside a quoted field or is its opening quote.
#[inline(always)]
fn inside(q: u64) -> u64 {
    use std::arch::x86_64::*;
    unsafe { _mm_cvtsi128_si64(_mm_clmulepi64_si128(_mm_set_epi64x(0, q as i64), _mm_set1_epi8(-1), 0)) as u64 }
}

/// Word j of the key b[s..s + n], zero-padded past its end.
#[inline(always)]
fn kw(b: &[u8], s: usize, n: usize, j: usize) -> u64 {
    unsafe { std::arch::x86_64::_bzhi_u64(word(b, s + 8 * j), (8 * n.saturating_sub(8 * j)).min(64) as u32) }
}

#[inline(always)]
fn hash(b: &[u8], s: usize, n: usize) -> u64 {
    let m = (kw(b, s, n, 0) ^ 0x243F_6A88_85A3_08D3 ^ n as u64) as u128 * (kw(b, s, n, 1) ^ 0x1319_8A2E_0370_7344) as u128;
    ((m as u64) ^ (m >> 64) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[inline(always)]
fn prefetch<T>(x: *const T) {
    unsafe { std::arch::x86_64::_mm_prefetch(x as *const i8, std::arch::x86_64::_MM_HINT_T0) };
}


/// Running aggregates of one group, per thread.
#[derive(Clone, Copy)]
struct Agg {
    n: i64,
    ids: i64,
    sum: i64,
    min: i64,
    max: i64,
    name_bytes: i64,
    note_bytes: i64,
}

const NEW: Agg = Agg { n: 0, ids: 0, sum: 0, min: i64::MAX, max: i64::MIN, name_bytes: 0, note_bytes: 0 };

/// Shared mutable array written at disjoint indices by several threads.
struct Slots<T>(*mut T);
unsafe impl<T> Sync for Slots<T> {}

/// Insert-only concurrent name dictionary with the names inline. A slot is 32 bytes: a header
/// `n | pairs << 20 | (id + 1) << 40` (0 = empty, BUSY while being filled; pairs = the `""` in the
/// raw name) and the first 24 bytes of the name, zero-padded. Two slots share a cache line, so a lookup is one prefetched line. A name longer
/// than 24 bytes keeps all its words in `long` and their offset in the slot's second word.
struct Names {
    slots: &'static [AtomicU64],
    shift: u32,
    long: Slots<u64>,
    top: AtomicUsize,
    next_id: AtomicU32,
    max_ids: u32,
    full: AtomicBool,
}

const BUSY: u64 = u64::MAX;
const LONG: usize = 1 << 23; // words reserved for long names; only touched pages are charged

/// One thread's current block of ids.
#[derive(Default)]
struct Local {
    id: u32,
    id_end: u32,
}

impl Names {
    fn new(cap: usize) -> Self {
        Names {
            slots: zeroed(4 * cap),
            shift: 64 - cap.trailing_zeros(),
            long: Slots(Vec::<u64>::with_capacity(LONG).leak().as_mut_ptr()),
            top: AtomicUsize::new(0),
            next_id: AtomicU32::new(0),
            max_ids: (cap / 8 * 7) as u32,
            full: AtomicBool::new(false),
        }
    }

    /// Dense id and `""` count of the raw name b[s..s + n] with hash h (a slice of the mapped file,
    /// so loads may run past its end).
    #[inline(always)]
    fn id(&self, b: &[u8], s: usize, n: usize, h: u64, loc: &mut Local) -> (u32, usize) {
        let (w, nw) = (|j: usize| kw(b, s, n, j), (n + 7) / 8);
        let mut i = (h >> self.shift) as usize & !1; // probing starts at the first slot of a line
        loop {
            let slot = unsafe { self.slots.get_unchecked(4 * i..4 * i + 4) };
            let hdr = slot[0].load(Acquire);
            if hdr == 0 && slot[0].compare_exchange(0, BUSY, Acquire, Relaxed).is_ok() {
                if loc.id == loc.id_end {
                    loc.id = self.next_id.fetch_add(64, Relaxed);
                    loc.id_end = loc.id + 64;
                }
                let o = if n > 24 { self.top.fetch_add(nw, Relaxed) } else { 0 };
                if loc.id >= self.max_ids || o + nw > LONG || n >= 1 << 20 {
                    self.full.store(true, Relaxed);
                    slot[0].store(0, Release);
                    return (0, 0);
                }
                for j in 0..3 {
                    slot[1 + j].store(if n > 24 { o as u64 * (j == 0) as u64 } else { w(j) }, Relaxed);
                }
                for j in 0..nw * (n > 24) as usize {
                    unsafe { *self.long.0.add(o + j) = w(j) };
                }
                let pairs = (s..s + n).filter(|&j| at(b, j) == b'"').count() / 2;
                slot[0].store((n | pairs << 20) as u64 | (loc.id as u64 + 1) << 40, Release);
                loc.id += 1;
                return (loc.id - 1, pairs);
            }
            if hdr == BUSY || hdr == 0 {
                std::hint::spin_loop(); // another thread is filling this slot: look again
                continue;
            }
            let same = || {
                if n > 24 {
                    let o = slot[1].load(Relaxed) as usize;
                    (0..nw).all(|j| unsafe { *self.long.0.add(o + j) } == w(j))
                } else {
                    (slot[1].load(Relaxed) ^ w(0)) | (slot[2].load(Relaxed) ^ w(1)) | (slot[3].load(Relaxed) ^ w(2)) == 0
                }
            };
            if hdr as usize & 0xF_FFFF == n && same() {
                return ((hdr >> 40) as u32 - 1, (hdr >> 20) as usize & 0xF_FFFF);
            }
            i = (i + 1) & (self.slots.len() / 4 - 1);
        }
    }
}

/// One bit per (group, name id), in pages of 4096 names: [page][group][64 words], so a group's
/// bits for one page are contiguous (fast popcount) and pages past the last id are never touched.
/// Threads never use locked instructions on it: a locked OR waits for its line, which another core
/// has usually just written, and blocks every later load meanwhile. Instead each thread queues
/// (word, bit) pairs per region of the array (64 regions) and applies a full queue of 64 with plain
/// loads and stores while holding that region's lock: one lock per 64 bits.
struct Bits {
    w: &'static [AtomicU64],
    locks: Vec<Mutex<()>>,
}

/// One thread's queues: 64 regions x 64 pairs `word << 6 | bit`.
struct Pend {
    q: [[u32; 64]; 64],
    n: [usize; 64],
}

impl Bits {
    #[inline(always)]
    fn set(&self, p: &mut Pend, g: usize, id: u32) {
        let w = ((id >> 12) as usize * 256 + g) * 64 + (id as usize >> 6 & 63);
        let r = (w ^ w >> 6) & 63;
        p.q[r][p.n[r]] = (w as u32) << 6 | id & 63;
        p.n[r] += 1;
        if p.n[r] == 64 {
            self.apply(p, r);
        }
    }

    fn apply(&self, p: &mut Pend, r: usize) {
        let _lock = self.locks[r].lock().unwrap();
        for &x in &p.q[r][..p.n[r]] {
            let a = &self.w[(x >> 6) as usize];
            a.store(a.load(Relaxed) | 1 << (x & 63), Relaxed);
        }
        p.n[r] = 0;
    }
}

/// One parsed record waiting for its lookups.
#[derive(Clone, Copy, Default)]
struct Rec {
    r: usize,
    s: usize,
    n: usize,
    h: u64,
    id: i64,
    v: i64,
    quoted: usize,
    quotes: usize,
    note: usize,
}

const BATCH: usize = 32;

/// Parse the records that start in b[start..end) (start is a record start) into agg, names and
/// bits. Returns where parsing stopped: exactly `end` when `end` is a record boundary.
/// Each 64-byte block gives bit masks of quotes, commas and LFs; the commas and LFs outside quotes
/// are the field ends. Their positions go into a buffer without branches, and every record takes
/// exactly five of them (four commas and the LF), so fields come from positions alone.
fn chunk(b: &[u8], start: usize, end: usize, tab: &[u8], names: &Names, bits: &Bits, loc: &mut Local, pend: &mut Pend, agg: &mut [Agg; 256]) -> usize {
    let (mut base, mut carry, mut quotes, mut rs) = (start, 0u64, 0usize, start);
    // Ring of structural positions, n written, i consumed; quotes before the record start.
    let (mut pos, mut n, mut i, mut qrs) = ([0usize; 128], 0usize, 0usize, 0usize);
    let (mut batch, mut nb) = ([Rec::default(); BATCH], 0);
    'blocks: while rs < end && base < b.len() {
        let (q, c, l) = masks(b, base);
        let x = inside(q) ^ carry;
        carry = ((x as i64) >> 63) as u64;
        let mut s = (c | l) & !x;
        let k = n + s.count_ones() as usize;
        while n < k {
            for _ in 0..8 {
                pos[n & 127] = base + s.trailing_zeros() as usize;
                s &= s.wrapping_sub(1);
                n += 1;
            }
        }
        n = k;
        while i + 5 <= n {
            // id , name , note , region , amount (CR) LF; a BOM may precede the id. A record's
            // fifth position is always in this block, so its quotes are counted up to there; the
            // name's are known to the dictionary, and the rest are the note's.
            let at5 = |j: usize| pos[(i + j) & 127];
            let (c0, c1, c2, c3, lf) = (at5(0), at5(1), at5(2), at5(3), at5(4));
            let qlf = quotes + unsafe { std::arch::x86_64::_bzhi_u64(q, (lf - base) as u32) }.count_ones() as usize;
            let r = &mut batch[nb & (BATCH - 1)];
            (r.quotes, qrs) = (qlf.wrapping_sub(qrs), qlf);
            r.quoted = (at(b, c0 + 1) == b'"') as usize;
            r.note = c2 - c1 - 1;
            r.id = num(b, &mut (rs + 3 * (at(b, rs) == 0xEF) as usize)) as i64;
            r.s = c0 + 1 + r.quoted;
            r.n = (c1 - r.quoted).saturating_sub(r.s);
            r.h = hash(b, r.s, r.n);
            let line = names.slots.as_ptr().wrapping_add(4 * ((r.h >> names.shift) as usize & !1));
            prefetch(line);
            prefetch(line.wrapping_add(8)); // the next line: probes often run past the first two slots
            r.r = num(b, &mut (c2 + 1)) as usize;
            prefetch(tab.as_ptr().wrapping_add(r.r)); // a prefetch never faults
            let mut p = c3 + 1;
            let neg = at(b, p) == b'-';
            p += neg as usize;
            let v = num(b, &mut p) as i64 * 100 + (at(b, p) as i64 - 48) * 10 + at(b, p + 1) as i64 - 48;
            r.v = if neg { -v } else { v };
            nb += 1;
            if nb == BATCH {
                flush(b, &batch, tab, names, bits, loc, pend, agg);
                nb = 0;
            }
            rs = lf + 1;
            i += 5;
            if rs >= end {
                break 'blocks;
            }
        }
        (quotes, base) = (quotes + q.count_ones() as usize, base + 64);
    }
    flush(b, &batch[..nb], tab, names, bits, loc, pend, agg);
    rs
}

/// Lookups for a batch: the region byte and the dictionary slot were prefetched while the batch was
/// parsed; pass one adds each record to its group, pass two finds the name's id and queues its bit.
#[inline(always)]
fn flush(b: &[u8], batch: &[Rec], tab: &[u8], names: &Names, bits: &Bits, loc: &mut Local, pend: &mut Pend, agg: &mut [Agg; 256]) {
    let mut gs = [0usize; BATCH];
    for (r, g) in batch.iter().zip(gs.iter_mut()) {
        *g = tab.get(r.r).copied().unwrap_or(0) as usize;
        let a = &mut agg[*g];
        a.n += 1;
        a.ids += r.id;
        a.sum += r.v;
        a.min = a.min.min(r.v);
        a.max = a.max.max(r.v);
    }
    for (r, &g) in batch.iter().zip(gs.iter()) {
        let (id, pairs) = names.id(b, r.s, r.n, r.h, loc);
        let tq = r.quotes.wrapping_sub(r.quoted * (2 + 2 * pairs)); // the note's quotes
        let a = &mut agg[g];
        a.name_bytes += (r.n - pairs) as i64;
        a.note_bytes += r.note.wrapping_sub(tq / 2 + (tq > 0) as usize) as i64;
        bits.set(pend, g, id);
    }
}

/// First record start at or after i, by the shape of the data: a LF followed by a digit or a byte
/// order mark. Inside quotes a LF is always followed by a letter or a quote (spec 2.2); `run`
/// still checks every guess.
fn guess(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && !(at(b, i - 1) == b'\n' && (at(b, i).is_ascii_digit() || at(b, i) == 0xEF)) {
        i += 1;
    }
    i.min(b.len())
}

/// Aggregate all records with `threads` threads over `cap` dictionary slots. None if a chunk guess
/// was wrong or the dictionary filled up; the caller then retries more conservatively.
fn run(b: &[u8], tab: &[u8], cap: usize, threads: usize) -> Option<(Vec<Agg>, Vec<u64>)> {
    let first = b.iter().position(|&c| c == b'\n').map_or(b.len(), |i| i + 1); // after the header
    let k = if threads == 1 { 1 } else { threads * 8 };
    let mut cut: Vec<usize> = (0..k).map(|j| guess(b, first + (b.len() - first) * j / k)).collect();
    cut[0] = first;
    cut.push(b.len());
    let names = Names::new(cap);
    let bits = Bits { w: zeroed((names.max_ids as usize / 4096 + 1) * 256 * 64), locks: (0..64).map(|_| Mutex::new(())).collect() };
    assert!(bits.w.len() < 1 << 26);
    let (next, ends) = (AtomicUsize::new(0), Mutex::new(vec![0; k]));
    let aggs: Vec<[Agg; 256]> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let (mut agg, mut loc, mut pend) = ([NEW; 256], Local::default(), Pend { q: [[0; 64]; 64], n: [0; 64] });
                    loop {
                        let j = next.fetch_add(1, Relaxed);
                        if j >= k {
                            (0..64).for_each(|r| bits.apply(&mut pend, r));
                            break agg;
                        }
                        let e = chunk(b, cut[j], cut[j + 1], tab, &names, &bits, &mut loc, &mut pend, &mut agg);
                        ends.lock().unwrap()[j] = e;
                    }
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let ends = ends.into_inner().unwrap();
    eprintln!("names {} long words {}", names.next_id.load(Relaxed), names.top.load(Relaxed));
    if names.full.load(Relaxed) || (0..k - 1).any(|j| ends[j] != cut[j + 1]) {
        return None;
    }
    let mut total = vec![NEW; 256];
    for a in &aggs {
        for (t, x) in total.iter_mut().zip(a) {
            *t = Agg { n: t.n + x.n, ids: t.ids + x.ids, sum: t.sum + x.sum, min: t.min.min(x.min), max: t.max.max(x.max), name_bytes: t.name_bytes + x.name_bytes, note_bytes: t.note_bytes + x.note_bytes };
        }
    }
    let distinct = (0..256)
        .map(|g| bits.w.chunks(256 * 64).map(|page| page[g * 64..g * 64 + 64].iter().map(|w| w.load(Relaxed).count_ones() as u64).sum::<u64>()).sum())
        .collect();
    Some((total, distinct))
}

fn money(c: i64) -> String {
    format!("{}{}.{:02}", if c < 0 { "-" } else { "" }, c.abs() / 100, c.abs() % 100)
}

fn main() {
    let t0 = std::time::Instant::now();
    std::env::set_var("RUST_MIN_STACK", "262144");
    unsafe { mallopt(-8, 1) }; // M_ARENA_MAX = 1
    let threads = std::env::var("FW_THREADS").ok().and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |p| p.get()));

    // regions.csv: "id,CCC\n" lines in ascending id order, after a header. Group 0 is _UNMATCHED,
    // countries are numbered 1.. in order of first sight.
    let rg = map("/data/regions.csv");
    let body = &rg[rg.iter().position(|&c| c == b'\n').unwrap() + 1..];
    let last = body[..body.len() - 1].iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
    let mut tab = vec![0u8; num(body, &mut { last }) as usize + 1];
    let codes = Mutex::new(vec![*b"_UNMATCHED"]);
    let seen: &[AtomicU8] = zeroed(26 * 26 * 26);
    let (out, next, len) = (Slots(tab.as_mut_ptr()), AtomicUsize::new(0), tab.len());
    let parts: Vec<usize> = (0..=threads * 4).map(|j| guess_line(body, body.len() * j / (threads * 4))).collect();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let j = next.fetch_add(1, Relaxed);
                if j + 1 >= parts.len() {
                    break;
                }
                let (mut p, out) = (parts[j], &out);
                while p < parts[j + 1] {
                    let r = num(body, &mut p) as usize;
                    let c = body[p..p + 3].iter().fold(0, |a, &x| a * 26 + (x - b'A') as usize);
                    let mut g = seen[c].load(Relaxed);
                    if g == 0 {
                        let mut list = codes.lock().unwrap();
                        g = seen[c].load(Relaxed);
                        if g == 0 {
                            let mut code = *b"__________";
                            code[..3].copy_from_slice(&body[p..p + 3]);
                            list.push(code);
                            assert!(list.len() < 256, "more than 255 countries");
                            g = list.len() as u8 - 1;
                            seen[c].store(g, Relaxed);
                        }
                    }
                    assert!(r < len, "regions.csv ids not ascending");
                    unsafe { *out.0.add(r) = g };
                    p += 4;
                }
            });
        }
    });
    let codes = codes.into_inner().unwrap();
    let t1 = t0.elapsed();

    let rc = map("/data/records.csv");
    let (mut cap, mut t) = (1 << 17, threads);
    let (total, distinct) = loop {
        if let Some(r) = run(rc, &tab, cap, t) {
            break r;
        }
        (cap, t) = (cap * 4, 1);
    };
    let t2 = t0.elapsed();

    let mut order: Vec<usize> = (0..codes.len()).filter(|&g| total[g].n > 0).collect();
    order.sort_by_key(|&g| codes[g]);
    let mut text = String::new();
    for g in order {
        let a = &total[g];
        let key = std::str::from_utf8(&codes[g]).unwrap().trim_end_matches(|c| g > 0 && c == '_');
        text += &format!("{key} {} {} {} {} {} {} {} {}\n", a.n, a.ids, money(a.sum), money(a.min), money(a.max), distinct[g], a.name_bytes, a.note_bytes);
    }
    std::fs::write("/out/aggregates.txt", text).expect("write aggregates.txt");
    let mut ru = [0i64; 18];
    unsafe { getrusage(0, &mut ru) };
    eprintln!("cpu {:.1}ms sys {:.1}ms regions {t1:?} records {t2:?} total {:?}", (ru[0] * 1000000 + ru[1] + ru[2] * 1000000 + ru[3]) as f64 / 1e3, (ru[2] * 1000000 + ru[3]) as f64 / 1e3, t0.elapsed());
}

/// First line start at or after i.
fn guess_line(b: &[u8], mut i: usize) -> usize {
    while i > 0 && i < b.len() && b[i - 1] != b'\n' {
        i += 1;
    }
    i
}
