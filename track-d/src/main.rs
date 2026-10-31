// Track D, featherweight: every edge is 4 bytes and every node costs 1 byte plus 1/16 of a word,
// so the graph sits in L3. Each search carries a settled bitset and a small frontier map, not a
// distance array the size of the graph.
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicI64, AtomicU16, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
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
    has_in: Vec<u64>,
}

impl Graph {
    #[inline(always)]
    fn off(&self, k: usize) -> usize {
        self.base[k >> self.sh] as usize + self.rel[k] as usize
    }
}

fn build(g: &'static [u8], threads: usize) -> Graph {
    let mut i = 0;
    let n = num(g, &mut i) as usize;
    let _m = num(g, &mut i);
    let body = &g[i..];
    let vb = (usize::BITS - (n.max(2) - 1).leading_zeros()).max(1);
    let esc = (1u32 << (32 - vb)) - 1;
    let parts = chunks(body, threads * 4);
    let t = Instant::now();

    // Pass 1: slots per node (1, or 2 with an escape word). The counts become countdown cursors.
    let deg: Vec<AtomicU16> = (0..n + 1).map(|_| AtomicU16::new(0)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                while let Some(&(lo, hi)) = parts.get(next.fetch_add(1, Relaxed)) {
                    let mut i = lo;
                    while i < hi {
                        let u = num(body, &mut i) as usize;
                        while body[i] != b' ' {
                            i += 1;
                        }
                        i += 1;
                        let w = num(body, &mut i);
                        let k = 1 + (w >= esc as u64) as u16;
                        let old = deg[u].fetch_add(k, Relaxed);
                        assert!(old < u16::MAX - 2, "out-degree above 65k");
                    }
                }
            });
        }
    });
    let t1 = t.elapsed();

    // Prefix: the largest block whose in-block offsets fit a u8 (16 nodes on these graphs).
    let mut sh = 4u32;
    let (base, rel) = loop {
        let bs = 1usize << sh;
        let mut rel = vec![0u8; n + 1];
        let mut base = Vec::with_capacity((n >> sh) + 2);
        let (mut total, mut ok) = (0u32, true);
        for b0 in (0..=n).step_by(bs) {
            base.push(total);
            let mut acc = 0u32;
            for k in b0..(b0 + bs).min(n + 1) {
                if acc > 255 {
                    ok = false;
                }
                rel[k] = acc as u8;
                acc += deg[k].load(Relaxed) as u32;
            }
            total += acc;
        }
        base.push(total);
        if ok || sh == 0 {
            break (base, rel);
        }
        sh -= 1;
    };
    let total = *base.last().unwrap() as usize;
    let t2 = t.elapsed();

    // Pass 2: fill. Each node's cursor counts down, so threads write disjoint slots.
    let mut edges = vec![0u32; total];
    let out = Slots(edges.as_mut_ptr());
    let has_in: Vec<AtomicU64> = (0..n / 64 + 1).map(|_| AtomicU64::new(0)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let out = &out;
                while let Some(&(lo, hi)) = parts.get(next.fetch_add(1, Relaxed)) {
                    let mut i = lo;
                    while i < hi {
                        let u = num(body, &mut i) as usize;
                        let v = num(body, &mut i) as u32;
                        let w = num(body, &mut i);
                        let big = w >= esc as u64;
                        let k = 1 + big as u16;
                        let c = deg[u].fetch_sub(k, Relaxed) - k;
                        let p = base[u >> sh] as usize + rel[u] as usize + c as usize;
                        unsafe {
                            *out.0.add(p) = v | (w.min(esc as u64) as u32) << vb;
                            if big {
                                *out.0.add(p + 1) = w as u32;
                            }
                        }
                        if v as usize != u {
                            has_in[v as usize >> 6].fetch_or(1 << (v & 63), Relaxed);
                        }
                    }
                }
            });
        }
    });
    drop(deg);
    let has_in = has_in.into_iter().map(|a| a.into_inner()).collect();
    eprintln!("build pass1 {t1:?} prefix {:?} pass2 {:?} sh {sh} slots {total}", t2 - t1, t.elapsed() - t2);
    Graph { edges, base, rel, sh, vb, esc, has_in }
}

