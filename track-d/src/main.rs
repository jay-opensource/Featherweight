//! Track D (Traverse), Featherweight: minimum memory per unit of speed. Reads `/data/graph.txt` (a
//! directed weighted road-like graph, 1,740,512 nodes, 5M edges, 88 MB of text) and
//! `/data/queries.txt` (2,000 source-target pairs) and writes `/out/distances.txt`: each
//! shortest-path length, or -1. README.md has the design in prose; PERF.md every board run (step
//! numbers below are its "steps" and "Findings specific to the scoring box").
//!
//! Pipeline (`main`; every stage runs on a pool of scoped threads via `par`):
//! 1. Parse + build (`csr`): the text is parsed once, in byte ranges, into a compact CSR built in
//!    buckets of 8,192 nodes, so every random access stays in a small region.
//! 2. Relabel (`relabel`): nodes are renumbered in breadth-first order, so neighbours in the
//!    road network are neighbours in memory. The BFS copies each node's slots as it visits it, one
//!    pass moves them into the first graph's mapped edge array with the new ids, and the reverse
//!    graph is built from the result as in step 1.
//! 3. Queries (`bidir`, `step`): a two-ended Dijkstra per query, farthest pairs first, with labels
//!    in small per-thread pages (`Pages`) and a Dial bucket queue (`Dial`) per side. Rare queries
//!    that need edges of weight W or more or 32-bit overflow fall back to `via`, then `plain`.
//! 4. Output and exit: write the answers, give the big mappings back on all threads, `exit(0)`.
//!
//! Data structures: `Graph` is a CSR with a 4-byte edge slot (`target | weight << vb`, an escape
//! value for the 0.07% of weights that do not fit) and about 1.25 bytes of offsets per node
//! (a u32 base per block of 16 nodes plus a u8 offset). No atomics or locks guard the data: every
//! writer owns a disjoint range, which is what the `unsafe` blocks below rely on.
//!
//! Box-specific decisions (scoring box: i5-13500, THP "always", memory.peak charges whole 2 MB
//! pages, exit teardown runs on one core; PERF.md "Findings specific to the scoring box"):
//! - THP off for the process (`prctl` in `main`): once BFS order made the searches local, 2 MB pages
//!   gained nothing, and turning them off took the board from 76.5 to 64.3 MB (steps 4-6, 24-25).
//! - `graph.txt` and `queries.txt` are mapped read-only, never copied: the page cache is not charged.
//! - Lock-free builders: atomic cursors saved 5 MB but ran 0.208 s against 0.178 s (step 16).
//! - Exit: no frees, and every thread `MADV_DONTNEED`s its share of the big arrays first, taking
//!   the exit teardown from 8.45 to 3.62 ms (steps 22, 40).
use std::{fs::File, os::unix::io::AsRawFd};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering::*};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn prctl(option: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

/// Read-only private mapping of a whole file; the page cache is not charged to us, a heap copy would be.
fn map(path: &str) -> &'static [u8] {
    let f = File::open(path).expect(path);
    let len = f.metadata().unwrap().len() as usize; // an empty file still maps a page, never read
// SAFETY: a read-only private file mapping (1 = PROT_READ, 2 = MAP_PRIVATE); the checked result is
// the whole file, never written or unmapped, so 'static holds.
    let p = unsafe { mmap(std::ptr::null_mut(), len.max(1), 1, 2, f.as_raw_fd(), 0) };
    assert!(p as isize != -1, "mmap {path}");
// SAFETY: len bytes of the mapping above.
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
    // SAFETY: a prefetch is only a hint and never faults, whatever the address.
    unsafe { std::arch::x86_64::_mm_prefetch(x as *const i8, std::arch::x86_64::_MM_HINT_T0) };
}

/// s[i] without a bounds check, for the hot lookups (their indices come from the graph).
fn ld<T: Copy>(s: &[T], i: usize) -> T {
// SAFETY: callers pass node ids (< n) or slot offsets that the builders produced, in bounds.
    unsafe { *s.get_unchecked(i) }
}

