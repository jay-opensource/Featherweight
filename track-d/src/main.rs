// Track D, featherweight. Every edge is 4 bytes and every node 1 byte plus 1/16 of a word.
// Nodes are relabelled in breadth-first order, so a search touches nearby memory, and each query
// runs as a two-ended Dijkstra whose labels live in small per-thread pages, not in arrays the
// size of the graph. See PERF.md for the measurements behind each choice.
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicI64, AtomicU16, AtomicU32, AtomicUsize, Ordering::Relaxed};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

/// The scoring box backs anonymous memory with 2 MB pages (THP "always"), and memory.peak charges
/// a whole 2 MB page for its first touched byte. So every allocation of 256 KB or more is marked
/// MADV_NOHUGEPAGE, and only the graph arrays, which are dense and hot, opt back in (`huge`).
struct Alloc;

unsafe impl std::alloc::GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        advise(std::alloc::System.alloc(l), l.size(), 15)
    }
    unsafe fn alloc_zeroed(&self, l: std::alloc::Layout) -> *mut u8 {
        advise(std::alloc::System.alloc_zeroed(l), l.size(), 15)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        std::alloc::System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
        advise(std::alloc::System.realloc(p, l, n), n, 15)
    }
}

#[global_allocator]
static ALLOC: Alloc = Alloc;

/// madvise the whole pages inside [p, p + len) when len >= 256 KB (14 = HUGEPAGE, 15 = NOHUGEPAGE).
fn advise(p: *mut u8, len: usize, advice: i32) -> *mut u8 {
    let (lo, hi) = ((p as usize + 4095) & !4095, (p as usize + len) & !4095);
    if len >= 1 << 18 && hi > lo {
        unsafe { madvise(lo as *mut u8, hi - lo, advice) };
    }
    p
}

fn huge<T>(v: &mut Vec<T>) {
    advise(v.as_mut_ptr() as *mut u8, v.capacity() * std::mem::size_of::<T>(), 14);
}

/// Read-only private mapping of a whole file; the page cache is not charged to us, a heap copy would be.
fn map(path: &str) -> &'static [u8] {
    let f = File::open(path).expect(path);
    let len = f.metadata().unwrap().len() as usize;
    if len == 0 {
        return &[];
    }
    let p = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) };
    assert!(p as isize != -1, "mmap {path}");
    unsafe { std::slice::from_raw_parts(p, len) }
}

/// Parse the decimal number at b[i..] and step past the space or LF after it. Up to 7 digits in
/// one go: load 8 bytes, find the first non-digit, shift the digits to the top (the empty low
/// bytes act as leading zeros) and combine pairs, quads and octets with three multiplies.
#[inline(always)]
fn num(b: &[u8], i: &mut usize) -> u64 {
    if let Some(w) = b.get(*i..*i + 8) {
        let d = u64::from_le_bytes(w.try_into().unwrap()) ^ 0x3030_3030_3030_3030;
        let stop = d.wrapping_add(0x7676_7676_7676_7676) & 0x8080_8080_8080_8080; // bytes >= 10: not a digit
        let len = stop.trailing_zeros() as usize / 8;
        if len < 8 {
            *i += len + 1;
            let v = if len == 0 { 0 } else { (d << (64 - 8 * len)) & 0x0F0F_0F0F_0F0F_0F0F };
            let v = v.wrapping_mul(2561) >> 8 & 0x00FF_00FF_00FF_00FF;
            let v = v.wrapping_mul(6553601) >> 16 & 0x0000_FFFF_0000_FFFF;
            return v.wrapping_mul(42949672960001) >> 32;
        }
    }
    let mut x = 0u64;
    while let Some(&c) = b.get(*i).filter(|c| c.is_ascii_digit()) {
        x = x * 10 + (c - b'0') as u64;
        *i += 1;
    }
    *i += 1; // the space or LF after the number
    x
}

/// Hint the cache to fetch the line holding `x`.
#[inline(always)]
fn prefetch<T>(x: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(x as *const i8, std::arch::x86_64::_MM_HINT_T0)
    };
}

