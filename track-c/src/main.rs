// Track C, featherweight: little memory per unit of speed.
//
// Parsing. Threads take byte ranges of records.csv that start at a guessed record boundary; the
// guess is proved afterwards (each range must end exactly where the next one begins, see `run`).
// Each 64-byte block gives bit masks of quotes, commas and LFs; a carry-less multiply turns the
// quote bits into an inside-quotes mask, and the commas and LFs outside quotes are the field ends.
// Every record has exactly five of them, so its fields come from bit positions alone.
//
// Aggregation. Names go to one shared dictionary (names inline in 24-byte slots, dense ids), and
// distinct names per country are one bit per (country, name id): about 3 MB in all, instead of a
// hash set per thread. The input is mapped, not copied, and is never compared against: a name is
// compared with its dictionary slot, which the parser prefetched.
use std::arch::x86_64::*;
use std::sync::{atomic::Ordering::*, atomic::*, Mutex};
use std::{fs::File, os::unix::io::AsRawFd};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

/// n zeroed values in fresh anonymous memory: charged only when touched, never freed (the process
/// ends first), and kept off 2 MB pages (MADV_NOHUGEPAGE). The scoring box backs memory with 2 MB
/// pages, and memory.peak would charge a whole one for its first touched byte.
fn zeroed<T>(n: usize) -> &'static [T] {
    let len = n * std::mem::size_of::<T>();
    let p = unsafe { mmap(std::ptr::null_mut(), len.max(1), 3, 0x22, -1, 0) }; // RW, private, anon
    unsafe { madvise(p, len, 15) };
    unsafe { std::slice::from_raw_parts(p as *const T, n) }
}

/// A file mapped read-only over zeroed memory, so loads may run up to 4 KB past its end. The page
/// cache is not charged to us; a copy would be.
fn map(path: &str) -> &'static [u8] {
    let f = File::open(path).unwrap();
    let len = f.metadata().unwrap().len() as usize;
    let p = zeroed::<u8>(len + 8192).as_ptr() as *mut u8;
    let q = unsafe { mmap(p, len, 1, 0x12, f.as_raw_fd(), 0) }; // read-only, private, fixed
    assert!(len == 0 || p == q);
    unsafe { madvise(p, len, 2) }; // MADV_SEQUENTIAL
    unsafe { std::slice::from_raw_parts(p, len) }
}

fn word(b: &[u8], i: usize) -> u64 {
    unsafe { (b.as_ptr().add(i) as *const u64).read_unaligned() }
}

fn at(b: &[u8], i: usize) -> u8 {
    word(b, i) as u8
}

/// Position of the first byte c in b[i..], or b.len().
fn find(b: &[u8], i: usize, c: u8) -> usize {
    i + b[i..].iter().position(|&x| x == c).unwrap_or(b.len() - i)
}

fn prefetch<T>(p: *const T) {
    unsafe { _mm_prefetch(p as *const i8, _MM_HINT_T0) };
}

/// The number in b[s..e]: digits, or for money a maybe `-`, digits, `.` and two digits, read as
/// cents. Up to 8 bytes in one go: one load that ends at e, the dot squeezed out, the bytes before
/// the number masked off (they act as leading zeros), then pairs, quads and octets combined with
/// three multiplies. A longer field (never generated) takes the slow loop.
fn num(b: &[u8], s: usize, e: usize, money: bool) -> i64 {
    let (neg, x) = (money && at(b, s) == b'-', word(b, e - 8));
    let squeezed = (x & 0xFF_FFFF_FFFF) << 8 | x & 0xFFFF << 48;
    let x = if money { squeezed } else { x };
    let digits = 8 * (e - s - neg as usize - money as usize) as u32;
    let v = x & 0x0F0F_0F0F_0F0F_0F0F & u64::MAX.wrapping_shl(64u32.wrapping_sub(digits));
    let v = v.wrapping_mul(2561) >> 8 & 0x00FF_00FF_00FF_00FF;
    let v = v.wrapping_mul(6553601) >> 16 & 0x0000_FFFF_0000_FFFF;
    let mut v = v.wrapping_mul(42949672960001) >> 32;
    if e - s > 8 {
        let d = (s..e).filter(|&i| at(b, i).is_ascii_digit());
        v = d.fold(0, |x, i| 10 * x + at(b, i) as u64 - 48);
    }
    if neg { -(v as i64) } else { v as i64 }
}

