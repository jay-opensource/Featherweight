# Track D: Traverse

The task, the input files and the exact output format are in the
[spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md). In short:
read a directed, weighted road-like graph (`graph.txt`: 1,740,512 nodes, 5,000,000 edges, 88 MB of
text) and 2,000 source-target pairs (`queries.txt`), and write each shortest-path length, or -1.

Board result for the shipped code: **0.1337 s, 64.07 MB, 454 lines, 72.15 points**
(run-3e0a39c47769, commit b1046aa; unchanged in bbee872), against 72.69 s and 893 MB for the
reference.

## Approach

Thesis: **minimum memory per unit of speed.** A shortest-path query on a road network is a walk
through memory, so the program spends its effort on making that memory small and local.

1. **A 4-byte edge.** Each edge slot is `target:21 | weight:11` (the split comes from the header's
   n). 99.93% of weights fit 11 bits; a larger weight stores the escape value in the slot and the
   full u32 in the next slot. A node's slots start at a u32 base per block of 16 nodes plus a u8
   offset inside the block, so a node costs about 1.25 bytes of offsets. Both graphs (forward and
   reverse) are 22 MB each.
2. **Parsing without contention.** `graph.txt` is mapped read-only (the page cache is not charged
   to the process) and split into byte ranges; each work item parses the lines that start in its
   range, numbers 8 bytes at a time (SWAR). The CSR is built in buckets of 8,192 nodes: each work
   item counts its slots per bucket, appends slots to its bucket's region, and each bucket is then
   sorted by node in a 94 KB scratch copy. No atomics, and no store lands outside the bucket being
   worked on.
3. **Breadth-first relabelling.** Node ids are a random relabelling of a road lattice, so a
   search on the raw ids jumps all over memory. Each thread grows a breadth-first region from
   its own seed (claiming nodes with a CAS) and the graph is copied into that order; neighbours
   in the network become neighbours in memory. The reverse graph is built from the relabelled one
   with the same builder, into the first graph's edge array, whose pages are already mapped.
4. **Two-ended Dijkstra.** Forward from s and backward from t on the reverse graph, always
   expanding the side with fewer queued entries, stopping when the two queue minimums together
   reach the best meeting length. Over the 2,000 queries this settles 7.15M nodes instead of
   23.2M for a one-ended search. Labels are 31-bit distances (top bit = settled) in per-thread
   pages of 64 node ids, one set of pages and one page directory per side, allocated on first
   touch; BFS order keeps a search inside few pages. Each side's queue is a Dial bucket queue (one
   bucket per distance modulo W = 8192, above the largest main-body weight); an entry carries the
   label index and the node's edge range, so a pop needs no lookup, and the edges are prefetched
   when the node is queued. One search step is generic over the side, so forward/backward choices
   are compile-time constants.
5. **Exact for every weight.** Edges of weight >= W and labels that would need more than 31 bits
   are skipped; the smallest skipped length is kept, and only a query whose answer could be that
   long falls back to a one-ended u64 Dijkstra (10 queries on the full set). Queries from sources
   without in-edges, which always take that slow path, are started first, so none of them runs
   alone at the end.
6. **Memory accounting and exit.** The scoring box runs transparent huge pages "always", which
   charges a whole 2 MB page for the first touched byte of a sparse allocation. Once the searches
   were local, nothing gained from 2 MB pages (measured on the board), so the process opts out
   with one `prctl(PR_SET_THP_DISABLE)`; one malloc arena keeps the per-thread search vectors
   packed. After the answers are written, every thread drops its share of the text mapping and of
   both edge arrays, so the kernel's exit, which runs on one core, has little left to unmap.

The measurements behind each step, the board run IDs and the experiments that lost are in
[PERF.md](PERF.md).

## Build and run

```
docker build -t featherweight/track-d track-d
docker run --rm --network=none --read-only -v "$PWD/data/d:/data:ro" --tmpfs /out featherweight/track-d
```

The binary reads `/data/graph.txt` and `/data/queries.txt` and writes `/out/distances.txt`.
`FW_THREADS` overrides the thread count. `tools/bench.py d --gate G1|G2|G3` builds, runs with the
board's container flags and checks the published hashes (see PERF.md).

## Files

| File | What it is |
|---|---|
| `src/main.rs` | The whole program: parse and CSR builder, BFS relabelling, the two-ended search, the u64 fallback |
| `Cargo.toml` | No dependencies; release profile with fat LTO, one codegen unit, `panic = "abort"` |
| `.cargo/config.toml` | Build flags: `target-cpu=x86-64-v3` (AVX2, BMI2, LZCNT on the i5-13500) and a static binary |
| `Dockerfile` | The board's two-stage Rust build, offline |
| `PERF.md` | Every board run, the measurements behind each choice, and what did not work |

## Dependencies

None (Rust standard library only).

## With another 24 hours

- Build the forward and reverse graphs in one pass over the relabelled order instead of two.
- Better locality orderings than BFS regions (e.g. recursive bisection), if they can be computed
  inside the time budget.