/// s[i] without a bounds check, for the hot lookups (their indices come from the graph).
#[inline(always)]
fn ld<T: Copy>(s: &[T], i: usize) -> T {
    unsafe { *s.get_unchecked(i) }
}

/// &mut s[i] without a bounds check.
#[inline(always)]
fn mt<T>(s: &mut [T], i: usize) -> &mut T {
    unsafe { s.get_unchecked_mut(i) }
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

/// `0..n` in `k` ranges.
fn spans(n: usize, k: usize) -> Vec<(usize, usize)> {
    (0..k).map(|j| (j * n / k, (j + 1) * n / k)).collect()
}

/// Run `f` on `threads` threads, each pulling work items from `items` until they run out.
fn par<T: Sync>(threads: usize, items: &[T], f: impl Fn(&T) + Sync) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                while let Some(it) = items.get(next.fetch_add(1, Relaxed)) {
                    f(it);
                }
            });
        }
    });
}

/// Shared mutable array written at disjoint indices by several threads.
struct Slots<T>(*mut T);
unsafe impl<T> Sync for Slots<T> {}

/// Write edge (v, w) at slot p: one slot `v | w << vb`, or, when w does not fit, the escape
/// value in that slot and the full weight in the next. Returns the slots used.
#[inline(always)]
fn store(out: &Slots<u32>, p: usize, v: u32, w: u32, vb: u32, esc: u32) -> usize {
    unsafe {
        *out.0.add(p) = v | w.min(esc) << vb;
        if w >= esc {
            *out.0.add(p + 1) = w;
            return 2;
        }
    }
    1
}

/// Compact CSR. Node k's slots start at base[k >> sh] + rel[k] (see `store` for a slot).
struct Graph {
    edges: Vec<u32>,
    base: Vec<u32>,
    rel: Vec<u8>,
    sh: u32,
    vb: u32,
    esc: u32,
}

impl Graph {
    #[inline(always)]
    fn off(&self, k: usize) -> usize {
        ld(&self.base, k >> self.sh) as usize + ld(&self.rel, k) as usize
    }

    /// Call f(v, w) for every edge u -> v of weight w.
    #[inline(always)]
    fn adj(&self, u: usize, f: impl FnMut(u32, u32)) {
        self.slots(self.off(u), self.off(u + 1), f)
    }

    /// Call f(v, w) for the edges stored in slots e..end.
    #[inline(always)]
    fn slots(&self, mut e: usize, end: usize, mut f: impl FnMut(u32, u32)) {
        while e < end {
            let x = ld(&self.edges, e);
            let mut w = x >> self.vb;
            e += 1;
            if w == self.esc {
                w = self.edges[e];
                e += 1;
            }
            f(x & ((1 << self.vb) - 1), w);
        }
    }
}

/// Offsets for nodes 0..n with `slots(k)` slots each: the largest block (16 nodes on these graphs)
/// whose in-block offsets fit a u8. Blocks are independent, so threads fill them; only the u32
/// block bases are a serial prefix sum. Returns (base per block + sentinel, rel per node + sentinel,
/// shift).
fn layout(n: usize, threads: usize, slots: impl Fn(usize) -> u32 + Sync) -> (Vec<u32>, Vec<u8>, u32) {
    let mut sh = 4u32;
    loop {
        let nb = (n >> sh) + 1;
        let (mut rel, mut sums) = (vec![0u8; n + 1], vec![0u32; nb]);
        let (rp, sp, ok) = (Slots(rel.as_mut_ptr()), Slots(sums.as_mut_ptr()), std::sync::atomic::AtomicBool::new(true));
        par(threads, &spans(nb, threads * 4), |&(lo, hi)| {
            let (rp, sp) = (&rp, &sp);
            for b in lo..hi {
                let mut acc = 0u32;
                for k in b << sh..((b + 1) << sh).min(n + 1) {
                    if acc > 255 {
                        ok.store(false, Relaxed);
                    }
                    unsafe { *rp.0.add(k) = acc as u8 };
                    acc += if k < n { slots(k) } else { 0 };
                }
                unsafe { *sp.0.add(b) = acc };
            }
        });
        if ok.into_inner() || sh == 0 {
            let mut base = Vec::with_capacity(nb + 1);
            let mut total = 0u32;
            for &x in &sums {
                base.push(total);
                total += x;
            }
            base.push(total);
            huge(&mut rel);
            return (base, rel, sh);
        }
        sh -= 1;
    }
}

