# Track D: Traverse

The task, the input files and the exact output format are in the
[spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md). In short:
read a directed, weighted road-like graph (`graph.txt`: 1,740,512 nodes, 5,000,000 edges, 88 MB of
text) and 2,000 source-target pairs (`queries.txt`), and write each shortest-path length, or -1.

Board result for the shipped code (0e4fb56, D code 7c24aa3): **0.11701 s, 60.48 MB, 459 lines, 73.81 points**
(run-8d7aa56daed0; the D8 code d7c421b ran 0.11637 s / 59.89 MB / 73.75, run-0c5c2a4acdd4), the best of our runs. The reference takes 72.69 s and 893 MB. Before the last
round the same build ran 0.120-0.126 s at 62.7 MB (best run-0706657d930f, 73.39 points).

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
   range, numbers 8 bytes at a time (SWAR). The CSR is built in buckets of 8,192 nodes, and the text
   is parsed once: each work item appends slots to its bucket's region, with room for its expected
   share plus 6 standard deviations (a segment that overflows restarts the build with an exact
   counting pass). Each bucket is then finished on its own: offsets are laid out from the slots' u16
   places and the slots are sorted by node in a 94 KB scratch copy. No atomics, and no store lands
   outside the bucket being worked on.
3. **Breadth-first relabelling in one pass.** Node ids are a random relabelling of a road lattice,
   so a search on the raw ids jumps all over memory. Each thread grows a breadth-first region from
   its own seed, claiming nodes with one bit each in a 218 KB bitset. A node is dequeued with its edge
   list already in cache, so the walk gives it its new id (region and rank) and copies its slots with
   one `memcpy` to the end of the region's segment of a scratch array, keeping one count byte per
   node. A bucket pass then lays out the offsets and moves the slots into the first graph's mapped
   edge array with every target rewritten, and counts the reverse graph's slots on the way. This
   replaced a relabel builder whose 30 MB of staging were the memory peak (62.7 -> 59.9 MB). The
   reverse graph is built from the result by the same bucketed builder into a fresh array.
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
   long falls back (10 queries on the full set). The fallback, `via`, takes the minimum over the
   source's out-edges of w + a two-ended search from the head, each capped by a bound that grows
   round by round; a one-ended u64 Dijkstra is left for searches that give up again. Queries are
   pulled farthest pair first (|s - t| in BFS ids), so the long searches start early.
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
| `src/main.rs` | The whole program: parse and CSR builder (`csr`), BFS relabelling (`relabel`), the two-ended search (`bidir`), `via` and the u64 fallback |
| `Cargo.toml` | No dependencies; release profile with fat LTO, one codegen unit, `panic = "abort"` |
| `.cargo/config.toml` | Build flags: `target-cpu=x86-64-v3` (AVX2, BMI2, LZCNT on the i5-13500) and a static binary |
| `Dockerfile` | The board's two-stage Rust build, offline |
| `PERF.md` | Every board run, the measurements behind each choice, and what did not work |

## Dependencies

None (Rust standard library only).

## With another 24 hours

- Fit the reverse graph's staging into pages the BFS already faulted in (chunked segments: reverse
  kernel cycles 54M -> 22M, about +12 lines; the reverse build now writes fresh memory).
- 3-byte slots: -7.7 MB measured, but +16-22% time with a direct-copy relabel. It would need the
  relabel staged as per-node runs with escaped edges in per-bucket lists (estimated -7 MB at the
  peak, +35-45 lines, about +0.1 points net).
- Interleave the u32 block base and the u8 offsets in one cache line, so `off(v)` is one miss
  instead of two (about 25% of query samples); a plain 32-byte record was slower, so it needs a
  layout that keeps the base array cache-resident.
- Better locality orderings than BFS regions (e.g. recursive bisection), if they can be computed
  inside the time budget.