/// Word j of the name b[s..s + n], zero-padded past its end.
fn kw(b: &[u8], s: usize, n: usize, j: usize) -> u64 {
    let len = (8 * n.saturating_sub(8 * j)).min(64) as u32;
    unsafe { _bzhi_u64(word(b, s + 8 * j), len) }
}

/// Running aggregates of one group: count, id sum, amount sum, min, max, name bytes, note bytes,
/// distinct names (bits this thread turned on).
type Agg = [i64; 8];
const NEW: Agg = [0, 0, 0, i64::MAX, i64::MIN, 0, 0, 0];
type Words = &'static [AtomicU64];

/// The state threads share in one run: tab (region id -> group: 0 is _UNMATCHED, countries are
/// 1..); the name dictionary (slots; words of names over 16 bytes, with the next free word; the
/// next free block of 64 ids); and one bit per (group, name id), with a lock per region of it.
///
/// A dictionary slot is 24 bytes: a header `n | pairs << 20 | (id + 1) << 40` (0 = empty, BUSY
/// while being filled; pairs = the `""` in the raw name) and the name's 16 bytes, zero-padded (89%
/// of names fit; a longer one keeps its words in `long` and their offset in the slot). A lookup is
/// one or two prefetched lines. The raw
/// (still escaped) name is the key: escaping is a bijection, and `""` and an empty unquoted field
/// both give the empty key.
///
/// The bits live in pages of 4096 names: [page][group][64 words], so a group's bits for one page
/// are contiguous and pages past the last id are never touched. No locked instruction per bit: a
/// locked OR waits for its line, which another core has usually just written, and blocks every
/// later load meanwhile. Each thread queues (word, bit) pairs per region of the array (64 regions)
/// and applies a full queue with plain loads and stores under that region's lock. A word always
/// maps to the same lock, so each bit turns on exactly once: that is the distinct-name count.
struct Shared<'a>(&'a [AtomicU8], Words, Words, AtomicUsize, AtomicU32, Words, Vec<Mutex<()>>);

const BUSY: u64 = u64::MAX;

/// One thread's state: aggregates per group, its block of free ids, and its bit queues (64 regions
/// x 64 pairs `word << 6 | bit`, and their lengths).
type Local = ([Agg; 256], (u32, u32), Pend);
type Pend = ([[u32; 64]; 64], [usize; 64]);

