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

/// n zeroed values from calloc: fresh pages, charged only when touched. T is an atomic integer.
fn zeroed<T>(n: usize) -> Vec<T> {
    unsafe { Vec::from_raw_parts(std::alloc::alloc_zeroed(std::alloc::Layout::array::<T>(n).unwrap()) as *mut T, n, n) }
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
    word(b, s + 8 * j) & ((1u128 << (8 * n.saturating_sub(8 * j).min(8))) - 1) as u64
}

#[inline(always)]
fn hash(b: &[u8], s: usize, n: usize) -> u64 {
    let m = (kw(b, s, n, 0) ^ 0x243F_6A88_85A3_08D3 ^ n as u64) as u128 * (kw(b, s, n, 1) ^ 0x1319_8A2E_0370_7344) as u128;
    ((m as u64) ^ (m >> 64) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[inline(always)]
fn prefetch<T>(x: &T) {
    unsafe { std::arch::x86_64::_mm_prefetch(x as *const T as *const i8, std::arch::x86_64::_MM_HINT_T0) };
}

static EXP: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| std::env::var("FW_EXP").ok().and_then(|v| v.parse().ok()).unwrap_or(0));

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

/// Insert-only concurrent name dictionary. A table slot holds an 8-bit hash tag and the arena word
/// offset + 1 of the entry; an entry is one word `id | len << 32` and the name bytes, zero-padded
/// to whole words. Threads take arena space and ids in blocks, so the shared counters stay cold.
struct Names {
    table: Vec<AtomicU32>,
    shift: u32,
    _arena: Vec<u64>,
    words: Slots<u64>,
    top: AtomicUsize,
    next_id: AtomicU32,
    max_ids: u32,
    full: AtomicBool,
}

/// Arena words: a table slot has 24 bits for the offset.
const ARENA: usize = (1 << 24) - 1;

/// One thread's current arena block and id block.
#[derive(Default)]
struct Local {
    at: usize,
    end: usize,
    id: u32,
    id_end: u32,
}

impl Names {
    fn new(cap: usize) -> Self {
        let mut arena = Vec::with_capacity(ARENA); // only touched pages are charged
        let words = Slots(arena.as_mut_ptr());
        Names {
            table: zeroed(cap),
            shift: 64 - cap.trailing_zeros(),
            _arena: arena,
            words,
            top: AtomicUsize::new(0),
            next_id: AtomicU32::new(0),
            max_ids: (cap / 4 * 3) as u32,
            full: AtomicBool::new(false),
        }
    }

    /// Dense id of the name b[s..s + n] with hash h (a slice of the mapped file, so loads may run
    /// past its end).
    #[inline(always)]
    fn id(&self, b: &[u8], s: usize, n: usize, h: u64, loc: &mut Local) -> u32 {
        let w = |j: usize| kw(b, s, n, j);
        let (tag, nw) = ((h >> 8) as u32 & 0xFF00_0000, (n + 7) / 8);
        let mut i = (h >> self.shift) as usize;
        loop {
            let mut e = self.table[i].load(Acquire);
            if e == 0 {
                if loc.at + 1 + nw > loc.end {
                    let size = (1 + nw).max(512);
                    loc.at = self.top.fetch_add(size, Relaxed);
                    loc.end = loc.at + size;
                }
                if loc.id == loc.id_end {
                    loc.id = self.next_id.fetch_add(64, Relaxed);
                    loc.id_end = loc.id + 64;
                }
                if loc.end >= ARENA || loc.id >= self.max_ids {
                    self.full.store(true, Relaxed);
                    return 0;
                }
                unsafe {
                    *self.words.0.add(loc.at) = loc.id as u64 | (n as u64) << 32;
                    for j in 0..nw {
                        *self.words.0.add(loc.at + 1 + j) = w(j);
                    }
                }
                match self.table[i].compare_exchange(0, tag | (loc.at as u32 + 1), Release, Acquire) {
                    Ok(_) => {
                        loc.at += 1 + nw;
                        loc.id += 1;
                        return loc.id - 1;
                    }
                    Err(cur) => e = cur,
                }
            }
            if e & 0xFF00_0000 == tag {
                let o = (e & 0xFF_FFFF) as usize - 1;
                let hdr = unsafe { *self.words.0.add(o) };
                if hdr >> 32 == n as u64 && (0..nw).all(|j| unsafe { *self.words.0.add(o + 1 + j) } == w(j)) {
                    return hdr as u32;
                }
            }
            i = (i + 1) & (self.table.len() - 1);
        }
    }
}

/// Bit (group g, name id) lives in pages of 4096 names: [page][group][64 words], so a group's bits
/// for one page are contiguous (fast popcount) and pages past the last id are never touched.
#[inline(always)]
fn bit(g: usize, id: u32) -> (usize, u64) {
    (((id >> 12) as usize * 256 + g) * 64 + (id as usize >> 6 & 63), 1 << (id & 63))
}

/// One parsed record waiting for its name lookup: group, name key span and its hash.
#[derive(Clone, Copy, Default)]
struct Rec {
    g: usize,
    s: usize,
    n: usize,
    h: u64,
}

/// Parse the records that start in b[start..end) (start is a record start) into agg, names and
/// bits. Returns where parsing stopped: exactly `end` when `end` is a record boundary.
/// Each 64-byte block gives bit masks of quotes, commas and LFs; the commas and LFs outside quotes
/// are the field ends, so a record's fields come from bit positions and no byte is scanned twice.
fn chunk(b: &[u8], start: usize, end: usize, tab: &[u8], names: &Names, bits: &[AtomicU64], loc: &mut Local, agg: &mut [Agg; 256]) -> usize {
    let (mut base, mut carry, mut quotes) = (start, 0u64, 0usize);
    let (mut rs, mut f, mut cs, mut qs) = (start, 0usize, [0usize; 5], [0usize; 3]);
    let (mut batch, mut nb) = ([Rec::default(); 16], 0);
    while rs < end && base < b.len() {
        let (q, c, l) = masks(b, base);
        let x = inside(q) ^ carry;
        carry = ((x as i64) >> 63) as u64;
        let mut s = (c | l) & !x;
        while s != 0 {
            let t = s.trailing_zeros() as usize;
            s &= s - 1;
            if f < 3 {
                qs[f] = quotes + (q & ((1u64 << t) - 1)).count_ones() as usize;
            }
            if l >> t & 1 == 0 {
                cs[f] = base + t;
                f = (f + 1).min(4);
                continue;
            }
            if f == 4 {
                // id , name , note , region , amount (CR) LF; a BOM may precede the id.
                let mut p = rs + 3 * (at(b, rs) == 0xEF) as usize;
                let id = num(b, &mut p) as i64;
                let (nq, tq) = (qs[1] - qs[0], qs[2] - qs[1]);
                let r = &mut batch[nb];
                r.g = tab.get(num(b, &mut { cs[2] + 1 }) as usize).copied().unwrap_or(0) as usize;
                r.s = cs[0] + 1 + (nq > 0) as usize;
                r.n = (cs[1] - (nq > 0) as usize).saturating_sub(r.s);
                r.h = hash(b, r.s, r.n);
                prefetch(&names.table[(r.h >> names.shift) as usize]);
                let mut p = cs[3] + 1;
                let neg = at(b, p) == b'-';
                p += neg as usize;
                let v = num(b, &mut p) as i64 * 100 + (at(b, p) as i64 - 48) * 10 + at(b, p + 1) as i64 - 48;
                let v = if neg { -v } else { v };
                let a = &mut agg[r.g];
                a.n += 1;
                a.ids += id;
                a.sum += v;
                a.min = a.min.min(v);
                a.max = a.max.max(v);
                a.name_bytes += (cs[1] - cs[0] - 1 - nq / 2 - (nq > 0) as usize) as i64;
                a.note_bytes += (cs[2] - cs[1] - 1 - tq / 2 - (tq > 0) as usize) as i64;
                nb += 1;
                if nb == batch.len() {
                    flush(b, &batch, names, bits, loc);
                    nb = 0;
                }
            }
            (rs, f) = (base + t + 1, 0);
            if rs >= end {
                break;
            }
        }
        quotes += q.count_ones() as usize;
        base += 64;
    }
    flush(b, &batch[..nb], names, bits, loc);
    rs
}

/// Name lookups for a batch, then the bit sets: the table lines were prefetched while the batch was
/// parsed, the arena loads of different records overlap, and each locked OR (which waits for all
/// earlier memory operations) finds its line already fetched.
#[inline(always)]
fn flush(b: &[u8], batch: &[Rec], names: &Names, bits: &[AtomicU64], loc: &mut Local) {
    let x = *EXP;
    if x == 1 {
        return;
    }
    let mut ws = [(0usize, 0u64); 16];
    for (r, w) in batch.iter().zip(ws.iter_mut()) {
        *w = bit(r.g, if x == 7 { (r.h >> 47) as u32 } else { names.id(b, r.s, r.n, r.h, loc) });
        prefetch(&bits[w.0]);
    }
    if x == 2 {
        std::hint::black_box(&ws);
        return;
    }
    for &(w, m) in &ws[..batch.len()] {
        if x == 6 {
            bits[w].store(bits[w].load(Relaxed) | m, Relaxed);
        } else {
            bits[w].fetch_or(m, Relaxed);
        }
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
    let bits: Vec<AtomicU64> = zeroed((names.max_ids as usize / 4096 + 1) * 256 * 64);
    let (next, ends) = (AtomicUsize::new(0), Mutex::new(vec![0; k]));
    let aggs: Vec<[Agg; 256]> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let (mut agg, mut loc) = ([NEW; 256], Local::default());
                    loop {
                        let j = next.fetch_add(1, Relaxed);
                        if j >= k {
                            break agg;
                        }
                        let e = chunk(b, cut[j], cut[j + 1], tab, &names, &bits, &mut loc, &mut agg);
                        ends.lock().unwrap()[j] = e;
                    }
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let ends = ends.into_inner().unwrap();
    eprintln!("names {} arena words {}", names.next_id.load(Relaxed), names.top.load(Relaxed));
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
        .map(|g| bits.chunks(256 * 64).map(|page| page[g * 64..g * 64 + 64].iter().map(|w| w.load(Relaxed).count_ones() as u64).sum::<u64>()).sum())
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
    let seen: Vec<AtomicU8> = zeroed(26 * 26 * 26);
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
    let (mut cap, mut t) = (1 << 18, threads);
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
    eprintln!("regions {t1:?} records {t2:?} total {:?}", t0.elapsed());
}

/// First line start at or after i.
fn guess_line(b: &[u8], mut i: usize) -> usize {
    while i > 0 && i < b.len() && b[i - 1] != b'\n' {
        i += 1;
    }
    i
}