/// Build a CSR over n nodes from an edge stream: `each(item, emit)` calls emit(u, v, w) for the
/// edges of one work item, and runs twice. Every random access stays inside a small region:
/// 1. count each item's slots per bucket of 8192 nodes (213 counters per item, no atomics);
/// 2. append each slot to its bucket's region through the item's own cursor, with the node's
///    place inside the bucket in a parallel u16 array (213 sequential streams per thread; both
///    slots of an escaped edge get it, so the slots can be counted and moved one by one);
/// 3. per bucket, count each node's slots from the u16s alone; once the layout is known, reorder
///    the bucket's ~94 KB of slots by node through a scratch copy.
/// Nothing is locked, and no store lands outside the bucket being worked on.
fn csr<T: Sync>(n: usize, threads: usize, vb: u32, esc: u32, items: &[T], each: impl Fn(&T, &mut dyn FnMut(usize, u32, u32)) + Sync) -> Graph {
    const BS: u32 = 13;
    let (nb, ni) = ((n >> BS) + 1, items.len());
    let idx: Vec<usize> = (0..ni).collect();
    let mut cur = vec![0u32; ni * nb];
    let cp = Slots(cur.as_mut_ptr());
    par(threads, &idx, |&i| {
        let c = unsafe { std::slice::from_raw_parts_mut((&cp).0.add(i * nb), nb) };
        each(&items[i], &mut |u, _, w| c[u >> BS] += 1 + (w >= esc) as u32);
    });
    let (mut start, mut acc) = (vec![0usize; nb + 1], 0u32);
    for b in 0..nb {
        start[b] = acc as usize;
        for i in 0..ni {
            (cur[i * nb + b], acc) = (acc, acc + cur[i * nb + b]);
        }
    }
    start[nb] = acc as usize;
    let (mut edges, mut loc) = (vec![0u32; acc as usize], vec![0u16; acc as usize]);
    huge(&mut edges);
    let (ep, lp) = (Slots(edges.as_mut_ptr()), Slots(loc.as_mut_ptr()));
    par(threads, &idx, |&i| {
        let (ep, lp) = (&ep, &lp);
        let c = unsafe { std::slice::from_raw_parts_mut((&cp).0.add(i * nb), nb) };
        each(&items[i], &mut |u, v, w| {
            let (p, l) = (c[u >> BS] as usize, (u & ((1 << BS) - 1)) as u16);
            let k = store(ep, p, v, w, vb, esc);
            c[u >> BS] += k as u32;
            unsafe { (*lp.0.add(p), *lp.0.add(p + k - 1)) = (l, l) };
        });
    });
    let mut deg = vec![0u16; n + 1];
    let dp = Slots(deg.as_mut_ptr());
    let buckets: Vec<usize> = (0..nb).collect();
    par(threads, &buckets, |&b| {
        for &l in &loc[start[b]..start[b + 1]] {
            let d = unsafe { &mut *(&dp).0.add(b << BS | l as usize) };
            *d = d.checked_add(1).expect("out-degree above 65k");
        }
    });
    let (base, rel, sh) = layout(n, threads, |k| deg[k] as u32);
    par(threads, &buckets, |&b| {
        let (lo, hi) = (start[b], start[b + 1]);
        let e = unsafe { std::slice::from_raw_parts((&ep).0.add(lo), hi - lo) }.to_vec();
        let mut at: Vec<usize> = (b << BS..((b + 1) << BS).min(n + 1)).map(|k| base[k >> sh] as usize + rel[k] as usize).collect();
        for (&x, &l) in e.iter().zip(&loc[lo..hi]) {
            unsafe { *(&ep).0.add(at[l as usize]) = x };
            at[l as usize] += 1;
        }
    });
    Graph { edges, base, rel, sh, vb, esc }
}