impl Shared<'_> {
    /// Dense id and `""` count of the raw name b[s..s + n] with hash h. An overfull dictionary
    /// sets the id counter past every limit, and the run is redone.
    fn id(&self, b: &[u8], s: usize, n: usize, h: u64, ids: &mut (u32, u32)) -> (u32, usize) {
        let Shared(_, slots, long, top, next, ..) = self;
        let (w, nw) = (|j: usize| kw(b, s, n, j), (n + 7) / 8);
        let cap = slots.len() / 3;
        let mut i = (h >> cap.leading_zeros() + 1) as usize;
        loop {
            let slot = &slots[3 * i..3 * i + 3];
            let hdr = slot[0].load(Acquire);
            if hdr == 0 && slot[0].compare_exchange(0, BUSY, Acquire, Relaxed).is_ok() {
                if ids.0 == ids.1 {
                    *ids = (next.fetch_add(64, Relaxed), 0);
                    ids.1 = ids.0 + 64;
                }
                let o = if n > 16 { top.fetch_add(nw, Relaxed) } else { 0 };
                if ids.0 as usize >= cap / 8 * 7 || o + nw > long.len() || n >= 1 << 20 {
                    next.store(u32::MAX / 2, Relaxed);
                    slot[0].store(0, Release);
                    return (0, 0);
                }
                for j in 0..2 {
                    slot[1 + j].store(if n > 16 { (j == 0) as u64 * o as u64 } else { w(j) }, Relaxed);
                }
                for j in 0..nw * (n > 16) as usize {
                    long[o + j].store(w(j), Relaxed);
                }
                let pairs = (s..s + n).filter(|&j| at(b, j) == b'"').count() / 2;
                slot[0].store((n | pairs << 20) as u64 | (ids.0 as u64 + 1) << 40, Release);
                ids.0 += 1;
                return (ids.0 - 1, pairs);
            }
            if hdr == BUSY || hdr == 0 {
                std::hint::spin_loop(); // another thread is filling this slot: look again
                continue;
            }
            let o = slot[1].load(Relaxed);
            let short = (o ^ w(0)) | (slot[2].load(Relaxed) ^ w(1));
            let at_o = |j: usize| long.get(o as usize + j).map(|x| x.load(Relaxed));
            let same = || if n > 16 { (0..nw).all(|j| at_o(j) == Some(w(j))) } else { short == 0 };
            if hdr as usize & 0xF_FFFF == n && same() {
                return ((hdr >> 40) as u32 - 1, (hdr >> 20) as usize & 0xF_FFFF);
            }
            i = (i + 1) & (cap - 1);
        }
    }

    fn set(&self, (q, len): &mut Pend, agg: &mut [Agg; 256], g: usize, id: u32) {
        let w = ((id >> 12) as usize * 256 + g) * 64 + (id as usize >> 6 & 63);
        let r = (w ^ w >> 6) & 63;
        q[r][len[r]] = (w as u32) << 6 | id & 63;
        len[r] += 1;
        if len[r] == 64 {
            self.apply(&q[r], &mut len[r], r, agg);
        }
    }

    fn apply(&self, q: &[u32; 64], len: &mut usize, r: usize, agg: &mut [Agg; 256]) {
        let _lock = self.6[r].lock().unwrap();
        for &x in &q[..*len] {
            let (a, m) = (&self.5[(x >> 6) as usize], 1 << (x & 63));
            let old = a.load(Relaxed);
            a.store(old | m, Relaxed);
            agg[(x >> 12) as usize & 255][7] += (old & m == 0) as i64; // x = (page, group, word, bit)
        }
        *len = 0;
    }
}

/// A parsed record waiting for its lookups: name start, length, note length, whether the name is
/// quoted, the quotes in the record, region, id, amount, name hash.
#[derive(Clone, Copy, Default)]
struct Rec(usize, usize, usize, usize, usize, i64, i64, i64, u64);

/// Parse the records that start in b[start..end) (start is a record start). Returns where parsing
/// stopped: exactly `end` when `end` is a record boundary.
fn chunk(b: &[u8], start: usize, end: usize, sh: &Shared, me: &mut Local) -> usize {
    let (mut base, mut carry, mut quotes, mut rs) = (start, 0u64, 0usize, start);
    // Ring of field-end positions (n written, i consumed); quotes before the record start.
    let (mut pos, mut n, mut i, mut qrs) = ([0usize; 128], 0usize, 0usize, 0usize);
    let (mut batch, mut nb, shift) = ([Rec::default(); 32], 0, (sh.1.len() / 3).leading_zeros() + 1);
    'blocks: while rs < end && base < b.len() {
        let (q, c, l) = unsafe {
            let p = b.as_ptr().add(base) as *const __m256i;
            let (lo, hi) = (_mm256_loadu_si256(p), _mm256_loadu_si256(p.add(1)));
            let m = |c| {
                let c = _mm256_set1_epi8(c as i8);
                let m = |v| _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, c)) as u32 as u64;
                m(lo) | m(hi) << 32
            };
            (m(b'"'), m(b','), m(b'\n'))
        };
        // Prefix XOR of the quote bits: set inside quoted fields (and on their opening quote).
        let (qv, ones) = unsafe { (_mm_set_epi64x(0, q as i64), _mm_set1_epi8(-1)) };
        let x = unsafe { _mm_cvtsi128_si64(_mm_clmulepi64_si128(qv, ones, 0)) } as u64 ^ carry;
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
            // id , name , note , region , amount (CR) LF; a BOM may precede the id. The fifth
            // position is always in this block, so the record's quotes are counted up to there; the
            // name's are known to the dictionary, and the rest are the note's.
            let at5 = |j: usize| pos[(i + j) & 127];
            let (c0, c1, c2, c3, lf) = (at5(0), at5(1), at5(2), at5(3), at5(4));
            let qlf = quotes + unsafe { _bzhi_u64(q, (lf - base) as u32) }.count_ones() as usize;
            let p = rs + 3 * (at(b, rs) == 0xEF) as usize;
            let e = lf - (at(b, lf - 1) == b'\r') as usize;
            let (id, region) = (num(b, p, c0, false), num(b, c2 + 1, c3, false));
            let v = num(b, c3 + 1, e, true);
            let quoted = (at(b, c0 + 1) == b'"') as usize;
            let s = c0 + 1 + quoted;
            let n = (c1 - quoted).saturating_sub(s);
            let (k0, k1) = (kw(b, s, n, 0) ^ n as u64, kw(b, s, n, 1));
            let m = (k0 ^ 0x243F_6A88_85A3_08D3) as u128 * (k1 ^ 0x1319_8A2E_0370_7344) as u128;
            let h = (m as u64 ^ (m >> 64) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let line = &sh.1[3 * (h >> shift) as usize] as *const AtomicU64;
            prefetch(line);
            prefetch(line.wrapping_add(8)); // the next line: probes often run on
            prefetch(sh.0.as_ptr().wrapping_add(region as usize)); // a prefetch never faults
            batch[nb] = Rec(s, n, c2 - c1 - 1, quoted, qlf - qrs, region, id, v, h);
            (nb, qrs, rs, i) = (nb + 1, qlf, lf + 1, i + 5);
            if nb == batch.len() {
                flush(b, &batch, sh, me);
                nb = 0;
            }
            if rs >= end {
                break 'blocks;
            }
        }
        (quotes, base) = (quotes + q.count_ones() as usize, base + 64);
    }
    flush(b, &batch[..nb], sh, me);
    rs
}

