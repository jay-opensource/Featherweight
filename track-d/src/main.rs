// Track D, featherweight: every edge is 4 bytes and every node costs 1 byte plus 1/16 of a word.
// Nodes are relabelled in breadth-first order, so a search touches nearby memory. Each search
// carries a settled bitset and a small frontier map, not a distance array the size of the graph.
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicI64, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn prctl(option: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
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
        self.base[k >> self.sh] as usize + self.rel[k] as usize
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

    /// Pass 1: slots per node (1, or 2 when the weight needs an escape word).
    fn count(&self, threads: usize) -> Vec<AtomicU16> {
        let deg: Vec<AtomicU16> = (0..self.n + 1).map(|_| AtomicU16::new(0)).collect();
        let b = self.body;
        par(threads, &self.parts, |&(lo, hi)| {
            let mut i = lo;
            while i < hi {
                let u = num(b, &mut i) as usize;
                while b[i] != b' ' {
                    i += 1;
                }
                i += 1;
                let k = 1 + (num(b, &mut i) >= self.esc as u64) as u16;
                assert!(deg[u].fetch_add(k, Relaxed) < u16::MAX - 2, "out-degree above 65k");
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
        let out = Slots(edges.as_mut_ptr());
        let has_in: Vec<AtomicU64> = (0..self.n / 64 + 1).map(|_| AtomicU64::new(0)).collect();
        par(threads, &self.parts, |&(lo, hi)| {
            let out = &out;
            let mut i = lo;
            while i < hi {
                let (u, v) = (num(b, &mut i) as usize, num(b, &mut i) as usize);
                let w = num(b, &mut i);
                let big = w >= esc as u64;
                let k = 1 + big as u16;
                let p = base[u >> sh] as usize + rel[u] as usize + (deg[u].fetch_sub(k, Relaxed) - k) as usize;
                unsafe {
                    *out.0.add(p) = v as u32 | (w.min(esc as u64) as u32) << vb;
                    if big {
                        *out.0.add(p + 1) = w as u32;
                    }
                }
                let bit = 1u64 << (v & 63);
                if targets[v >> 6] & bit != 0 && v != u {
                    has_in[v >> 6].fetch_or(bit, Relaxed);
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
    let out = Slots(edges.as_mut_ptr());
    let mut has_in = vec![0u64; n / 64 + 1];
    par(threads, &ranges, |&(lo, hi)| {
        let out = &out;
        for u in lo..hi {
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

/// Parse and build the graph; with `renum`, relabel nodes in BFS order and copy the graph into
/// that order. Rewrites the ids in `qs` to match.
fn build(gtxt: &'static [u8], threads: usize, qs: &mut [(usize, usize)], renum: bool) -> Graph {
    let t = Instant::now();
    let txt = Text::new(gtxt, threads);
    let n = txt.n;
    let deg = txt.count(threads);
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
    eprintln!("build first {t1:?} bfs {:?} copy {:?} {}", t2 - t1, t.elapsed() - t2, mem());
    g
}

/// Monotone priority queue for Dijkstra: bucket i holds keys whose highest bit differing from the
/// last popped key is bit i-1. Pushing is O(1); a pop only re-sorts the smallest non-empty bucket.
struct Radix {
    b: Vec<Vec<(u64, u32)>>,
    occ: u64, // bit i-1 set: bucket i (1..=64) is non-empty
    last: u64,
}

impl Radix {
    fn new() -> Self {
        Radix { b: (0..65).map(|_| Vec::new()).collect(), occ: 0, last: 0 }
    }

    #[inline(always)]
    fn put(&mut self, d: u64, x: u32) {
        let i = (64 - (d ^ self.last).leading_zeros()) as usize;
        self.b[i].push((d, x));
        if i > 0 {
            self.occ |= 1 << (i - 1);
        }
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<(u64, u32)> {
        if self.b[0].is_empty() {
            if self.occ == 0 {
                return None;
            }
            let i = self.occ.trailing_zeros() as usize + 1;
            self.occ &= !(1 << (i - 1));
            let mut v = std::mem::take(&mut self.b[i]);
            self.last = v.iter().map(|e| e.0).min().unwrap();
            for &(d, x) in &v {
                self.put(d, x); // every element lands in a lower bucket
            }
            v.clear();
            self.b[i] = v;
        }
        self.b[0].pop()
    }

    fn clear(&mut self, mut f: impl FnMut(u32)) {
        for b in &mut self.b {
            b.iter().for_each(|e| f(e.1));
            b.clear();
        }
        self.occ = 0;
        self.last = 0;
    }
}

const EMPTY: u32 = u32::MAX;
static SETTLES: AtomicU64 = AtomicU64::new(0);

/// Tentative distances of unsettled nodes only: linear probing, backward-shift delete.
struct Frontier {
    keys: Vec<u32>,
    vals: Vec<u64>,
    mask: usize,
    len: usize,
}

impl Frontier {
    fn new(cap: usize) -> Self {
        Frontier { keys: vec![EMPTY; cap], vals: vec![0; cap], mask: cap - 1, len: 0 }
    }

    #[inline(always)]
    fn home(&self, k: u32) -> usize {
        ((k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize & self.mask
    }

    /// Lower k's distance to d; true if k is new or improved.
    #[inline(always)]
    fn relax(&mut self, k: u32, d: u64) -> bool {
        let mut i = self.home(k);
        loop {
            let key = self.keys[i];
            if key == k {
                if self.vals[i] <= d {
                    return false;
                }
                self.vals[i] = d;
                return true;
            }
            if key == EMPTY {
                self.keys[i] = k;
                self.vals[i] = d;
                self.len += 1;
                if self.len * 2 > self.keys.len() {
                    self.grow();
                }
                return true;
            }
            i = (i + 1) & self.mask;
        }
    }

    fn remove(&mut self, k: u32) {
        let mut i = self.home(k);
        while self.keys[i] != k {
            if self.keys[i] == EMPTY {
                return;
            }
            i = (i + 1) & self.mask;
        }
        self.len -= 1;
        let mut j = i;
        loop {
            j = (j + 1) & self.mask;
            let key = self.keys[j];
            if key == EMPTY {
                break;
            }
            let h = self.home(key);
            let stays = if i <= j { i < h && h <= j } else { i < h || h <= j };
            if !stays {
                self.keys[i] = key;
                self.vals[i] = self.vals[j];
                i = j;
            }
        }
        self.keys[i] = EMPTY;
    }

    fn grow(&mut self) {
        let old = std::mem::replace(self, Frontier::new(self.keys.len() * 2));
        for (k, v) in old.keys.into_iter().zip(old.vals) {
            if k != EMPTY {
                self.relax(k, v);
            }
        }
    }
}

/// Per-thread search state: settled bitset with a one-bit-per-word dirty summary, frontier, queue.
struct Search {
    done: Vec<u64>,
    dirty: Vec<u64>,
    front: Frontier,
    heap: Radix,
}

impl Search {
    fn new(n: usize) -> Self {
        let words = n / 64 + 1;
        Search { done: vec![0; words], dirty: vec![0; words / 64 + 1], front: Frontier::new(8192), heap: Radix::new() }
    }

    fn run(&mut self, g: &Graph, s: usize, t: usize) -> i64 {
        let mut res = -1;
        let mut settled = 0u64;
        self.front.relax(s as u32, 0);
        self.heap.put(0, s as u32);
        while let Some((d, u)) = self.heap.pop() {
            let (w, bit) = (u as usize >> 6, 1u64 << (u & 63));
            if self.done[w] & bit != 0 {
                continue; // stale entry
            }
            if self.done[w] == 0 {
                self.dirty[w >> 6] |= 1 << (w & 63);
            }
            self.done[w] |= bit;
            settled += 1;
            self.front.remove(u);
            if u as usize == t {
                res = d as i64;
                break;
            }
            let (mut e, end) = (g.off(u as usize), g.off(u as usize + 1));
            while e < end {
                let x = g.edges[e];
                let v = x & ((1 << g.vb) - 1);
                let mut wt = x >> g.vb;
                e += 1;
                if wt == g.esc {
                    wt = g.edges[e];
                    e += 1;
                }
                if self.done[v as usize >> 6] & (1 << (v & 63)) != 0 {
                    continue;
                }
                let nd = d + wt as u64;
                if self.front.relax(v, nd) {
                    self.heap.put(nd, v);
                }
            }
        }
        SETTLES.fetch_add(settled, Relaxed);
        let front = &mut self.front;
        self.heap.clear(|x| front.remove(x));
        for i in 0..self.dirty.len() {
            let mut m = std::mem::take(&mut self.dirty[i]);
            while m != 0 {
                self.done[i * 64 + m.trailing_zeros() as usize] = 0;
                m &= m - 1;
            }
        }
        res
    }
}

fn main() {
    let t0 = Instant::now();
    let env = |k: &str| std::env::var(k).ok();
    if env("FW_THP").as_deref() != Some("1") {
        // Transparent huge pages round sparse allocations (per-thread lists, malloc arenas) up to
        // 2 MB, and memory.peak charges the whole page. PR_SET_THP_DISABLE = 41.
        unsafe { prctl(41, 1, 0, 0, 0) };
    }
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
    let t_build = t0.elapsed();

    let answers: Vec<AtomicI64> = qs.iter().map(|_| AtomicI64::new(-1)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let mut st = Search::new(n);
                loop {
                    let k = next.fetch_add(1, Relaxed);
                    let Some(&(s, t)) = qs.get(k) else { break };
                    let ans = if s == t {
                        0
                    } else if g.off(s) == g.off(s + 1) || g.has_in[t >> 6] & (1 << (t & 63)) == 0 {
                        -1 // no way out of s, or no way into t
                    } else {
                        st.run(&g, s, t)
                    };
                    answers[k].store(ans, Relaxed);
                }
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
    eprintln!("phase build {t_build:?} queries {:?} total {:?} settles {} {}", t_query - t_build, t0.elapsed(),
        SETTLES.load(Relaxed), mem());
}