/// Breadth-first relabelling, in parallel: each thread grows its own region from a seed in its
/// slice of the id range (claiming nodes with a CAS) and re-seeds from that slice when its region
/// is enclosed. New ids follow thread 0's visit order, then thread 1's, and so on.
fn bfs_order(g: &Graph, threads: usize) -> Vec<u32> {
    const FREE: u32 = u32::MAX;
    let n = g.rel.len() - 1;
    let id: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(FREE)).collect();
    let claim = |v: usize| id[v].load(Relaxed) == FREE && id[v].compare_exchange(FREE, 0, Relaxed, Relaxed).is_ok();
    let lists: Vec<Vec<u32>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                let claim = &claim;
                s.spawn(move || {
                    let mut list: Vec<u32> = Vec::with_capacity(n); // only touched pages are charged
                    let (mut scan, end, mut head) = (t * n / threads, (t + 1) * n / threads, 0);
                    loop {
                        if head == list.len() {
                            while scan < end && !claim(scan) {
                                scan += 1;
                            }
                            if scan == end {
                                break;
                            }
                            list.push(scan as u32);
                        }
                        // Software pipeline over the queue: offsets 16 nodes ahead, edge lists 8
                        // ahead, so the misses of upcoming nodes overlap with this one's work.
                        if let Some(&w) = list.get(head + 16) {
                            prefetch(&g.rel[w as usize]);
                            prefetch(&g.base[w as usize >> g.sh]);
                        }
                        if let Some(x) = list.get(head + 8).and_then(|&w| g.edges.get(g.off(w as usize))) {
                            prefetch(x);
                        }
                        head += 1;
                        g.adj(list[head - 1] as usize, |v, _| {
                            if claim(v as usize) {
                                list.push(v);
                            }
                        });
                    }
                    list
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut start = vec![0u32];
    for l in &lists {
        start.push(start.last().unwrap() + l.len() as u32);
    }
    let jobs: Vec<usize> = (0..lists.len()).collect();
    par(threads, &jobs, |&j| {
        for (i, &v) in lists[j].iter().enumerate() {
            id[v as usize].store(start[j] + i as u32, Relaxed);
        }
    });
    id.into_iter().map(|a| a.into_inner()).collect()
}

/// Copy `g` into the order given by `newid` (old id -> new id), each node's slots by one thread,
/// with every target rewritten to its new id.
fn relabel(g: &Graph, newid: &[u32], threads: usize) -> Graph {
    let n = newid.len();
    let ranges = spans(n, threads * 4);
    let deg: Vec<AtomicU16> = (0..n).map(|_| AtomicU16::new(0)).collect();
    par(threads, &ranges, |&(lo, hi)| {
        for u in lo..hi {
            deg[newid[u] as usize].store((g.off(u + 1) - g.off(u)) as u16, Relaxed);
        }
    });
    let (base, rel, sh) = layout(n, threads, |k| deg[k].load(Relaxed) as u32);
    drop(deg);
    let mut edges = vec![0u32; *base.last().unwrap() as usize];
    huge(&mut edges);
    let out = Slots(edges.as_mut_ptr());
    par(threads, &ranges, |&(lo, hi)| {
        let out = &out;
        for u in lo..hi {
            if u + 8 < hi {
                // Prefetch where node u+8 goes and the new ids of its targets.
                let w = newid[u + 8] as usize;
                prefetch(&rel[w]);
                prefetch(&base[w >> sh]);
                g.adj(u + 8, |v, _| prefetch(&newid[v as usize]));
            }
            let k = newid[u] as usize;
            let mut p = base[k >> sh] as usize + rel[k] as usize;
            g.adj(u, |v, w| p += store(out, p, newid[v as usize], w, g.vb, g.esc));
        }
    });
    Graph { edges, base, rel, sh, vb: g.vb, esc: g.esc }
}

/// A queued node: its label's index in the pages, then its first and end edge slot.
type Item = [u32; 3];

/// Buckets per queue: the search relaxes only edges lighter than W (see `bidir`).
const W: usize = 1 << 13;

/// Monotone bucket queue (Dial): one bucket per key modulo W. Each key pushed is the popped key
/// cur plus an edge lighter than W, so every queued key lies in [cur, cur + W) and a bucket's
/// position gives its key; a bitmap of non-empty buckets finds the next one. A bucket is a list
/// through a pool of entries (an Item and the next entry's index) that reuses popped entries. The
/// Item holds all that settling the node needs, so a pop starts on the label and the edges at once.
/// Entry 0 is a sentinel, so index 0 means "none" in `head`, in a next field and in `free`.
#[derive(Default)]
struct Dial {
    head: Vec<u32>,
    bits: Vec<u64>,
    pool: Vec<[u32; 4]>,
    free: u32,
    cur: u32,
    len: usize,
}

impl Dial {
    /// Queue x with key d in [cur, cur + W).
    #[inline(always)]
    fn put(&mut self, d: u32, [i, a, e]: Item) {
        let b = d as usize % W;
        if self.free == 0 {
            self.free = self.pool.len() as u32;
            self.pool.push([0; 4]);
        }
        let k = self.free as usize;
        self.free = ld(&self.pool, k)[3];
        *mt(&mut self.pool, k) = [i, a, e, ld(&self.head, b)];
        *mt(&mut self.head, b) = k as u32;
        *mt(&mut self.bits, b / 64) |= 1 << b % 64;
        self.len += 1;
    }

    /// Move cur to the smallest key; false if the queue is empty.
    #[inline(always)]
    fn min(&mut self) -> bool {
        let p = self.cur as usize % W;
        let (mut j, mut m) = (p / 64, ld(&self.bits, p / 64) >> p % 64 << p % 64);
        while m == 0 && self.len > 0 {
            j = (j + 1) % (W / 64);
            m = ld(&self.bits, j);
        }
        self.cur += ((j * 64 + m.trailing_zeros() as usize + W - p) % W) as u32;
        self.len > 0
    }

    /// Take an item of key cur; min() must have returned Some since the last pop.
    #[inline(always)]
    fn pop(&mut self) -> Item {
        let b = self.cur as usize % W;
        let k = ld(&self.head, b) as usize;
        let [i, a, e, next] = ld(&self.pool, k);
        (*mt(&mut self.head, b), mt(&mut self.pool, k)[3]) = (next, self.free);
        self.free = k as u32;
        if next == 0 {
            *mt(&mut self.bits, b / 64) &= !(1 << b % 64);
        }
        self.len -= 1;
        [i, a, e]
    }

    /// Empty the queue (the first time, size it).
    fn clear(&mut self) {
        for j in 0..self.bits.len() {
            while self.bits[j] != 0 {
                self.head[j * 64 + self.bits[j].trailing_zeros() as usize] = 0;
                self.bits[j] &= self.bits[j] - 1;
            }
        }
        (self.free, self.cur, self.len) = (0, 0, 0);
        self.head.resize(W, 0);
        self.bits.resize(W / 64, 0);
        self.pool.resize(1, [0; 4]);
    }
}

/// Distances in the two-ended search are 31 bits; the top bit marks "settled".
const SETTLED: u32 = 1 << 31;
const INF: u32 = SETTLED - 1;

/// Labels of the two-ended search, per side, in pages of 64 consecutive node ids (neighbours in
/// BFS order) allocated on first touch from a per-thread pool. The two balls grow around s and
/// around t, so each side gets its own pages. dir holds a page's number in the pool as a u16 (half
/// the cache footprint of an offset), side 0 then side 1; pages grow if n ever needs more numbers.
/// Page 0 is all INF and stands for every page not allocated yet, so reading a label never branches.
struct Pages {
    dir: Vec<u16>,
    half: usize,
    pb: u32,
    pool: Vec<u32>,
    used: Vec<u32>,
}

impl Pages {
    fn new(n: usize) -> Self {
        let pb = (6..).find(|&b| 2 * ((n >> b) + 1) < u16::MAX as usize).unwrap();
        let (half, pool, used) = ((n >> pb) + 1, vec![INF; 1 << pb], Vec::new());
        Pages { dir: vec![0; 2 * half], half, pb, pool, used }
    }

    /// Index of v's label on `side`, allocating its page.
    #[inline(always)]
    fn at(&mut self, v: u32, side: usize) -> usize {
        let d = side * self.half + (v >> self.pb) as usize;
        if ld(&self.dir, d) == 0 {
            self.dir[d] = (self.pool.len() >> self.pb) as u16;
            self.pool.resize(self.pool.len() + (1 << self.pb), INF);
            self.used.push(d as u32);
        }
        self.ix(v, side)
    }

    /// Index of v's label on `side`, in page 0 if v has none.
    #[inline(always)]
    fn ix(&self, v: u32, side: usize) -> usize {
        let p = ld(&self.dir, side * self.half + (v >> self.pb) as usize) as usize;
        p << self.pb | v as usize & ((1 << self.pb) - 1)
    }

    fn clear(&mut self) {
        for &d in &self.used {
            self.dir[d as usize] = 0;
        }
        self.used.clear();
        self.pool.truncate(1 << self.pb);
    }
}

/// Two-ended Dijkstra: forward from s on `g`, backward from t on the reverse graph `r`, always
/// expanding the side with fewer queued entries. mu is the best s-t length seen through any node
/// labelled by both sides; stop when the two queue minimums together reach mu. A source with no
/// way out or a target with no way in empties its queue at once, so those answer -1 immediately;
/// s = t starts with mu = 0 and answers 0.
/// Two kinds of relaxation are skipped, and the smallest length they would have given is kept in
/// `skip`: a label that would not fit 31 bits, and an edge of weight W or more (none is reached on
/// seed 1). The search is then exact on the graph without the heavy edges for every distance below
/// 2^31. If a shorter s-t path used a heavy edge, the first one on it starts at a node the forward
/// search settled, or the last one ends at a node the backward search settled (else the path
/// would be at least the two queue minimums, hence mu), so skip is below mu. So mu is the answer
/// unless skip < mu, or something was skipped and mu does not fit 31 bits: then return None and
/// the caller runs the one-ended u64 search.
fn bidir(pg: &mut Pages, hs: &mut [Dial; 2], g: &Graph, r: &Graph, s: usize, t: usize) -> Option<i64> {
    pg.clear();
    let mut m = [if s == t { 0 } else { u64::MAX }, u64::MAX]; // mu, skip
    for (side, x, gr) in [(0, s, g), (1, t, r)] {
        let i = pg.at(x as u32, side);
        pg.pool[i] = 0;
        hs[side].clear();
        hs[side].put(0, [i as u32, gr.off(x) as u32, gr.off(x + 1) as u32]);
    }
    let mut go = true;
    while go && (hs[0].cur as u64 + hs[1].cur as u64) < m[0] {
        go = if hs[0].len > hs[1].len { step::<1>(pg, &mut hs[1], r, &mut m) } else { step::<0>(pg, &mut hs[0], g, &mut m) };
    }
    match m {
        [mu, skip] if skip < mu || skip < u64::MAX && mu >= INF as u64 => None,
        [u64::MAX, _] => Some(-1),
        [mu, _] => Some(mu as i64),
    }
}

/// One step of side S (0: forward on g, 1: backward on r, so the side is a constant in the code):
/// pop its smallest entry, settle the node unless the entry is stale, and move to the next key.
/// m is [mu, skip]. False once the side's queue is empty.
#[inline(always)]
fn step<const S: usize>(pg: &mut Pages, h: &mut Dial, gr: &Graph, m: &mut [u64; 2]) -> bool {
    let (d, [iu, e, end]) = (h.cur, h.pop());
    if ld(&pg.pool, iu as usize) == d {
        // not settled, and not a stale entry (a label only drops after a push)
        *mt(&mut pg.pool, iu as usize) |= SETTLED;
        gr.slots(e as usize, end as usize, |v, w| {
            let nd = d as u64 + w as u64;
            if nd >= INF as u64 || w as usize >= W {
                m[1] = m[1].min(nd);
                return;
            }
            let iv = pg.at(v, S);
            let lv = ld(&pg.pool, iv);
            if lv & SETTLED != 0 || lv as u64 <= nd {
                return;
            }
            *mt(&mut pg.pool, iv) = nd as u32;
            // Start fetching the node's edges now: it is usually popped later.
            let (a, b) = (gr.off(v as usize), gr.off(v as usize + 1));
            prefetch(gr.edges.as_ptr().wrapping_add(a));
            h.put(nd as u32, [iv as u32, a as u32, b as u32]);
            let o = ld(&pg.pool, pg.ix(v, 1 - S)) & !SETTLED;
            if o != INF {
                m[0] = m[0].min(nd + o as u64);
            }
        });
    }
    h.min()
}

/// One-ended Dijkstra with u64 distances, for the few queries whose distances need more than
/// 31 bits (edges of weight near 2^32 at the special nodes). Speed does not matter here.
fn plain(g: &Graph, s: usize, t: usize) -> i64 {
    use std::cmp::Reverse;
    let mut dist = std::collections::HashMap::from([(s, 0u64)]);
    let mut heap = std::collections::BinaryHeap::from([Reverse((0u64, s))]);
    while let Some(Reverse((d, u))) = heap.pop() {
        if u == t {
            return d as i64;
        }
        if dist[&u] < d {
            continue; // stale entry
        }
        g.adj(u, |v, w| {
            let (v, nd) = (v as usize, d + w as u64);
            if dist.get(&v).map_or(true, |&o| nd < o) {
                dist.insert(v, nd);
                heap.push(Reverse((nd, v)));
            }
        });
    }
    -1
}

fn main() {
    // The scoring box backs anonymous memory with 2 MB pages. That helps the graph, but would round
    // every thread's stack and malloc arena up to 2 MB: so small stacks, and one arena (M_ARENA_MAX).
    std::env::set_var("RUST_MIN_STACK", "262144");
    unsafe { mallopt(-8, 1) };
    let t0 = std::time::Instant::now();
    let threads = std::env::var("FW_THREADS").ok().and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |p| p.get()));
    let q = map("/data/queries.txt");
    let (mut qs, mut j) = (Vec::new(), 0);
    while j < q.len() {
        qs.push((num(q, &mut j) as usize, num(q, &mut j) as usize));
    }

    let gt = map("/data/graph.txt");
    let mut i = 0;
    let n = num(gt, &mut i) as usize;
    num(gt, &mut i); // m: the edge lines are simply read to the end
    let body = &gt[i..];
    let vb = (usize::BITS - (n.max(2) - 1).leading_zeros()).max(1);
    let esc = (1u32 << (32 - vb)) - 1;
    let g = {
        let parts = chunks(body, threads * 4);
        let g0 = csr(n, threads, vb, esc, &parts, |&(lo, hi), emit| {
            let mut i = lo;
            while i < hi {
                let (u, v) = (num(body, &mut i) as usize, num(body, &mut i) as u32);
                emit(u, v, num(body, &mut i) as u32);
            }
        });
        let newid = bfs_order(&g0, threads);
        qs.iter_mut().for_each(|q| *q = (newid[q.0] as usize, newid[q.1] as usize));
        relabel(&g0, &newid, threads)
    };
    let parts = spans(n, threads * 4);
    let r = csr(n, threads, vb, esc, &parts, |&(lo, hi), emit| {
        for u in lo..hi {
            g.adj(u, |v, w| emit(v as usize, u as u32, w));
        }
    });
    let t1 = t0.elapsed();

    let answers: Vec<AtomicI64> = qs.iter().map(|_| AtomicI64::new(-1)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let (mut pg, mut hs) = (Pages::new(n), <[Dial; 2]>::default());
                loop {
                    let k = next.fetch_add(1, Relaxed);
                    let Some(&(s, t)) = qs.get(k) else { break };
                    let ans = bidir(&mut pg, &mut hs, &g, &r, s, t);
                    answers[k].store(ans.unwrap_or_else(|| plain(&g, s, t)), Relaxed);
                }
            });
        }
    });

    let mut out = Vec::with_capacity(qs.len() * 12);
    for a in &answers {
        out.extend_from_slice(a.load(Relaxed).to_string().as_bytes());
        out.push(b'\n');
    }
    std::fs::write("/out/distances.txt", out).expect("write distances.txt");
    eprintln!("build {t1:?} total {:?}", t0.elapsed());
}
