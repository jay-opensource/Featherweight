// Track D, featherweight. Every edge is 4 bytes and every node 1 byte plus 1/16 of a word.
// Nodes are relabelled in breadth-first order, so a search touches nearby memory, and each query
// runs as a two-ended Dijkstra whose labels live in small per-thread pages, not in arrays the
// size of the graph. See PERF.md for the measurements behind each choice.
use std::{fs::File, os::unix::io::AsRawFd};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering::Relaxed};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn prctl(option: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

/// Read-only private mapping of a whole file; the page cache is not charged to us, a heap copy would be.
fn map(path: &str) -> &'static [u8] {
    let f = File::open(path).expect(path);
    let len = f.metadata().unwrap().len() as usize; // an empty file still maps a page, never read
    let p = unsafe { mmap(std::ptr::null_mut(), len.max(1), 1, 2, f.as_raw_fd(), 0) };
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
            let v = d.checked_shl(64 - 8 * len as u32).unwrap_or(0) & 0x0F0F_0F0F_0F0F_0F0F;
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
fn prefetch<T>(x: *const T) {
    unsafe { std::arch::x86_64::_mm_prefetch(x as *const i8, std::arch::x86_64::_MM_HINT_T0) };
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

/// `0..n` in `k` ranges, each with the tag `x`.
fn spans<T: Copy>(x: T, n: usize, k: usize) -> Vec<(T, usize, usize)> {
    (0..k).map(|j| (x, j * n / k, (j + 1) * n / k)).collect()
}

/// Run `f` on `threads` threads, each pulling work items from `items` until they run out.
fn par<T: Sync>(threads: usize, items: &[T], f: impl Fn(&T) + Sync) {
    let next = AtomicUsize::new(0);
    let work = || {
        while let Some(it) = items.get(next.fetch_add(1, Relaxed)) {
            f(it)
        }
    };
    std::thread::scope(|s| (0..threads).for_each(|_| _ = s.spawn(work)));
}

/// Shared mutable array written at disjoint indices by several threads.
struct Slots<T>(*mut T);
unsafe impl<T> Sync for Slots<T> {}

/// Write edge (v, w) at slot p: one slot `v | w << vb`, or, when w does not fit, the escape
/// value in that slot and the full weight in the next. Returns the slots used.
fn store(out: &Slots<u32>, p: usize, v: u32, w: u32, vb: u32, esc: u32) -> usize {
    let k = 1 + (w >= esc) as usize; // the weight goes first, so that without an escape it is overwritten
    unsafe { (*out.0.add(p + k - 1), *out.0.add(p)) = (w, v | w.min(esc) << vb) };
    k
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

/// Offsets for nodes 0..n with `slots(k)` slots each (k = n too, the sentinel): the largest block
/// (16 nodes on these graphs) whose in-block offsets fit a u8. Blocks are independent, so threads
/// fill them; only the u32 block bases are a serial prefix sum. Returns the graph without edges.
fn layout(n: usize, t: usize, vb: u32, esc: u32, slots: impl Fn(usize) -> u32 + Sync) -> Graph {
    for sh in (0..=4).rev() {
        let (mut rel, mut base) = (vec![0u8; n + 1], vec![0u32; (n >> sh) + 2]);
        let (rp, bp) = (Slots(rel.as_mut_ptr()), Slots(base.as_mut_ptr()));
        let ok = AtomicBool::new(true);
        par(t, &spans((), (n >> sh) + 1, t * 4), |&(_, lo, hi)| {
            for b in lo..hi {
                let mut acc = 0;
                for k in b << sh..((b + 1) << sh).min(n + 1) {
                    (acc > 255).then(|| ok.store(false, Relaxed));
                    unsafe { *(&rp).0.add(k) = acc as u8 };
                    acc += slots(k);
                }
                unsafe { *(&bp).0.add(b + 1) = acc };
            }
        });
        if ok.into_inner() || sh == 0 {
            (1..base.len()).for_each(|b| base[b] += base[b - 1]);
            return Graph { edges: vec![], base, rel, sh, vb, esc };
        }
    }
    unreachable!()
}

/// A work item of an edge stream: `each(emit)` calls emit(u, v, w) for each of its edges. A trait
/// (not a closure taking a `dyn` emitter) so that the emitter is inlined into the parsing loop.
trait Edges: Sync {
    fn each(&self, emit: impl FnMut(usize, u32, u32));
}

/// The lines of the graph text that start at a byte in lo..hi (a line that starts before lo and
/// ends inside it belongs to the item before).
impl Edges for (&[u8], usize, usize) {
    fn each(&self, mut emit: impl FnMut(usize, u32, u32)) {
        let (b, lo, hi) = *self;
        let mut i = (lo..hi).find(|&p| p == 0 || b[p - 1] == 10).unwrap_or(hi); // 10: LF
        while i < hi {
            let (u, v) = (num(b, &mut i) as usize, num(b, &mut i) as u32);
            emit(u, v, num(b, &mut i) as u32);
        }
    }
}

/// The edges u -> v of nodes lo..hi of a graph, as v -> u.
impl Edges for (&Graph, usize, usize) {
    fn each(&self, mut emit: impl FnMut(usize, u32, u32)) {
        for u in self.1..self.2 {
            self.0.adj(u, |v, w| emit(v as usize, u as u32, w));
        }
    }
}

/// Build a CSR over n nodes from the edges of `items`, read twice. Every random access stays
/// inside a small region:
/// 1. count each item's slots per bucket of 8192 nodes (213 counters per item, no atomics);
/// 2. append each slot to its bucket's region through the item's own cursor, with the node's
///    place inside the bucket in a parallel u16 array (213 sequential streams per thread; both
///    slots of an escaped edge get it, so the slots can be counted and moved one by one);
/// 3. per bucket, count each node's slots from the u16s alone; once the layout is known, reorder
///    the bucket's ~94 KB of slots by node through a scratch copy.

/// Fault in a fresh zeroed array one contiguous slice per thread before random writers use it:
/// first touches at random addresses from many threads queue on the same page-table locks.
fn touch<T>(mut v: Vec<T>, threads: usize) -> Vec<T> {
    let (p, pages) = (Slots(v.as_mut_ptr() as *mut u8), v.len() * std::mem::size_of::<T>() >> 12);
    par(threads, &spans((), pages, threads * 4), |&(_, lo, hi)| {
        let p = &p;
        (lo..hi).for_each(|k| unsafe { p.0.add(k << 12).write_volatile(0) });
    });
    v
}

/// Nothing is locked, and no store lands outside the bucket being worked on.
fn csr(n: usize, threads: usize, vb: u32, esc: u32, items: &[impl Edges]) -> Graph {
    const BS: u32 = 13;
    let (nb, ni) = ((n >> BS) + 1, items.len());
    let mut cur = vec![0u32; ni * nb];
    let cp = Slots(cur.as_mut_ptr());
    par(threads, &(0..ni).collect::<Vec<_>>(), |&i| {
        let c = unsafe { std::slice::from_raw_parts_mut((&cp).0.add(i * nb), nb) };
        items[i].each(|u, _, w| c[u >> BS] += 1 + (w >= esc) as u32);
    });
    let (mut start, mut acc) = (vec![0usize; nb + 1], 0u32);
    for b in 0..nb {
        for i in 0..ni {
            (cur[i * nb + b], acc) = (acc, acc + cur[i * nb + b]);
        }
        start[b + 1] = acc as usize;
    }
    let (mut edges, mut loc) = (touch(vec![0u32; acc as usize], threads), touch(vec![0u16; acc as usize], threads));
    let (ep, lp) = (Slots(edges.as_mut_ptr()), Slots(loc.as_mut_ptr()));
    par(threads, &(0..ni).collect::<Vec<_>>(), |&i| {
        let c = unsafe { std::slice::from_raw_parts_mut((&cp).0.add(i * nb), nb) };
        items[i].each(|u, v, w| {
            let (p, l) = (c[u >> BS] as usize, (u & ((1 << BS) - 1)) as u16);
            let k = store(&ep, p, v, w, vb, esc);
            c[u >> BS] += k as u32;
            unsafe { (*(&lp).0.add(p), *(&lp).0.add(p + k - 1)) = (l, l) };
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
    let lay = layout(n, threads, vb, esc, |k| deg[k] as u32);
    par(threads, &buckets, |&b| {
        let (lo, hi) = (start[b], start[b + 1]);
        let e = unsafe { std::slice::from_raw_parts((&ep).0.add(lo), hi - lo) }.to_vec();
        let mut at = Vec::from_iter((b << BS..((b + 1) << BS).min(n + 1)).map(|k| lay.off(k)));
        for (&x, &l) in e.iter().zip(&loc[lo..hi]) {
            unsafe { *(&ep).0.add(at[l as usize]) = x };
            at[l as usize] += 1;
        }
    });
    Graph { edges, ..lay }
}

/// Breadth-first relabelling, in parallel: each thread grows its own region from a seed in its
/// slice of the id range (claiming nodes with a CAS) and re-seeds from that slice when its region
/// is enclosed. New ids follow one region's visit order, then the next region's, and so on.
fn bfs_order(g: &Graph, threads: usize) -> Vec<u32> {
    let n = g.rel.len() - 1;
    // Zeroed by the kernel on first touch (in parallel); 0 is free, 1 claimed, until the ids.
    let mut id = touch(vec![0u32; n], threads);
    let ip = Slots(id.as_mut_ptr());
    let at = |v: usize| unsafe { AtomicU32::from_ptr((&ip).0.add(v)) };
    let claim = |v| at(v).load(Relaxed) == 0 && at(v).compare_exchange(0, 1, Relaxed, Relaxed).is_ok();
    let lists = std::sync::Mutex::new(Vec::new());
    par(threads, &spans((), n, threads), |&(_, mut scan, end)| {
        let (mut list, mut head) = (Vec::with_capacity(n), 0); // only touched pages are charged
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
            // Software pipeline over the queue: offsets 16 nodes ahead, edge lists 8 ahead, the
            // claim words of the targets 4 ahead, so the misses of upcoming nodes overlap.
            if let Some(&w) = list.get(head + 16) {
                prefetch(&g.rel[w as usize]);
                prefetch(&g.base[w as usize >> g.sh]);
            }
            if let Some(&w) = list.get(head + 8) {
                prefetch(g.edges.as_ptr().wrapping_add(g.off(w as usize)));
            }
            if let Some(&w) = list.get(head + 4) {
                g.adj(w as usize, |v, _| prefetch(at(v as usize)));
            }
            let u = list[head] as usize;
            head += 1;
            g.adj(u, |v, _| _ = claim(v as usize).then(|| list.push(v)));
        }
        lists.lock().unwrap().push(list);
    });
    let lists = lists.into_inner().unwrap();
    par(threads, &(0..lists.len()).collect::<Vec<_>>(), |&j| {
        let start = lists[..j].iter().map(|l| l.len()).sum::<usize>();
        for (i, &v) in lists[j].iter().enumerate() {
            at(v as usize).store((start + i) as u32, Relaxed);
        }
    });
    id
}

/// Copy `g` into the order given by `id` (old id -> new id), each node's slots by one thread,
/// with every target rewritten to its new id.
fn relabel(g: &Graph, id: &[u32], threads: usize) -> Graph {
    let (n, ranges) = (id.len(), spans((), id.len(), threads * 4));
    let mut deg = vec![0u16; n + 1]; // id is a permutation: no two threads write one entry
    let dp = Slots(deg.as_mut_ptr());
    par(threads, &ranges, |&(_, lo, hi)| {
        for u in lo..hi {
            unsafe { *(&dp).0.add(id[u] as usize) = (g.off(u + 1) - g.off(u)) as u16 };
        }
    });
    let lay = layout(n, threads, g.vb, g.esc, |k| deg[k] as u32);
    drop(deg);
    let mut edges = touch(vec![0u32; g.edges.len()], threads);
    let ep = Slots(edges.as_mut_ptr());
    par(threads, &ranges, |&(_, lo, hi)| {
        for u in lo..hi {
            if u + 8 < hi {
                // Prefetch where node u+8 goes and the new ids of its targets.
                let w = id[u + 8] as usize;
                prefetch(&lay.rel[w]);
                prefetch(&lay.base[w >> lay.sh]);
                g.adj(u + 8, |v, _| prefetch(&id[v as usize]));
            }
            let mut p = lay.off(id[u] as usize);
            g.adj(u, |v, w| p += store(&ep, p, id[v as usize], w, g.vb, g.esc));
        }
    });
    Graph { edges, ..lay }
}

/// Buckets per queue: the search relaxes only edges lighter than W (see `bidir`).
const W: usize = 1 << 13;

/// Monotone bucket queue (Dial): one bucket per key modulo W. Each key pushed is the popped key
/// cur plus an edge lighter than W, so every queued key lies in [cur, cur + W) and a bucket's
/// position gives its key; the next key is found by stepping cur over empty buckets (the frontier
/// is dense: a few steps). A bucket is a list through a pool of entries (an item and the next
/// entry's index) that reuses popped entries. The item holds all that settling the node needs, so
/// a pop starts on the label and the edges at once. Entry 0 is a sentinel: index 0 means "none" in
/// `head`, in a next field and in `free`.
#[derive(Default)]
struct Dial {
    head: Vec<u32>,
    pool: Vec<[u32; 4]>,
    free: u32,
    cur: u32,
    len: usize,
}

impl Dial {
    /// Queue a node with key d in [cur, cur + W): its label's index in the pages, then its first
    /// and end edge slot.
    #[inline(always)]
    fn put(&mut self, d: u32, [i, a, e]: [u32; 3]) {
        if self.free == 0 {
            self.free = self.pool.len() as u32;
            self.pool.push([0; 4]);
        }
        let (k, b) = (self.free as usize, d as usize % W);
        self.free = ld(&self.pool, k)[3];
        *mt(&mut self.pool, k) = [i, a, e, ld(&self.head, b)];
        *mt(&mut self.head, b) = k as u32;
        self.len += 1;
    }

    /// Move cur to the smallest key; false if the queue is empty.
    #[inline(always)]
    fn min(&mut self) -> bool {
        while self.len > 0 && ld(&self.head, self.cur as usize % W) == 0 {
            self.cur += 1;
        }
        self.len > 0
    }

    /// Take an item of key cur; min() must have returned true since the last pop.
    #[inline(always)]
    fn pop(&mut self) -> [u32; 3] {
        let b = self.cur as usize % W;
        let k = ld(&self.head, b) as usize;
        let [i, a, e, next] = ld(&self.pool, k);
        *mt(&mut self.head, b) = next;
        mt(&mut self.pool, k)[3] = std::mem::replace(&mut self.free, k as u32);
        self.len -= 1;
        [i, a, e]
    }

    /// Empty the queue (the first time, size it).
    fn clear(&mut self) {
        (self.free, self.cur, self.len) = (0, 0, 0);
        self.head = vec![0; W];
        self.pool.resize(1, [0; 4]);
    }
}

/// Distances in the two-ended search are 31 bits; the top bit marks "settled".
const SETTLED: u32 = 1 << 31;
const INF: u32 = SETTLED - 1;

/// Labels of the two-ended search, per side, in pages of 2^pb (64 on these graphs) consecutive
/// node ids (neighbours in BFS order) allocated on first touch from a per-thread pool. The two
/// balls grow around s and around t, so each side gets its own pages. dir holds a page's number
/// in the pool as a u16 (half the cache footprint of an offset), side 0 then side 1; pb grows if
/// n ever needs more numbers. Page 0 is all INF and stands for every page not allocated yet, so
/// reading a label never branches.
struct Pages {
    dir: Vec<u16>,
    pb: u32,
    pool: Vec<u32>,
    own: Vec<u32>, // the dir entries of the pages in use
}

impl Pages {
    fn new(n: usize) -> Self {
        let pb = 6.max(usize::BITS - (n / 32766).leading_zeros());
        let (dir, pool, own) = (vec![0; 2 * ((n >> pb) + 1)], vec![INF; 1 << pb], Vec::new());
        Pages { dir, pb, pool, own }
    }

    /// Index of v's label on `side`, allocating its page.
    #[inline(always)]
    fn at(&mut self, v: u32, side: usize) -> usize {
        let d = side * self.dir.len() / 2 + (v >> self.pb) as usize;
        if ld(&self.dir, d) == 0 {
            self.dir[d] = (self.pool.len() >> self.pb) as u16;
            self.pool.resize(self.pool.len() + (1 << self.pb), INF);
            self.own.push(d as u32);
        }
        self.ix(v, side)
    }

    /// Index of v's label on `side`, in page 0 if v has none.
    #[inline(always)]
    fn ix(&self, v: u32, side: usize) -> usize {
        let d = side * self.dir.len() / 2 + (v >> self.pb) as usize;
        (ld(&self.dir, d) as usize) << self.pb | v as usize & ((1 << self.pb) - 1)
    }

    fn clear(&mut self) {
        self.own.drain(..).for_each(|d| self.dir[d as usize] = 0);
        self.pool.truncate(1 << self.pb);
    }
}

/// Two-ended Dijkstra: forward from s on g[0], backward from t on the reverse graph g[1], always
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
fn bidir(pg: &mut Pages, hs: &mut [Dial; 2], g: [&Graph; 2], s: usize, t: usize) -> Option<i64> {
    pg.clear();
    let mut m = [if s == t { 0 } else { u64::MAX }, u64::MAX]; // mu, skip
    for (side, x, gr) in [(0, s, g[0]), (1, t, g[1])] {
        let i = pg.at(x as u32, side);
        pg.pool[i] = 0;
        hs[side].clear();
        hs[side].put(0, [i as u32, gr.off(x) as u32, gr.off(x + 1) as u32]);
    }
    let mut go = true;
    while go && (hs[0].cur as u64 + hs[1].cur as u64) < m[0] {
        go = match hs[0].len > hs[1].len {
            true => step::<1>(pg, &mut hs[1], g[1], &mut m),
            false => step::<0>(pg, &mut hs[0], g[0], &mut m),
        };
    }
    let [mu, skip] = m; // mu = u64::MAX (no path) casts to -1
    (skip >= mu && (skip == u64::MAX || mu < INF as u64)).then_some(mu as i64)
}

/// One step of side S (0: forward, 1: backward, so the side is a constant in the code): pop its
/// smallest entry, settle the node unless the entry is stale, and move to the next key. m is
/// [mu, skip]. False once the side's queue is empty.
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
            if ld(&pg.pool, iv) as i32 <= nd as i32 {
                return; // settled (negative as i32), or labelled no worse
            }
            *mt(&mut pg.pool, iv) = nd as u32;
            // Start fetching the node's edges now: it is usually popped later.
            let (a, b) = (gr.off(v as usize), gr.off(v as usize + 1));
            prefetch(gr.edges.as_ptr().wrapping_add(a));
            h.put(nd as u32, [iv as u32, a as u32, b as u32]);
            let o = ld(&pg.pool, pg.ix(v, 1 - S)) & !SETTLED; // INF: no label on the other side
            m[0] = m[0].min(nd + o as u64 | (o == INF) as u64 * u64::MAX);
        });
    }
    h.min()
}

/// One-ended Dijkstra with u64 distances, for the few queries whose distances need more than
/// 31 bits (edges of weight near 2^32 at the special nodes). Speed does not matter here: a stale
/// heap entry is simply expanded again (it cannot improve anything).
fn plain(g: &Graph, s: usize, t: usize) -> i64 {
    use std::collections::{BTreeSet, HashMap};
    let (mut dist, mut q) = (HashMap::from([(s, 0)]), BTreeSet::from([(0, s)]));
    while let Some((d, u)) = q.pop_first().filter(|&(_, u)| u != t) {
        g.adj(u, |v, w| {
            let (e, nd) = (dist.entry(v as usize).or_insert(u64::MAX), d + w as u64);
            if nd < *e {
                *e = nd;
                q.insert((nd, v as usize));
            }
        });
    }
    dist.get(&t).map_or(-1, |&d| d as i64) // t is popped at its distance, or never labelled
}

fn main() {
    // The scoring box backs anonymous memory with 2 MB pages (THP "always") and memory.peak
    // charges a whole 2 MB page for its first touched byte. Nothing here gains from 2 MB pages
    // (measured on the board), so the process opts out: PR_SET_THP_DISABLE = 41. One malloc arena
    // (M_ARENA_MAX = 1) instead of one per thread keeps the search's small vectors packed.
    unsafe { (prctl(41, 1, 0, 0, 0), mallopt(-8, 1)) };
    let t0 = std::time::Instant::now();
    let par0 = std::thread::available_parallelism().map_or(1, |p| p.get());
    let threads = std::env::var("FW_THREADS").map_or(par0, |v| v.parse().unwrap());
    let (q, mut qs, mut j) = (map("/data/queries.txt"), Vec::new(), 0);
    while j < q.len() {
        qs.push((num(q, &mut j) as usize, num(q, &mut j) as usize));
    }

    let (gt, mut i) = (map("/data/graph.txt"), 0);
    let (n, _m) = (num(gt, &mut i) as usize, num(gt, &mut i)); // the edge lines are read to the end
    let body = &gt[i..];
    let vb = (usize::BITS - (n.max(2) - 1).leading_zeros()).max(1);
    let esc = (1u32 << (32 - vb)) - 1;
    let g = {
        let g0 = csr(n, threads, vb, esc, &spans(body, body.len(), threads * 4));
        let newid = bfs_order(&g0, threads);
        qs.iter_mut().for_each(|q| *q = (newid[q.0] as usize, newid[q.1] as usize));
        relabel(&g0, &newid, threads)
    };
    let r = csr(n, threads, vb, esc, &spans(&g, n, threads * 4));
    let t1 = t0.elapsed();

    let ans: Vec<AtomicI64> = qs.iter().map(|_| AtomicI64::new(-1)).collect();
    let next = AtomicUsize::new(0);
    par(threads, &vec![(); threads], |_| {
        let (mut pg, mut hs) = (Pages::new(n), <[Dial; 2]>::default());
        loop {
            let k = next.fetch_add(1, Relaxed);
            let Some(&(s, t)) = qs.get(k) else { break };
            let a = bidir(&mut pg, &mut hs, [&g, &r], s, t);
            ans[k].store(a.unwrap_or_else(|| plain(&g, s, t)), Relaxed);
        }
    });
    let out: String = ans.iter().map(|a| format!("{a:?}\n")).collect(); // Debug: the value
    std::fs::write("/out/distances.txt", out).unwrap();
    eprintln!("build {t1:?} total {:?}", t0.elapsed());
    std::process::exit(0); // the kernel reclaims everything; do not free the big arrays one by one
}
