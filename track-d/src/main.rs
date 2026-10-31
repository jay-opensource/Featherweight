// Track D baseline: plain CSR + Dijkstra, one search per query, queries spread over all threads.
// This is ladder step 1-2 (correct first); later steps shrink the graph and the per-thread state.
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

fn num(b: &[u8], i: &mut usize) -> u64 {
    let mut x = 0u64;
    while *i < b.len() && b[*i].is_ascii_digit() {
        x = x * 10 + (b[*i] - b'0') as u64;
        *i += 1;
    }
    *i += 1; // the space or LF after the number
    x
}

fn main() {
    let t0 = Instant::now();
    let g = std::fs::read("/data/graph.txt").expect("read graph.txt");
    let qtxt = std::fs::read("/data/queries.txt").expect("read queries.txt");

    let mut i = 0;
    let n = num(&g, &mut i) as usize;
    let m = num(&g, &mut i) as usize;
    let (mut eu, mut ev, mut ew) = (Vec::with_capacity(m), Vec::with_capacity(m), Vec::with_capacity(m));
    for _ in 0..m {
        eu.push(num(&g, &mut i) as u32);
        ev.push(num(&g, &mut i) as u32);
        ew.push(num(&g, &mut i) as u32);
    }
    drop(g);

    // CSR: off[u]..off[u+1] are u's out-edges.
    let mut off = vec![0u32; n + 1];
    for &u in &eu {
        off[u as usize + 1] += 1;
    }
    for k in 0..n {
        off[k + 1] += off[k];
    }
    let mut cur = off[..n].to_vec();
    let (mut tgt, mut wt) = (vec![0u32; m], vec![0u32; m]);
    for e in 0..m {
        let u = eu[e] as usize;
        let p = cur[u] as usize;
        cur[u] += 1;
        tgt[p] = ev[e];
        wt[p] = ew[e];
    }
    drop((cur, eu, ev, ew));
    let mut has_in = vec![false; n];
    for &v in &tgt {
        has_in[v as usize] = true;
    }
    let t_build = t0.elapsed();

    let mut qs = Vec::new();
    let mut j = 0;
    while j < qtxt.len() {
        let s = num(&qtxt, &mut j) as u32;
        let t = num(&qtxt, &mut j) as u32;
        qs.push((s, t));
    }

    let answers: Vec<AtomicI64> = (0..qs.len()).map(|_| AtomicI64::new(-1)).collect();
    let next = AtomicUsize::new(0);
    let threads = std::env::var("FW_THREADS").ok().and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |p| p.get()));
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let mut dist = vec![u64::MAX; n];
                let mut touched: Vec<u32> = Vec::new();
                let mut heap = BinaryHeap::new();
                loop {
                    let k = next.fetch_add(1, Relaxed);
                    if k >= qs.len() {
                        break;
                    }
                    let (s, t) = (qs[k].0 as usize, qs[k].1 as usize);
                    let ans = if s == t {
                        0
                    } else if off[s] == off[s + 1] || !has_in[t] {
                        -1
                    } else {
                        let mut res = -1;
                        dist[s] = 0;
                        touched.push(s as u32);
                        heap.push(Reverse((0u64, s as u32)));
                        while let Some(Reverse((d, u))) = heap.pop() {
                            let u = u as usize;
                            if d > dist[u] {
                                continue; // stale entry
                            }
                            if u == t {
                                res = d as i64;
                                break;
                            }
                            for e in off[u] as usize..off[u + 1] as usize {
                                let v = tgt[e] as usize;
                                let nd = d + wt[e] as u64;
                                if nd < dist[v] {
                                    if dist[v] == u64::MAX {
                                        touched.push(v as u32);
                                    }
                                    dist[v] = nd;
                                    heap.push(Reverse((nd, v as u32)));
                                }
                            }
                        }
                        for &x in &touched {
                            dist[x as usize] = u64::MAX;
                        }
                        touched.clear();
                        heap.clear();
                        res
                    };
                    answers[k].store(ans, Relaxed);
                }
            });
        }
    });
    let t_query = t0.elapsed();

    let mut out = String::with_capacity(qs.len() * 12);
    for a in &answers {
        out.push_str(&a.load(Relaxed).to_string());
        out.push('\n');
    }
    std::fs::write("/out/distances.txt", out).expect("write distances.txt");
    eprintln!("phase build {:?} queries {:?} total {:?}", t_build, t_query - t_build, t0.elapsed());
}
