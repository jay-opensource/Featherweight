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
fn prefetch<T>(x: &T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(x as *const T as *const i8, std::arch::x86_64::_MM_HINT_T0)
    };
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
        unsafe { *self.base.get_unchecked(k >> self.sh) as usize + *self.rel.get_unchecked(k) as usize }
    }

    /// Call f(v, w) for every edge u -> v of weight w.
    #[inline(always)]
    fn adj(&self, u: usize, mut f: impl FnMut(u32, u32)) {
        let (mut e, end) = (self.off(u), self.off(u + 1));
        while e < end {
            let x = unsafe { *self.edges.get_unchecked(e) };
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
/// edges of one work item. It runs twice, to count each node's slots and then to fill them
/// through per-node countdown cursors, so threads write disjoint slots. The locked updates go in
/// batches of 16: a locked op waits for every earlier store, so one at a time the scattered stores
/// would not overlap their cache misses.
fn csr<T: Sync>(n: usize, threads: usize, vb: u32, esc: u32, items: &[T], each: impl Fn(&T, &mut dyn FnMut(usize, u32, u32)) + Sync) -> Graph {
    let deg: Vec<AtomicU16> = (0..n + 1).map(|_| AtomicU16::new(0)).collect();
    par(threads, items, |it| {
        let (mut b, mut k) = ([(0usize, 0u16); 16], 0);
        let add = |b: &[(usize, u16)]| {
            for &(u, c) in b {
                assert!(deg[u].fetch_add(c, Relaxed) < u16::MAX - 2, "out-degree above 65k");
            }
        };
        each(it, &mut |u, _, w| {
            prefetch(&deg[u]);
            b[k] = (u, 1 + (w >= esc) as u16);
            k += 1;
            if k == 16 {
                add(&b);
                k = 0;
            }
        });
        add(&b[..k]);
    });
    let (base, rel, sh) = layout(n, threads, |k| deg[k].load(Relaxed) as u32);
    let mut edges = vec![0u32; *base.last().unwrap() as usize];
    huge(&mut edges);
    let out = Slots(edges.as_mut_ptr());
    par(threads, items, |it| {
        let out = &out;
        let (mut b, mut k) = ([(0usize, 0u32, 0u32); 16], 0);
        let fill = |b: &mut [(usize, u32, u32)]| {
            for e in b.iter_mut() {
                let c = 1 + (e.2 >= esc) as u16;
                e.0 = base[e.0 >> sh] as usize + rel[e.0] as usize + (deg[e.0].fetch_sub(c, Relaxed) - c) as usize;
            }
            for &(p, v, w) in b.iter() {
                store(out, p, v, w, vb, esc);
            }
        };
        each(it, &mut |u, v, w| {
            prefetch(&deg[u]);
            b[k] = (u, v, w);
            k += 1;
            if k == 16 {
                fill(&mut b);
                k = 0;
            }
        });
        fill(&mut b[..k]);
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

/// Monotone priority queue for Dijkstra on 31-bit distances: bucket i holds entries whose key's
/// highest bit differing from the last popped key is bit i-1. Pushing is O(1); a pop only
/// re-sorts the smallest non-empty bucket. An entry is key << 32 | node, 8 bytes.
struct Radix {
    b: Vec<Vec<u64>>,
    occ: u64, // bit i-1 set: bucket i (1..=32) is non-empty
    last: u32,
    len: usize,
}

impl Radix {
    fn new() -> Self {
        Radix { b: (0..33).map(|_| Vec::new()).collect(), occ: 0, last: 0, len: 0 }
    }

    #[inline(always)]
    fn put(&mut self, d: u32, x: u32) {
        let i = (32 - (d ^ self.last).leading_zeros()) as usize;
        unsafe { self.b.get_unchecked_mut(i) }.push((d as u64) << 32 | x as u64);
        self.len += 1;
        if i > 0 {
            self.occ |= 1 << (i - 1);
        }
    }

    /// Smallest key, after moving it into bucket 0.
    #[inline(always)]
    fn min(&mut self) -> Option<u32> {
        if self.b[0].is_empty() {
            if self.occ == 0 {
                return None;
            }
            let i = self.occ.trailing_zeros() as usize + 1;
            self.occ &= !(1 << (i - 1));
            let mut v = std::mem::take(&mut self.b[i]);
            self.last = (v.iter().min().unwrap() >> 32) as u32;
            for &e in &v {
                self.put((e >> 32) as u32, e as u32); // every entry lands in a lower bucket
            }
            self.len -= v.len();
            v.clear();
            self.b[i] = v;
        }
        Some(self.last)
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<(u32, u32)> {
        self.min()?;
        self.len -= 1;
        self.b[0].pop().map(|e| ((e >> 32) as u32, e as u32))
    }

    fn clear(&mut self) {
        self.b.iter_mut().for_each(|b| b.clear());
        (self.occ, self.last, self.len) = (0, 0, 0);
    }
}

/// Distances in the two-ended search are 31 bits; the top bit marks "settled".
const SETTLED: u32 = 1 << 31;
const INF: u32 = SETTLED - 1;
const NOPAGE: u32 = u32::MAX;

/// Labels of the two-ended search, per side, in pages of 64 consecutive node ids (neighbours in
/// BFS order) allocated on first touch from a per-thread pool. The two balls grow around s and
/// around t, so each side gets its own pages. dir holds a page's pool offset, side 0 then side 1.
const PB: u32 = 6;

struct Pages {
    dir: Vec<u32>,
    half: usize,
    pool: Vec<u32>,
    top: usize,
    used: Vec<u32>,
}

impl Pages {
    fn new(n: usize) -> Self {
        let half = (n >> PB) + 1;
        Pages { dir: vec![NOPAGE; 2 * half], half, pool: Vec::new(), top: 0, used: Vec::new() }
    }

    /// Index of v's label on `side`, allocating its page.
    #[inline(always)]
    fn at(&mut self, v: u32, side: usize) -> usize {
        let d = side * self.half + (v >> PB) as usize;
        if unsafe { *self.dir.get_unchecked(d) } == NOPAGE {
            self.dir[d] = self.top as u32;
            self.top += 1 << PB;
            if self.pool.len() < self.top {
                self.pool.resize(self.top, INF);
            } else {
                self.pool[self.top - (1 << PB)..self.top].fill(INF);
            }
            self.used.push(d as u32);
        }
        unsafe { *self.dir.get_unchecked(d) as usize + (v & ((1 << PB) - 1)) as usize }
    }

    /// v's label on `side` (tentative or settled), INF if none.
    #[inline(always)]
    fn get(&self, v: u32, side: usize) -> u32 {
        match unsafe { *self.dir.get_unchecked(side * self.half + (v >> PB) as usize) } {
            NOPAGE => INF,
            p => unsafe { *self.pool.get_unchecked(p as usize + (v & ((1 << PB) - 1)) as usize) },
        }
    }

    fn clear(&mut self) {
        for &d in &self.used {
            self.dir[d as usize] = NOPAGE;
        }
        self.used.clear();
        self.top = 0;
    }
}

/// Two-ended Dijkstra: forward from s on `g`, backward from t on the reverse graph `r`, always
/// expanding the side with fewer queued entries. mu is the best s-t length seen through any node
/// labelled by both sides; stop when the two queue minimums together reach mu. A source with no
/// way out or a target with no way in empties its queue at once, so those answer -1 immediately.
/// A label that would not fit 31 bits is skipped: every path through it is at least that long,
/// so the search stays exact for shorter answers. If something was skipped and no shorter answer
/// was found, return None and the caller runs the one-ended u64 search.
fn bidir(pg: &mut Pages, hs: &mut [Radix; 2], g: &Graph, r: &Graph, s: usize, t: usize) -> Option<i64> {
    let (mut mu, mut skipped) = (u64::MAX, false);
    for (side, x) in [(0, s), (1, t)] {
        let i = pg.at(x as u32, side);
        pg.pool[i] = 0;
        hs[side].put(0, x as u32);
    }
    while let (Some(kf), Some(kb)) = (hs[0].min(), hs[1].min()) {
        if kf as u64 + kb as u64 >= mu {
            break;
        }
        let side = (hs[0].len > hs[1].len) as usize;
        let (d, u) = hs[side].pop().unwrap();
        let iu = pg.at(u, side);
        let lu = pg.pool[iu];
        if lu & SETTLED != 0 || lu < d {
            continue; // stale entry
        }
        pg.pool[iu] = lu | SETTLED;
        let (gr, h) = (if side == 0 { g } else { r }, &mut hs[side]);
        gr.adj(u as usize, |v, w| {
            let nd = d as u64 + w as u64;
            if nd >= INF as u64 {
                skipped = true;
                return;
            }
            let iv = pg.at(v, side);
            let lv = unsafe { *pg.pool.get_unchecked(iv) };
            if lv & SETTLED != 0 || lv as u64 <= nd {
                return;
            }
            unsafe { *pg.pool.get_unchecked_mut(iv) = nd as u32 };
            h.put(nd as u32, v);
            let o = pg.get(v, 1 - side) & !SETTLED;
            if o != INF {
                mu = mu.min(nd + o as u64);
            }
        });
    }
    pg.clear();
    hs.iter_mut().for_each(Radix::clear);
    match mu {
        _ if skipped && mu >= INF as u64 => None,
        u64::MAX => Some(-1),
        _ => Some(mu as i64),
    }
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
        let g0 = csr(n, threads, vb, esc, &chunks(body, threads * 4), |&(lo, hi), emit| {
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
    let r = csr(n, threads, vb, esc, &spans(n, threads * 4), |&(lo, hi), emit| {
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
                let (mut pg, mut hs) = (Pages::new(n), [Radix::new(), Radix::new()]);
                loop {
                    let k = next.fetch_add(1, Relaxed);
                    let Some(&(s, t)) = qs.get(k) else { break };
                    let ans = if s == t { 0 } else { bidir(&mut pg, &mut hs, &g, &r, s, t).unwrap_or_else(|| plain(&g, s, t)) };
                    answers[k].store(ans, Relaxed);
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