/// Lookups for a batch: its region bytes and dictionary slots were prefetched while it was parsed.
fn flush(b: &[u8], batch: &[Rec], sh: &Shared, (agg, ids, pend): &mut Local) {
    for &Rec(s, n, note, quoted, quotes, region, id, v, h) in batch {
        let g = sh.0.get(region as usize).map_or(0, |g| g.load(Relaxed)) as usize;
        let (name, pairs) = sh.id(b, s, n, h, ids);
        let tq = quotes.wrapping_sub(quoted * (2 + 2 * pairs)); // the note's quotes
        let a = &mut agg[g];
        (a[0], a[1], a[2]) = (a[0] + 1, a[1] + id, a[2] + v);
        (a[3], a[4], a[5]) = (a[3].min(v), a[4].max(v), a[5] + (n - pairs) as i64);
        a[6] += note.wrapping_sub(tq / 2 + (tq > 0) as usize) as i64;
        sh.set(pend, agg, g, name);
    }
}

/// k + 1 cut points from `from` to b.len(): the first line start at or after each k-th of the way,
/// found by the shape of the data, a LF followed by a digit or a byte order mark. Inside quotes a
/// LF is always followed by a letter or a quote (spec 2.2), and `run` still proves every cut.
fn cuts(b: &[u8], from: usize, k: usize) -> Vec<usize> {
    let start = |i: usize| at(b, i - 1) == b'\n' && (at(b, i).is_ascii_digit() || at(b, i) == 0xEF);
    let line = |j: usize| ((from + (b.len() - from) * j / k).max(1)..b.len()).find(|&i| start(i));
    (0..=k).map(|j| if j > 0 { line(j).unwrap_or(b.len()) } else { from }).collect()
}

/// Run `f` on `threads` threads and collect the results.
fn par<T: Send>(threads: usize, f: impl Fn() -> T + Sync) -> Vec<T> {
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads).map(|_| s.spawn(&f)).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Job numbers below n, handed out one at a time.
fn jobs(next: &AtomicUsize, n: usize) -> impl Iterator<Item = usize> + '_ {
    std::iter::repeat_with(|| next.fetch_add(1, Relaxed)).take_while(move |&j| j < n)
}