/// 4-ary min-heap of (distance, node).
struct Heap(Vec<(u64, u32)>);

impl Heap {
    #[inline(always)]
    fn push(&mut self, d: u64, x: u32) {
        let a = &mut self.0;
        let mut i = a.len();
        a.push((d, x));
        while i > 0 {
            let p = (i - 1) / 4;
            if a[p].0 <= d {
                break;
            }
            a[i] = a[p];
            i = p;
        }
        a[i] = (d, x);
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<(u64, u32)> {
        let a = &mut self.0;
        let last = a.pop()?;
        if a.is_empty() {
            return Some(last);
        }
        let top = a[0];
        let (len, mut i) = (a.len(), 0);
        loop {
            let c = 4 * i + 1;
            if c >= len {
                break;
            }
            let mut m = c;
            for j in c + 1..(c + 4).min(len) {
                if a[j].0 < a[m].0 {
                    m = j;
                }
            }
            if a[m].0 >= last.0 {
                break;
            }
            a[i] = a[m];
            i = m;
        }
        a[i] = last;
        Some(top)
    }
}

const EMPTY: u32 = u32::MAX;
static SETTLES: AtomicU64 = AtomicU64::new(0);
static PUSHES: AtomicU64 = AtomicU64::new(0);

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

/// Per-thread search state: settled bitset with a one-bit-per-word dirty summary, frontier, heap.
struct Search {
    done: Vec<u64>,
    dirty: Vec<u64>,
    front: Frontier,
    heap: Heap,
}

impl Search {
    fn new(n: usize) -> Self {
        let words = n / 64 + 1;
        Search { done: vec![0; words], dirty: vec![0; words / 64 + 1], front: Frontier::new(8192), heap: Heap(Vec::new()) }
    }

    fn run(&mut self, g: &Graph, s: usize, t: usize) -> i64 {
        let mut res = -1;
        let (mut ns, mut np) = (0u64, 0u64);
        self.front.relax(s as u32, 0);
        self.heap.push(0, s as u32);
        while let Some((d, u)) = self.heap.pop() {
            let (w, bit) = (u as usize >> 6, 1u64 << (u & 63));
            if self.done[w] & bit != 0 {
                continue; // stale entry
            }
            if self.done[w] == 0 {
                self.dirty[w >> 6] |= 1 << (w & 63);
            }
            self.done[w] |= bit;
            ns += 1;
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
                    np += 1;
                    self.heap.push(nd, v);
                }
            }
        }
        SETTLES.fetch_add(ns, Relaxed);
        PUSHES.fetch_add(np, Relaxed);
        for &(_, x) in &self.heap.0 {
            self.front.remove(x);
        }
        self.heap.0.clear();
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
    let threads = std::env::var("FW_THREADS").ok().and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |p| p.get()));
    let g = build(map("/data/graph.txt"), threads);
    let t_build = t0.elapsed();
    let n = g.rel.len() - 1;

    let qtxt = map("/data/queries.txt");
    let mut qs = Vec::new();
    let mut j = 0;
    while j < qtxt.len() {
        qs.push((num(qtxt, &mut j) as usize, num(qtxt, &mut j) as usize));
    }

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
        let v = a.load(Relaxed);
        out.extend_from_slice(v.to_string().as_bytes());
        out.push(b'\n');
    }
    std::fs::write("/out/distances.txt", out).expect("write distances.txt");
    eprintln!("phase build {:?} queries {:?} total {:?} settles {} pushes {}", t_build, t_query - t_build, t0.elapsed(), SETTLES.load(Relaxed), PUSHES.load(Relaxed));
}