/// &mut s[i] without a bounds check.
fn mt<T>(s: &mut [T], i: usize) -> &mut T {
// SAFETY: as in `ld`.
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
// SAFETY: the users (`csr`, `finish`, `bfs_order`) write only indices that no other thread writes
// (see the comment at each use), and the array outlives the `par` scope that shares it.
unsafe impl<T> Sync for Slots<T> {}

/// Compact CSR. Node k's slots start at base[k >> sh] + rel[k] (see `store` for a slot).
#[derive(Default)]
struct Graph {
    // Edge slots: `target | weight << vb`; the weight value `esc` means the full weight is in the next slot.
    edges: Vec<u32>,
    // First slot of each block of 2^sh nodes; rel[k] is node k's offset inside its block.
    base: Vec<u32>,
    rel: Vec<u8>,
    // sh: log2 of the block size (4, i.e. 16 nodes, unless an offset overflowed a u8); vb: bits of a
    // target (ceil(log2 n)); esc: the all-ones weight, `u32::MAX >> vb`.
    sh: u32,
    vb: u32,
    esc: u32,
}

impl Graph {
    /// First slot of node k (and, for k + 1, one past its last); defined for k <= n.
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

/// The edges u -> v of nodes lo..hi of a graph, reversed: as v -> u.
impl Edges for (&Graph, usize, usize) {
    fn each(&self, mut emit: impl FnMut(usize, u32, u32)) {
        (self.1..self.2).for_each(|u| self.0.adj(u, |v, w| emit(v as usize, u as u32, w)))
    }
}

/// Build a CSR over n nodes from the edges of `it`. Every random access stays inside a small
/// region:
/// 1. count each item's slots per bucket of 8192 nodes (213 counters per item, no atomics);
/// 2. append each slot to its bucket's region through the item's own cursor, with the node's
///    place inside the bucket in a parallel u16 array (213 sequential streams per thread; both
///    slots of an escaped edge get it, so the slots can be counted and moved one by one);
/// 3. per bucket, count each node's slots from the u16s alone, lay its offsets out from
///    the bucket's first slot, and reorder the bucket's ~94 KB of slots by node through a scratch
///    copy. Offsets come in blocks of 16 nodes (a u32 base each, u8 offsets inside); if an
///    in-block offset does not fit, they are laid out again with smaller blocks (the slots stay).
/// Nothing is locked, and no store lands outside the bucket being worked on. `bf` is Ok(the counts
/// of step 1) or Err(est); step 1 runs only if every count is 0. If est > 0 (the expected slot
/// count), the counts are rooms instead: each item gets its share of est in each bucket plus 6
/// standard deviations (the text is parsed once), the unused room is holes (place 0), and step 3
/// packs the slots: a bucket's packed slots start at or before its staged ones, over the staged
/// slots of earlier buckets, so it writes them only once those buckets have copied theirs (buckets
/// are taken in order, so it waits only for ones being worked on). A segment that would overflow
/// starts it all again, counted.
fn csr(n: usize, t: usize, vb: u32, it: &[impl Edges], bf: Result<Vec<u32>, usize>) -> Graph {
    let esc = u32::MAX >> vb; // the escape value: all ones in the weight bits
    let (nb, ni) = ((n >> BS) + 1, it.len());
    let est = *bf.as_ref().err().unwrap_or(&0);
    // the room of item j / nb in bucket j % nb (8192 of the n + 1 nodes); 0 if counted
    let mu = |j: usize| (est * (n + 1 - j % nb * 8192).min(8192) / (n + 1) / ni) as f64;
    let room = |j: usize| (mu(j) + 6.0 * mu(j).sqrt()) as u32;
    let mut cur = bf.unwrap_or_else(|_| (0..ni * nb).map(room).collect());
    let cp = Slots(cur.as_mut_ptr());
    if cur.iter().all(|&c| c == 0) {
        par(t, &(0..ni).collect::<Vec<_>>(), |&i| {
            // SAFETY: item i owns the counters i * nb..(i + 1) * nb of `cur` and one thread runs each item,
            // so the slice is not aliased. `(&cp).0` borrows the whole `Slots` so the closure stays `Sync`.
            let c = unsafe { std::slice::from_raw_parts_mut((&cp).0.add(i * nb), nb) };
            it[i].each(|u, _, w| c[u >> BS] += 1 + (w >= esc) as u32);
        });
    }
    let (mut st, mut acc, mut end) = (vec![[0, 0]; nb + 1], 0u32, vec![0; ni * nb]);
    for b in 0..nb {
        for i in 0..ni {
            (cur[i * nb + b], acc) = (acc, acc + cur[i * nb + b]);
            end[i * nb + b] = acc as usize; // where the segment ends
        }
        st[b + 1][0] = acc as usize; // each bucket's first slot: staged, packed (no holes)
    }
    let seg = cur.clone(); // where each segment starts
    let (mut edges, mut loc) = (vec![0; acc as usize], vec![0u16; acc as usize]);
    let (ep, lp) = (Slots(edges.as_mut_ptr()), Slots(loc.as_mut_ptr()));
    let ok = AtomicBool::new(true);
    par(t, &(0..ni).collect::<Vec<_>>(), |&i| {
        // SAFETY: as in step 1 (item i alone uses its own counters).
        let c = unsafe { std::slice::from_raw_parts_mut((&cp).0.add(i * nb), nb) };
        it[i].each(|u, v, w| {
            let (b, l) = (u >> BS, (u & ((1 << BS) - 1)) as u16 + 1);
            if est > 0 && c[b] as usize + 1 + (w >= esc) as usize > end[i * nb + b] {
                return ok.store(false, Relaxed);
            }
            // Edge (v, w) at slot p: one slot `v | w << vb`, or, when w does not fit, the escape
            // value in that slot and the full weight in the next (written first, so that without
            // an escape it is overwritten).
            let (p, k) = (c[b] as usize, 1 + (w >= esc) as usize);
            // SAFETY: slots p..p + k belong to item i's segment of bucket b: with an estimate they are checked
            // against `end` above, with exact counts they are reserved by step 1. Segments are disjoint, so
            // no other thread writes these slots.
            unsafe { (*(&ep).0.add(p + k - 1), *(&ep).0.add(p)) = (w, v | w.min(esc) << vb) };
            c[b] += k as u32;
            // SAFETY: the same slots of `loc`, owned by the same segment.
            unsafe { (*(&lp).0.add(p), *(&lp).0.add(p + k - 1)) = (l, l) };
        });
    });
    if !ok.into_inner() {
        return csr(n, t, vb, it, Err(0));
    }
    for b in 0..nb {
        let used: u32 = (0..ni).map(|i| cur[i * nb + b] - seg[i * nb + b]).sum();
        st[b + 1][1] = st[b][1] + used as usize;
    }
    // Step 3: the slots of each bucket in node order (`loc` and the staged `edges` are dropped).
    let mut g = Graph::default();
    (g.edges, g.vb, g.esc) = (edges, vb, esc);
    let copied = Vec::from_iter((0..nb).map(|_| AtomicBool::new(false)));
    let ep = Slots(g.edges.as_mut_ptr());
// Try blocks of 2^4 = 16 nodes first; if an in-block offset does not fit a u8, lay out again with
// half-size blocks (the slots stay where they are).
    _ = (0..=4).rev().find(|&sh| {
        (g.sh, g.rel, g.base) = (sh, vec![0; n + 1], vec![0; (n >> sh) + 1]);
        let (rp, bp) = (Slots(g.rel.as_mut_ptr()), Slots(g.base.as_mut_ptr()));
        let ok = AtomicBool::new(true);
        par(t, &(0..nb).collect::<Vec<_>>(), |&b| {
            let ([s0, lo], [s1, _], k0) = (st[b], st[b + 1], b << BS);
            // SAFETY: s0..s1 is bucket b's staged range inside `edges`. It is copied out before `copied[b]`
            // is set, and packed writes into any earlier bucket's staged range wait for that flag (below).
            let e = unsafe { std::slice::from_raw_parts((&ep).0.add(s0), s1 - s0) }.to_vec();
            copied[b].store(true, Release);
            let mut at = vec![0u32; (n + 1 - k0).min(1 << BS)];
            for &l in &loc[s0..s1] {
                at.get_mut(l as usize - 1).map(|a| *a += 1); // not a hole
            }
            let (mut p, mut p0) = (lo as u32, 0);
            for (k, a) in (k0..).zip(&mut at) {
                if k & ((1 << sh) - 1) == 0 {
                    // SAFETY: bucket b owns nodes k0.. of its 8,192 (a multiple of the block size), so it alone
                    // writes these `base` entries.
                    (p0, _) = (p, unsafe { *(&bp).0.add(k >> sh) = p }); // a block starts at p
                }
                (p - p0 > 255).then(|| ok.store(false, Relaxed));
                (*a, p) = (p, p + *a);
                // SAFETY: k <= n is in this bucket's node range of `rel`, written by this thread only.
                unsafe { *(&rp).0.add(k) = (*a - p0) as u8 };
            }
            let mut w = b; // wait for the earlier buckets whose staged slots end after lo
            while w > 0 && st[w][0] > lo {
                w -= copied[w - 1].load(Acquire) as usize;
            }
            for (&x, &l) in e.iter().zip(&loc[s0..s1]).filter(|_| sh == 4) {
                if let Some(a) = at.get_mut(l as usize - 1) {
                    // SAFETY: *a is inside bucket b's packed range, which is disjoint from all other packed
                    // ranges and, by the wait above, no longer needed by the buckets that staged slots there.
                    unsafe { *(&ep).0.add(*a as usize) = x };
                    *a += 1;
                }
            }
        });
        ok.into_inner() // (blocks of 1 node always fit)
    });
    (g.edges.truncate(st[nb][1]), g.edges.shrink_to_fit()); // the tail goes back
    g
}

/// Bits of a bucket's node range in `csr`.
// 13 bits = 8,192 nodes per bucket: ~94 KB of slots, small enough to reorder in cache (213 buckets).
const BS: u32 = 13;

/// g0 in breadth-first order, built in parallel while the order is found: item j grows a region
/// from a seed in its slice of the id range (claiming nodes with one bit each) and re-seeds from
/// that slice when its region is enclosed. New ids follow one region's visit order, then the next
/// region's. A node is dequeued with its slots in cache (from claiming its neighbours): it gets the
/// id j << vb | its rank in the region, its slots are appended verbatim to item j's segment, and
/// its queue entry becomes its first slot there. This replaces a relabel builder whose 30 MB of
/// staging were the process's memory peak. The scratch, the n ids and then segment j at n + j * cap
/// (cap: g0's slot count), is one allocation, of which only the pages written are charged. Then,
/// per bucket of 8192 new ids, the offsets are laid out, in the largest blocks (16 nodes on these
/// graphs) whose in-block offsets fit a u8, and (in the first pass) the slots are moved into g0's
/// edge array with every target rewritten and the reverse graph's slots counted into rc[b * nb +
/// (v >> BS)] for each edge u -> v of bucket b (step 1 of a build of the reverse graph whose items
/// are single buckets). The queries are mapped to the new ids.
fn relabel(g0: Graph, t: usize, qs: &mut [(usize, usize)], rc: &mut Vec<u32>) -> Graph {
    let (n, cap, vb, esc) = (g0.rel.len() - 1, g0.edges.len(), g0.vb, g0.esc);
    let lists = std::sync::Mutex::new(vec![(vec![], 0); t]);
    let mut g = Graph::default();
    // allocated below the lists, so that the heap gives their pages back when they are freed
    (g.rel, g.base, g.vb, g.esc) = (vec![0; n + 1], vec![0; (n >> 4) + 1], vb, esc);
    let mut seg = vec![0u32; n + t * cap];
    let bits = Vec::from_iter((0..=n / 64).map(|_| AtomicU64::new(0)));
    let claim = |&v: &usize| {
        let (b, m) = (&bits[v / 64], 1 << v % 64); // one claim bit per node (218 KB, cache-resident)
        b.load(Relaxed) & m == 0 && b.fetch_or(m, Relaxed) & m == 0
    };
    let items = Vec::from_iter((0..t).map(|j| [j, j * n / t, (j + 1) * n / t]));
    let sp = Slots(seg.as_mut_ptr());
    par(t, &items, |&[j, mut sc, e]| {
        let (mut list, mut head, mut at) = (Vec::with_capacity(n), 0, n + j * cap);
        loop {
            if head == list.len() {
                let Some(s) = (sc..e).find(claim) else { break };
                (sc, _) = (s + 1, list.push(s as u32));
            }
            // Software pipeline over the queue: offsets 16 nodes ahead, edge lists 8 ahead, so
            // the misses of upcoming nodes overlap (the claim bits stay in cache).
            if let Some(&w) = list.get(head + 16) {
                prefetch(&g0.rel[w as usize]);
                prefetch(&g0.base[w as usize >> g0.sh]);
            }
            if let Some(&w) = list.get(head + 8) {
                prefetch(g0.edges.as_ptr().wrapping_add(g0.off(w as usize)));
            }
            let u = list[head] as usize;
            g0.adj(u, |v, _| _ = claim(&(v as usize)).then(|| list.push(v)));
            let (a, b, d) = (g0.off(u), g0.off(u + 1), (&sp).0.wrapping_add(at));
            // SAFETY: only this item dequeues u, and segment j (room for all of g0's slots) is its own.
            unsafe { *(&sp).0.add(u) = (j << vb | head) as u32 };
            unsafe { std::ptr::copy_nonoverlapping(g0.edges.as_ptr().add(a), d, b - a) };
            (list[head], head, at) = ((at - n - j * cap) as u32, head + 1, at + b - a);
        }
        lists.lock().unwrap()[j] = (list, at - n - j * cap);
    });
    let (ls, mut rb) = (lists.into_inner().unwrap(), vec![[0; 2]]); // each region's first id, slot
    for (l, s) in &ls {
        let r = rb[rb.len() - 1];
        rb.push([r[0] + l.len(), r[1] + s]);
    }
    // new id k is node k - rb[j][0] of region j; its first slot (k = n: the slot count)
    let off = |k: usize| {
        let j = rb.partition_point(|r| r[0] <= k) - 1;
        rb[j][1] + ls.get(j).map_or(0, |l| l.0[k - rb[j][0]] as usize)
    };
    let (mask, nb) = ((1 << vb) - 1, (n >> BS) + 1);
    let fin = |v: usize| rb[ld(&seg, v) as usize >> vb][0] + (ld(&seg, v) & mask) as usize;
    qs.iter_mut().for_each(|q| *q = (fin(q.0), fin(q.1)));
    (g.edges, *rc) = ({ g0 }.edges, vec![0; nb * nb]); // g0's offsets are freed
    let (ep, cp) = (Slots(g.edges.as_mut_ptr()), Slots(rc.as_mut_ptr()));
    _ = (0..=4).rev().find(|&sh| {
        (g.sh, _) = (sh, g.base.resize((n >> sh) + 1, 0));
        let (rp, bp) = (Slots(g.rel.as_mut_ptr()), Slots(g.base.as_mut_ptr()));
        let ok = AtomicBool::new(true);
        par(t, &(0..nb).collect::<Vec<_>>(), |&b| {
            let blocks = b << BS >> sh..=n.min((b + 1 << BS) - 1) >> sh; // with the sentinel n
            for (c, p) in blocks.map(|c| (c, off(c << sh))) {
                // SAFETY: bucket b alone writes its blocks' offsets, its nodes' slots and row b of `rc`.
                unsafe { *(&bp).0.add(c) = p as u32 };
                for (k, d) in (c << sh..(c + 1 << sh).min(n + 1)).map(|k| (k, off(k) - p)) {
                    (d > 255).then(|| ok.store(false, Relaxed));
                    unsafe { *(&rp).0.add(k) = d as u8 };
                }
            }
            for j in (0..t).filter(|_| sh == 4) {
                // region j's nodes in bucket b: slots s..e of g, at s + d in the scratch
                let (lo, hi) = (rb[j][0].max(b << BS), rb[j + 1][0].min(b + 1 << BS));
                let (mut s, e, d) = (off(lo), off(hi.max(lo)), n + j * cap - rb[j][1]);
                while s < e {
                    if let Some(&x) = seg.get(s + d + 16) {
                        prefetch(seg.as_ptr().wrapping_add((x & mask) as usize)); // random reads
                    }
                    let (x, k) = (ld(&seg, s + d), 1 + (ld(&seg, s + d) >> vb == esc) as usize);
                    let y = fin((x & mask) as usize) as u32;
                    unsafe {
                        *(&ep).0.add(s + k - 1) = ld(&seg, s + d + k - 1); // the full weight, if escaped
                        *(&ep).0.add(s) = y | x & !mask;
                        *(&cp).0.add(b * nb + (y >> BS) as usize) += k as u32;
                    }
                    s += k;
                }
            }
        });
        ok.into_inner() // (blocks of 1 node always fit)
    });
    g
}

/// Buckets per queue: the search relaxes only edges lighter than W (see `bidir`). 8,192 is above
/// the largest main-body weight, and the queue is W list heads (32 KB) per side.
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

    /// Take an item of key cur (`step` moves cur to the smallest key after each pop).
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
}

/// Distances in the two-ended search are 31 bits; the top bit marks "settled".
const SETTLED: u32 = 1 << 31;
const INF: u32 = SETTLED - 1;

/// Labels of the two-ended search, per side, in pages of 2^pb (64 on these graphs) consecutive
/// node ids (neighbours in BFS order) allocated on first touch from a per-thread pool. The two
/// balls grow around s and around t, so each side gets its own pages. dir[side] holds a page's
/// number in the pool as a u16 (half the cache footprint of an offset); one array per side, so
/// the side's base address is one load (2% fewer query cycles than one array with an offset).
/// pb grows if n ever needs more numbers. Page 0 is all INF and stands for every page not
/// allocated yet, so reading a label never branches.
struct Pages {
    dir: [Vec<u16>; 2],
    pb: u32,
    pool: Vec<u32>,
    own: Vec<(usize, usize)>, // the dir entries of the pages in use
}

impl Pages {
    /// Empty labels for n nodes. Page numbers are u16 (< 32,766 pages), so pb, the log2 of the page
    /// size, grows beyond 6 (64 nodes) only if n needs more pages than that.
    fn new(n: usize) -> Self {
        let pb = 6.max(usize::BITS - (n / 32766).leading_zeros());
        let dir = [0; 2].map(|_| vec![0; (n >> pb) + 1]);
        let (pool, own) = (vec![INF; 1 << pb], vec![]);
        Pages { dir, pb, pool, own }
    }

    /// Index of v's label on `side`, allocating its page.
    #[inline(always)]
    fn at(&mut self, v: u32, side: usize) -> usize {
        let d = (v >> self.pb) as usize;
        if ld(&self.dir[side], d) == 0 {
            self.dir[side][d] = (self.pool.len() >> self.pb) as u16;
            self.pool.resize(self.pool.len() + (1 << self.pb), INF);
            self.own.push((side, d));
        }
        self.ix(v, side)
    }

    /// Index of v's label on `side`, in page 0 if v has none.
    #[inline(always)]
    fn ix(&self, v: u32, side: usize) -> usize {
        let d = (v >> self.pb) as usize;
        (ld(&self.dir[side], d) as usize) << self.pb | v as usize & ((1 << self.pb) - 1)
    }
}

/// Two-ended Dijkstra: forward from s on g[0], backward from t on the reverse graph g[1], always
/// expanding the side with fewer queued entries. mu is the best s-t length seen through any node
/// labelled by both sides, starting at the cap c; stop when the two queue minimums together reach mu,
/// which is then min(d(s, t), c) (u64::MAX: no path). A source with no way out or a target with
/// no way in empties its queue at once, so those end immediately; s = t starts with mu = 0.
/// Two kinds of relaxation are skipped, and the smallest length they would have given is kept in
/// `skip`: a label that would not fit 31 bits, and an edge of weight W or more (none is reached on
/// seed 1). The search is then exact on the graph without the heavy edges for every distance below
/// 2^31. If a shorter s-t path used a heavy edge, the first one on it starts at a node the forward
/// search settled, or the last one ends at a node the backward search settled (else the path
/// would be at least the two queue minimums, hence mu), so skip is below mu. So mu is the answer
/// unless skip < mu, or something was skipped and mu does not fit 31 bits: then return None and
/// the caller runs via().
fn bidir(pg: &mut Pages, hs: &mut [Dial; 2], g: [&Graph; 2], [s, t, c]: [usize; 3]) -> Option<u64> {
    pg.own.drain(..).for_each(|(s, d)| pg.dir[s][d] = 0); // free the pages
    pg.pool.truncate(1 << pg.pb);
    let mut m = [if s == t { 0 } else { c as u64 }, u64::MAX]; // mu, skip
    for (side, x, gr) in [(0, s, g[0]), (1, t, g[1])] {
        let i = pg.at(x as u32, side);
        pg.pool[i] = 0;
        let h = &mut hs[side]; // emptied (the first time, sized)
        (h.free, h.cur, h.len, h.head) = (0, 0, 0, vec![0; W]);
        h.pool.resize(1, [0; 4]);
        h.put(0, [i as u32, gr.off(x) as u32, gr.off(x + 1) as u32]);
    }
    // Step the chosen side while the minimums are below mu; a step that empties its queue ends it.
    while (hs[0].cur as u64 + hs[1].cur as u64) < m[0]
        && match hs[0].len > hs[1].len {
            true => step::<1>(pg, &mut hs[1], g[1], &mut m),
            false => step::<0>(pg, &mut hs[0], g[0], &mut m),
        }
    {}
    let [mu, skip] = m;
    (skip >= mu && (skip == u64::MAX || mu < INF as u64)).then_some(mu)
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
    while h.len > 0 && ld(&h.head, h.cur as usize % W) == 0 {
        h.cur += 1; // to the smallest key
    }
    h.len > 0
}

/// d(s, t) for a query that bidir gave up on: the minimum over the out-edges (s, v, w) of
/// w + d(v, t). A source without in-edges leaves through edges near 2^32 into the main body, where a
/// one-ended search settles every node closer than t (15 ms for one query on holdout seed 11). Each
/// round bounds the answer by b = lo + 2^12, 2^14, ... (lo: the lightest edge) and caps each
/// bidir(v, t) at min(b, best) - w, so no search grows past what the bound allows; a result below b
/// is exact. plain() takes the searches that give up again (heavy edges inside an island).
fn via(pg: &mut Pages, hs: &mut [Dial; 2], g: [&Graph; 2], s: usize, t: usize) -> u64 {
    let mut out = vec![];
    g[0].adj(s, |v, w| out.push((w as u64, v as usize)));
    let (lo, mut best, mut k) = (out.iter().map(|e| e.0).min().unwrap_or(0), u64::MAX, 54);
    while k > 0 && best >= lo.saturating_add(u64::MAX >> k) {
        k -= 2; // the last round (k = 0) has no bound
        let b = lo.saturating_add(u64::MAX >> k);
        best = out.iter().fold(u64::MAX, |best, &(w, v)| {
            let d = bidir(pg, hs, g, [v, t, b.min(best).saturating_sub(w) as usize]);
            best.min(w.saturating_add(d.unwrap_or_else(|| plain(g[0], v, t))))
        });
    }
    best
}

/// One-ended Dijkstra with u64 distances, for the few paths that need edges of weight W or more
/// (inside the islands, see via). A stale heap entry is simply expanded again (it cannot
/// improve anything). u64::MAX if t is unreachable.
fn plain(g: &Graph, s: usize, t: usize) -> u64 {
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
    *dist.get(&t).unwrap_or(&u64::MAX) // t is popped at its distance, or never labelled
}

fn main() {
    // The scoring box backs anonymous memory with 2 MB pages (THP "always") and memory.peak
    // charges a whole 2 MB page for its first touched byte. Nothing here gains from 2 MB pages
    // (measured on the board), so the process opts out: PR_SET_THP_DISABLE = 41. One malloc arena
    // (M_ARENA_MAX = 1) instead of one per thread keeps the search's small vectors packed.
// SAFETY: plain libc calls made before any thread starts (41 = PR_SET_THP_DISABLE, -8 =
// M_ARENA_MAX); they only change allocator and page-size policy for this process.
    unsafe { (prctl(41, 1, 0, 0, 0), mallopt(-8, 1)) };
    let par0 = std::thread::available_parallelism().map_or(1, |p| p.get());
    let threads = std::env::var("FW_THREADS").map_or(par0, |v| v.parse().unwrap());
    let (q, mut qs, mut j) = (map("/data/queries.txt"), Vec::new(), 0);
    while j < q.len() {
        qs.push((num(q, &mut j) as usize, num(q, &mut j) as usize));
    }

    let (gt, mut i) = (map("/data/graph.txt"), 0);
    // n, m (the edge lines are simply read to the end) and the text after the header line
    let (n, m, tx) = (num(gt, &mut i) as usize, num(gt, &mut i) as usize, &gt[i..]);
    let vb = (usize::BITS - (n.max(2) - 1).leading_zeros()).max(1);
    let k = threads * 4; // work items per pass
    let est = Err(m.min(tx.len() / 6)); // a line has at least 6 bytes
    let g0 = csr(n, threads, vb, &spans(tx, tx.len(), k), est);
    // g0 in BFS order (in g0's edge array), with the queries mapped and the reverse graph's counts
    // for items of one bucket each; then the reverse graph.
    let mut rc = vec![];
    let g = relabel(g0, threads, &mut qs, &mut rc);
    let rev = Vec::from_iter((0..=n >> BS).map(|b| (&g, b << BS, n.min(b + 1 << BS))));
    let r = csr(n, threads, vb, &rev, Ok(rc));

    let ans: Vec<AtomicI64> = qs.iter().map(|_| AtomicI64::new(-1)).collect();
    let next = AtomicUsize::new(0);
    // Far pairs in BFS order first: |s - t| tracks the size of a search (correlation 0.48 in logs),
    // so the long searches start early and short ones fill the end. Query phase on 12 threads, 40
    // in-process pairs against the file order: 0.981, 0.989, 0.978 on seeds 1, 11 and 12.
    let mut o = Vec::from_iter(qs.iter().enumerate());
    o.sort_by_key(|&(_, &(s, t))| std::cmp::Reverse(s.abs_diff(t)));
    par(threads, &vec![(); threads], |_| {
        let (mut pg, mut hs) = (Pages::new(n), <[Dial; 2]>::default());
        while let Some(&(k, &(s, t))) = o.get(next.fetch_add(1, Relaxed)) {
            let a = bidir(&mut pg, &mut hs, [&g, &r], [s, t, usize::MAX]);
            let a = a.unwrap_or_else(|| via(&mut pg, &mut hs, [&g, &r], s, t));
            ans[k].store(a as i64, Relaxed); // u64::MAX (no path) is -1
        }
    });
    let out: String = ans.iter().map(|a| format!("{a:?}\n")).collect(); // Debug: the value
    std::fs::write("/out/distances.txt", out).unwrap();
    // The kernel's exit unmaps every page on one core (~0.1-0.4 us per 4 KB page; 5 ms locally
    // for the text and the two graphs). Drop those pages on all threads first: MADV_DONTNEED on the
    // whole pages inside each thread's share (nothing reads them again).
// SAFETY: u32 to u8 (the same bytes, no padding), so `align_to` gives the whole array as bytes.
    let big = unsafe { [gt, g.edges.align_to().1, r.edges.align_to().1] };
    let parts = Vec::from_iter(big.iter().flat_map(|&b| spans(b, b.len(), threads)));
    par(threads, &parts, |&(b, lo, hi)| {
        let [a, e] = [lo + 4095, hi].map(|x| b.as_ptr() as usize + x & !4095);
// SAFETY: a..e is whole pages inside the slice (a rounded up, e down; 4 = MADV_DONTNEED), and
// nothing reads the arrays again: the next step is `exit`.
        unsafe { madvise(a as *mut u8, e.saturating_sub(a), 4) };
    });
    std::process::exit(0); // the kernel reclaims the rest; do not free the big arrays one by one
}