/// Aggregates and distinct-name counts per group. Attempt 0 uses all threads; a later attempt
/// (never needed on generated data) uses one thread and a dictionary 4 times larger. None if a cut
/// was not a record boundary or the dictionary filled up.
///
/// Why a cut is proved: the first range starts right after the header, a true record start in
/// the outside-quotes state, so its parse is exact and stops at the first record boundary at or
/// after its end. If that equals the next cut, the next cut is a true record start too, and so on
/// by induction. A cut inside a quoted field shows up as a range that ends elsewhere.
fn run(b: &[u8], tab: &[AtomicU8], attempt: u32, threads: usize) -> Option<Vec<Agg>> {
    let (cap, threads) = (1 << (17 + 2 * attempt), if attempt > 0 { 1 } else { threads });
    if attempt > 0 {
        eprintln!("warning: parallel run rejected; attempt {attempt} on one thread, {cap} slots");
    }
    let cut = cuts(b, find(b, 0, b'\n') + 1, if threads > 1 { threads * 48 } else { 1 });
    let locks = (0..64).map(|_| Mutex::new(())).collect();
    let bits = zeroed((cap / 8 * 7 / 4096 + 1) * 256 * 64);
    let (top, next) = (AtomicUsize::new(0), AtomicU32::new(0));
    let sh = Shared(tab, zeroed(3 * cap), zeroed(1 << 23), top, next, bits, locks);
    let (ends, next): (Vec<AtomicUsize>, _) = (cut.iter().map(|_| 0.into()).collect(), 0.into());
    let aggs = par(threads, || {
        let mut me: Local = ([NEW; 256], (0, 0), ([[0; 64]; 64], [0; 64]));
        for j in jobs(&next, cut.len() - 1) {
            ends[j].store(chunk(b, cut[j], cut[j + 1], &sh, &mut me), Relaxed);
        }
        (0..64).for_each(|r| sh.apply(&me.2 .0[r], &mut me.2 .1[r], r, &mut me.0));
        me.0
    });
    if sh.4.load(Relaxed) as usize >= cap / 8 * 7 || (1..cut.len() - 1).any(|j| ends[j - 1].load(Relaxed) != cut[j]) {
        return None;
    }
    let mut total = vec![NEW; 256];
    for (g, x) in aggs.iter().flat_map(|a| a.iter().enumerate()) {
        let t = &mut total[g];
        (t[0], t[1], t[2], t[3]) = (t[0] + x[0], t[1] + x[1], t[2] + x[2], t[3].min(x[3]));
        (t[4], t[5], t[6], t[7]) = (t[4].max(x[4]), t[5] + x[5], t[6] + x[6], t[7] + x[7]);
    }
    Some(total)
}

fn money(c: i64) -> String {
    let sign = if c < 0 { "-" } else { "" };
    format!("{sign}{}.{:02}", c.abs() / 100, c.abs() % 100)
}

fn main() {
    std::env::set_var("RUST_MIN_STACK", "262144"); // small thread stacks, never on a 2 MB page
    let cores = std::thread::available_parallelism().map_or(1, |p| p.get());
    let threads = std::env::var("FW_THREADS").map_or(cores, |v| v.parse().unwrap());

    // regions.csv: a header, then "id,CCC" lines. tab maps a region id to its group: 0 is
    // _UNMATCHED, and countries are numbered 1.. in order of first sight (codes[g]).
    let rg = map("/data/regions.csv");
    let body = &rg[find(rg, 0, b'\n') + 1..];
    let tab: &[AtomicU8] = zeroed(1 << 24); // region ids are below 10^7; only touched pages count
    let (codes, seen) = (Mutex::new(vec![[255u8; 3]]), zeroed::<AtomicU8>(1 << 15));
    let (cut, next) = (cuts(body, 0, threads * 4), AtomicUsize::new(0));
    par(threads, || {
        for j in jobs(&next, cut.len() - 1) {
            let mut p = cut[j];
            while p < cut[j + 1] {
                let c = find(body, p, b',');
                let code = unsafe { _pext_u64(word(body, c + 1), 0x1F1F1F) } as usize;
                if seen[code].load(Acquire) == 0 {
                    let mut list = codes.lock().unwrap();
                    if seen[code].load(Relaxed) == 0 {
                        list.push(body[c + 1..c + 4].try_into().unwrap());
                        assert!(list.len() < 256, "more than 255 countries");
                        seen[code].store(list.len() as u8 - 1, Release);
                    }
                }
                tab[num(body, p, c, false) as usize].store(seen[code].load(Acquire), Relaxed);
                p = c + 5;
            }
        }
    });
    let codes = codes.into_inner().unwrap();

    let rc = map("/data/records.csv");
    let total = (0..).find_map(|k| run(rc, tab, k, threads)).unwrap();
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
