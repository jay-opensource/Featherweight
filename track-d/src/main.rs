// Track D, featherweight: every edge is 4 bytes and every node costs 1 byte plus 1/16 of a word.
// Nodes are relabelled in breadth-first order, so a search touches nearby memory. Each search
// carries a settled bitset and a small frontier map, not a distance array the size of the graph.
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicI64, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

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

#[inline(always)]
fn num(b: &[u8], i: &mut usize) -> u64 {
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

/// Diagnostics: process RSS high-water mark and the container's memory.current / memory.peak (MB).
fn mem() -> String {
    let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let hwm = st.lines().find(|l| l.starts_with("VmHWM")).unwrap_or("").replace(' ', "");
    let rd = |f: &str| std::fs::read_to_string(f).ok().and_then(|x| x.trim().parse::<f64>().ok()).unwrap_or(0.0) / 1e6;
    format!("[{hwm} cur {:.1} peak {:.1}]", rd("/sys/fs/cgroup/memory.current"), rd("/sys/fs/cgroup/memory.peak"))
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

/// Shared mutable array written at disjoint indices by the fill threads.
struct Slots(*mut u32);
unsafe impl Sync for Slots {}

/// Compact CSR. Node k's slots start at base[k >> sh] + rel[k]; an edge slot is `v | w << vb`,
/// and w == esc means the next slot holds the full 32-bit weight.
struct Graph {
    edges: Vec<u32>,
    base: Vec<u32>,
    rel: Vec<u8>,
    sh: u32,
    vb: u32,
    esc: u32,
    has_in: Vec<u64>, // valid for query targets only
}

impl Graph {
    #[inline(always)]
    fn off(&self, k: usize) -> usize {
        unsafe { *self.base.get_unchecked(k >> self.sh) as usize + *self.rel.get_unchecked(k) as usize }
    }
}

/// Offsets for nodes 0..n with `slots(k)` slots each: the largest block (16 nodes on these graphs)
/// whose in-block offsets fit a u8. Returns (base per block + sentinel, rel per node + sentinel, shift).
fn layout(n: usize, slots: impl Fn(usize) -> u32) -> (Vec<u32>, Vec<u8>, u32) {
    let mut sh = 4u32;
    loop {
        let bs = 1usize << sh;
        let mut rel = vec![0u8; n + 1];
        let mut base = Vec::with_capacity((n >> sh) + 2);
        let (mut total, mut ok) = (0u32, true);
        for b0 in (0..=n).step_by(bs) {
            base.push(total);
            let mut acc = 0u32;
            for k in b0..(b0 + bs).min(n + 1) {
                ok &= acc <= 255;
                rel[k] = acc as u8;
                acc += if k < n { slots(k) } else { 0 };
            }
            total += acc;
        }
        base.push(total);
        if ok || sh == 0 {
            huge(&mut rel);
            return (base, rel, sh);
        }
        sh -= 1;
    }
}

/// The edge lines of graph.txt, split for parallel parsing.
struct Text {
    body: &'static [u8],
    parts: Vec<(usize, usize)>,
    n: usize,
    vb: u32,
    esc: u32,
}

impl Text {
    fn new(g: &'static [u8], threads: usize) -> Text {
        let mut i = 0;
        let n = num(g, &mut i) as usize;
        let _m = num(g, &mut i);
        let vb = (usize::BITS - (n.max(2) - 1).leading_zeros()).max(1);
        Text { body: &g[i..], parts: chunks(&g[i..], threads * 4), n, vb, esc: (1u32 << (32 - vb)) - 1 }
    }

    /// Pass 1: slots per node (1, or 2 when the weight needs an escape word). Lines go in batches
    /// of 16: a locked add cannot overlap its cache miss with the next one, so the 16 counters are
    /// prefetched first and their misses overlap.
    fn count(&self, threads: usize) -> Vec<AtomicU16> {
        let deg: Vec<AtomicU16> = (0..self.n + 1).map(|_| AtomicU16::new(0)).collect();
        let b = self.body;
        par(threads, &self.parts, |&(lo, hi)| {
            let mut i = lo;
            let mut batch = [(0usize, 0u16); 16];
            while i < hi {
                let mut k = 0;
                while k < 16 && i < hi {
                    let u = num(b, &mut i) as usize;
                    while b[i] != b' ' {
                        i += 1;
                    }
                    i += 1;
                    batch[k] = (u, 1 + (num(b, &mut i) >= self.esc as u64) as u16);
                    prefetch(&deg[u]);
                    k += 1;
                }
                for &(u, c) in &batch[..k] {
                    assert!(deg[u].fetch_add(c, Relaxed) < u16::MAX - 2, "out-degree above 65k");
                }
            }
        });
        deg
    }

    /// Pass 2: write every edge through its node's countdown cursor, so threads write disjoint
    /// slots. In-edges are recorded for nodes marked in `targets` only.
    fn fill(&self, threads: usize, deg: &[AtomicU16], lay: &(Vec<u32>, Vec<u8>, u32), targets: &[u64]) -> Graph {
        let (base, rel, sh) = lay;
        let (b, vb, esc) = (self.body, self.vb, self.esc);
        let mut edges = vec![0u32; *base.last().unwrap() as usize];
        huge(&mut edges);
        let out = Slots(edges.as_mut_ptr());
        let has_in: Vec<AtomicU64> = (0..self.n / 64 + 1).map(|_| AtomicU64::new(0)).collect();
        par(threads, &self.parts, |&(lo, hi)| {
            let out = &out;
            let mut i = lo;
            let mut batch = [(0usize, 0usize, 0u64); 16];
            while i < hi {
                // A locked op waits for every earlier store to land, so take the 16 cursors first
                // and only then issue the 16 scattered stores, which overlap their misses.
                let mut k = 0;
                while k < 16 && i < hi {
                    let (u, v) = (num(b, &mut i) as usize, num(b, &mut i) as usize);
                    batch[k] = (u, v, num(b, &mut i));
                    prefetch(&deg[u]);
                    k += 1;
                }
                for e in &mut batch[..k] {
                    let (u, big) = (e.0, e.2 >= esc as u64);
                    let c = 1 + big as u16;
                    e.0 = base[u >> sh] as usize + rel[u] as usize + (deg[u].fetch_sub(c, Relaxed) - c) as usize;
                    if e.1 == u {
                        e.1 |= 1 << 63; // self-loop: not an in-edge for the precheck
                    }
                }
                for &(p, v, w) in &batch[..k] {
                    unsafe {
                        *out.0.add(p) = v as u32 | (w.min(esc as u64) as u32) << vb;
                        if w >= esc as u64 {
                            *out.0.add(p + 1) = w as u32;
                        }
                    }
                    let bit = 1u64 << (v & 63);
                    if v >> 63 == 0 && targets[v >> 6] & bit != 0 {
                        has_in[v >> 6].fetch_or(bit, Relaxed);
                    }
                }
            }
        });
        let has_in = has_in.into_iter().map(|a| a.into_inner()).collect();
        Graph { edges, base: base.clone(), rel: rel.clone(), sh: *sh, vb, esc, has_in }
    }
}

/// Breadth-first relabelling, in parallel: each thread grows its own region from a seed in its
/// slice of the id range (claiming nodes with a CAS) and re-seeds from that slice when its region
/// is enclosed. New ids follow thread 0's visit order, then thread 1's, and so on.
fn bfs_order(g: &Graph, threads: usize) -> Vec<u32> {
    const FREE: u32 = u32::MAX;
    let n = g.rel.len() - 1;
    let vmask = (1u32 << g.vb) - 1;
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
                        let u = list[head] as usize;
                        head += 1;
                        let (mut e, end_e) = (g.off(u), g.off(u + 1));
                        while e < end_e {
                            let x = g.edges[e];
                            e += 1 + ((x >> g.vb) == g.esc) as usize;
                            if claim((x & vmask) as usize) {
                                list.push(x & vmask);
                            }
                        }
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

/// Copy `g` into the order given by `newid` (old id -> new id). Node k's slots are copied in
/// parallel, with each target rewritten to its new id.
fn relabel(g: &Graph, newid: &[u32], threads: usize) -> Graph {
    let n = newid.len();
    let vmask = (1u32 << g.vb) - 1;
    let deg: Vec<AtomicU16> = (0..n).map(|_| AtomicU16::new(0)).collect();
    let ranges: Vec<(usize, usize)> = (0..threads * 4).map(|j| (j * n / (threads * 4), (j + 1) * n / (threads * 4))).collect();
    par(threads, &ranges, |&(lo, hi)| {
        for u in lo..hi {
            deg[newid[u] as usize].store((g.off(u + 1) - g.off(u)) as u16, Relaxed);
        }
    });
    let (base, rel, sh) = layout(n, |k| deg[k].load(Relaxed) as u32);
    drop(deg);
    let mut edges = vec![0u32; *base.last().unwrap() as usize];
    huge(&mut edges);
    let out = Slots(edges.as_mut_ptr());
    let mut has_in = vec![0u64; n / 64 + 1];
    par(threads, &ranges, |&(lo, hi)| {
        let out = &out;
        for u in lo..hi {
            // Prefetch the new ids of the targets of the node 8 ahead, and where it will go.
            if u + 8 < hi {
                let w = newid[u + 8] as usize;
                prefetch(&rel[w]);
                prefetch(&base[w >> sh]);
                for e in g.off(u + 8)..g.off(u + 9) {
                    newid.get((g.edges[e] & vmask) as usize).map(prefetch); // escape words: harmless
                }
            }
            let k = newid[u] as usize;
            let mut p = base[k >> sh] as usize + rel[k] as usize;
            let (mut e, end) = (g.off(u), g.off(u + 1));
            while e < end {
                let x = g.edges[e];
                let big = (x >> g.vb) == g.esc;
                unsafe {
                    *out.0.add(p) = newid[(x & vmask) as usize] | (x & !vmask);
                    if big {
                        *out.0.add(p + 1) = g.edges[e + 1];
                    }
                }
                e += 1 + big as usize;
                p += 1 + big as usize;
            }
        }
    });
    for u in 0..n {
        if g.has_in[u >> 6] & (1 << (u & 63)) != 0 {
            let k = newid[u] as usize;
            has_in[k >> 6] |= 1 << (k & 63);
        }
    }
    Graph { edges, base, rel, sh, vb: g.vb, esc: g.esc, has_in }
}

/// Reverse graph: v's slots list (u, w) for every edge u -> v of `g`.
fn transpose(g: &Graph, threads: usize) -> Graph {
    let n = g.rel.len() - 1;
    let vmask = (1u32 << g.vb) - 1;
    let ranges: Vec<(usize, usize)> = (0..threads * 4).map(|j| (j * n / (threads * 4), (j + 1) * n / (threads * 4))).collect();
    // Calls f(u, e) for every edge slot e of every node u.
    let each = |f: &(dyn Fn(usize, usize) + Sync)| {
        par(threads, &ranges, |&(lo, hi)| {
            for u in lo..hi {
                let (mut e, end) = (g.off(u), g.off(u + 1));
                while e < end {
                    f(u, e);
                    e += 1 + ((g.edges[e] >> g.vb) == g.esc) as usize;
                }
            }
        })
    };
    let deg: Vec<AtomicU16> = (0..n + 1).map(|_| AtomicU16::new(0)).collect();
    each(&|_, e| {
        let x = g.edges[e];
        deg[(x & vmask) as usize].fetch_add(1 + ((x >> g.vb) == g.esc) as u16, Relaxed);
    });
    let (base, rel, sh) = layout(n, |k| deg[k].load(Relaxed) as u32);
    let mut edges = vec![0u32; *base.last().unwrap() as usize];
    huge(&mut edges);
    let out = Slots(edges.as_mut_ptr());
    let out = &out;
    each(&|u, e| {
        let x = g.edges[e];
        let v = (x & vmask) as usize;
        let big = (x >> g.vb) == g.esc;
        let k = 1 + big as u16;
        let p = base[v >> sh] as usize + rel[v] as usize + (deg[v].fetch_sub(k, Relaxed) - k) as usize;
        unsafe {
            *out.0.add(p) = u as u32 | (x & !vmask);
            if big {
                *out.0.add(p + 1) = g.edges[e + 1];
            }
        }
    });
    Graph { edges, base, rel, sh, vb: g.vb, esc: g.esc, has_in: Vec::new() }
}

/// Parse and build the graph; with `renum`, relabel nodes in BFS order and copy the graph into
/// that order. Rewrites the ids in `qs` to match.
fn build(gtxt: &'static [u8], threads: usize, qs: &mut [(usize, usize)], renum: bool) -> Graph {
    let t = Instant::now();
    let txt = Text::new(gtxt, threads);
    let n = txt.n;
    let deg = txt.count(threads);
    let tc = t.elapsed();
    let lay = layout(n, |k| deg[k].load(Relaxed) as u32);
    let mut targets = vec![0u64; n / 64 + 1];
    qs.iter().for_each(|&(_, q)| targets[q >> 6] |= 1 << (q & 63));
    let g = txt.fill(threads, &deg, &lay, &targets);
    drop((deg, lay));
    if !renum {
        return g;
    }
    let t1 = t.elapsed();
    let newid = bfs_order(&g, threads);
    let t2 = t.elapsed();
    let g = relabel(&g, &newid, threads);
    for q in qs.iter_mut() {
        *q = (newid[q.0] as usize, newid[q.1] as usize);
    }
    eprintln!("build count {tc:?} fill {:?} bfs {:?} copy {:?} {}", t1 - tc, t2 - t1, t.elapsed() - t2, mem());
    g
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
        self.occ = 0;
        self.last = 0;
        self.len = 0;
    }
}

static SETTLES: AtomicU64 = AtomicU64::new(0);
static STATE: AtomicU64 = AtomicU64::new(0);
static FALLBACKS: AtomicU64 = AtomicU64::new(0);

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
/// expanding the side with the smaller queue minimum. mu is the best s-t length seen through any
/// node labelled by both sides; stop when the two minimums together reach mu.
/// A label that would not fit 31 bits is skipped: every path through it is at least that long,
/// so the search stays exact for answers below 2^31. If something was skipped and no shorter
/// answer was found, return None and the caller runs the one-ended u64 search.
fn bidir(pg: &mut Pages, hs: &mut [Radix; 2], g: &Graph, r: &Graph, s: usize, t: usize) -> Option<i64> {
    let (mut mu, mut settled, mut skipped) = (u64::MAX, 0u64, false);
    let i = pg.at(s as u32, 0);
    pg.pool[i] = 0;
    hs[0].put(0, s as u32);
    let i = pg.at(t as u32, 1);
    pg.pool[i] = 0;
    hs[1].put(0, t as u32);
    loop {
        let (Some(kf), Some(kb)) = (hs[0].min(), hs[1].min()) else { break };
        if kf as u64 + kb as u64 >= mu {
            break;
        }
        let side = (hs[0].len > hs[1].len) as usize; // the side with fewer queued entries
        let gr = if side == 0 { g } else { r };
        let (d, u) = hs[side].pop().unwrap();
        let iu = pg.at(u, side);
        let lu = pg.pool[iu];
        if lu & SETTLED != 0 || lu < d {
            continue; // stale entry
        }
        pg.pool[iu] = lu | SETTLED;
        settled += 1;
        let (mut e, end) = (gr.off(u as usize), gr.off(u as usize + 1));
        while e < end {
            let x = unsafe { *gr.edges.get_unchecked(e) };
            let v = x & ((1 << gr.vb) - 1);
            let mut wt = x >> gr.vb;
            e += 1;
            if wt == gr.esc {
                wt = gr.edges[e];
                e += 1;
            }
            let nd = d as u64 + wt as u64;
            if nd >= INF as u64 {
                skipped = true;
                continue;
            }
            let iv = pg.at(v, side);
            let lv = unsafe { *pg.pool.get_unchecked(iv) };
            if lv & SETTLED != 0 || lv as u64 <= nd {
                continue;
            }
            unsafe { *pg.pool.get_unchecked_mut(iv) = nd as u32 };
            hs[side].put(nd as u32, v);
            let o = pg.get(v, 1 - side) & !SETTLED;
            if o != INF {
                mu = mu.min(nd + o as u64);
            }
        }
    }
    SETTLES.fetch_add(settled, Relaxed);
    pg.clear();
    hs.iter_mut().for_each(|h| h.clear());
    match mu {
        _ if skipped && mu >= INF as u64 => None,
        u64::MAX => Some(-1),
        _ => Some(mu as i64),
    }
}

/// One-ended Dijkstra with u64 distances, for the few queries whose distances need more than
/// 31 bits (edges of weight near 2^32 at the special nodes). Speed does not matter here.
fn plain(g: &Graph, s: usize, t: usize) -> i64 {
    FALLBACKS.fetch_add(1, Relaxed);
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
        let (mut e, end) = (g.off(u), g.off(u + 1));
        while e < end {
            let x = g.edges[e];
            let (v, mut w) = ((x & ((1 << g.vb) - 1)) as usize, x >> g.vb);
            e += 1;
            if w == g.esc {
                w = g.edges[e];
                e += 1;
            }
            let nd = d + w as u64;
            if dist.get(&v).map_or(true, |&o| nd < o) {
                dist.insert(v, nd);
                heap.push(Reverse((nd, v)));
            }
        }
    }
    -1
}

fn main() {
    let t0 = Instant::now();
    let env = |k: &str| std::env::var(k).ok();
    // The scoring box backs anonymous memory with 2 MB pages. That helps the graph, but would round
    // every thread's stack and malloc arena up to 2 MB: so small stacks, and one arena (M_ARENA_MAX).
    std::env::set_var("RUST_MIN_STACK", "262144");
    unsafe { mallopt(-8, 1) };
    let threads = env("FW_THREADS").and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |p| p.get()));
    let qtxt = map("/data/queries.txt");
    let mut qs = Vec::new();
    let mut j = 0;
    while j < qtxt.len() {
        qs.push((num(qtxt, &mut j) as usize, num(qtxt, &mut j) as usize));
    }
    let g = build(map("/data/graph.txt"), threads, &mut qs, env("FW_RENUM").map_or(true, |v| v != "0"));
    let n = g.rel.len() - 1;
    let tt = Instant::now();
    let r = transpose(&g, threads);
    let t_build = t0.elapsed();
    eprintln!("transpose {:?}", tt.elapsed());

    let answers: Vec<AtomicI64> = qs.iter().map(|_| AtomicI64::new(-1)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let (mut pg, mut hs) = (Pages::new(n), [Radix::new(), Radix::new()]);
                loop {
                    let k = next.fetch_add(1, Relaxed);
                    let Some(&(s, t)) = qs.get(k) else { break };
                    let ans = if s == t {
                        0
                    } else if g.off(s) == g.off(s + 1) || g.has_in[t >> 6] & (1 << (t & 63)) == 0 {
                        -1 // no way out of s, or no way into t
                    } else if let Some(d) = bidir(&mut pg, &mut hs, &g, &r, s, t) {
                        d
                    } else {
                        plain(&g, s, t)
                    };
                    answers[k].store(ans, Relaxed);
                }
                let heap: usize = hs.iter().flat_map(|h| h.b.iter()).map(|b| b.capacity() * 8).sum();
                STATE.fetch_add((pg.pool.capacity() * 4 + pg.dir.len() * 4) as u64, Relaxed);
                SETTLES.fetch_add((heap as u64) << 40, Relaxed);
            });
        }
    });
    let t_query = t0.elapsed();

    let mut out = Vec::with_capacity(qs.len() * 12);
    for a in &answers {
        out.extend_from_slice(a.load(Relaxed).to_string().as_bytes());
        out.push(b'\n');
    }
    std::fs::write("/out/distances.txt", out).expect("write distances.txt");
    eprintln!("phase build {t_build:?} queries {:?} total {:?} settles {} fallbacks {} state {:.1} MB {}", t_query - t_build, t0.elapsed(),
        SETTLES.load(Relaxed) & ((1 << 40) - 1), FALLBACKS.load(Relaxed), STATE.load(Relaxed) as f64 / 1e6, mem());
    eprintln!("heap buckets {:.1} MB", (SETTLES.load(Relaxed) >> 40) as f64 / 1e6);
}
