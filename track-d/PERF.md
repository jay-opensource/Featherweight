# Track D: performance log

Thesis: **minimum memory per unit of speed.** A shortest-path query on a road network is a walk
through memory; the smaller and more local that memory, the faster the walk. Every step below is
one change, measured on the live board (run ID) and, for its mechanism, locally with phase timers.

Local box: i5-11400H laptop (6 cores, 12 MB L3) running Docker; the scoring box is an i5-13500
(24 MB L3, 2.5 GHz fixed). Local times are for A/B decisions only; the run IDs are the record.

## Board runs

| Step | Commit | Change | Run ID | T (s) | M (MB) | S | Points |
|---|---|---|---|---|---|---|---|
| 0 | | The reference, unchanged | | 72.69 | 893 | 43 | 0 |
| 1 | 941a1e3 | Correct baseline: u32 CSR, one Dijkstra per query, binary heap, 12 threads | run-811fef4b3870 | 1.340 | 230.6 | 141 | 45.4 |
| 2 | ecc48d8 | 4-byte edges, u8 offsets, settled bitset + frontier-only map, mmap, 2 parallel passes | run-3145812eecc8 | 0.762 | 43.7 | 427 | 55.1 |
| 3 | a42fac6 | Radix heap; parallel BFS relabelling + copy; in-edge check for query targets only | run-05f2c67e3caf | 0.418 | 77.0 | 601 | 58.1 |
| 4 | ca8691a | Experiment: transparent huge pages off for the whole process | run-8dc1e66e592c | 0.489 | 58.9 | 605 | 57.5 |
| 5 | 57f5d8a | Experiment: THP on, 256 KB stacks, one malloc arena | run-5b075ade658a | 0.420 | 76.7 | 611 | 58.0 |
| 6 | 6a4cfdc | Two-ended Dijkstra; batched fill; allocator keeps 2 MB pages for the graph only | run-3182f2b6b289 | 0.355 | 79.3 | 886 | 58.4 |
| 7 | b7dc4ef | Skip labels above 31 bits instead of abandoning the two-ended search | run-d0196c98deea | 0.248 | 78.5 | 721 | 63.0 |
| 8 | d70cf3a | 8-byte queue entries (distance << 32 \| node), unchecked hot lookups | run-ae48272541b4 | 0.233 | 74.6 | 720 | 64.0 |
| 9 | 7971071 | Software prefetch in the BFS walk and the relabel copy | run-a5a954964edb | 0.226 | 72.4 | 738 | 64.3 |
| 10 | eab717a | Per-side 64-node label pages; expand the side with fewer queued entries | run-8098591c571a | 0.224 | 72.2 | 775 | 64.3 |
| 11 | 5d9967b | One CSR builder and one edge iterator (-120 lines); SWAR number parsing | run-125edba6abfe | 0.200 | 72.0 | 619 | 66.3 |
| 12 | 064a61b | Experiment: every malloc block >= 4 KB in its own mapping | run-e10a2dc56bee | 0.200 | 72.1 | 636 | 66.1 |
| 13 | 2babdea | Bucketed CSR builder: no locked ops, no store outside the current bucket | run-6e37653c1567 | 0.178 | 77.0 | 644 | 67.1 |
| 14 | c697a6e | Experiment: prefetch the offsets of the next node a side will settle | run-1266ccbcec18 | 0.180 | 78.5 | | 66.9 |
| 15 | fe25faa | Prefetch reverted; u16 page numbers in the label directory | run-07372c236cf8 | 0.185 | 72.1 | 649 | 67.0 |
| 16 | e3964b4 | Experiment: reverse graph through atomic countdown cursors instead of the bucketed builder | run-cc347453b71c | 0.208 | 72.2 | 677 | 65.5 |
| 17 | 090bbfd | Dial bucket queue (radix heap replaced; bucket per distance mod W, heap for heavier keys) | run-0f4e0e3ee631 | 0.172 | 77.1 | 726 | 67.1 |
| 18 | 018ba3b | Prefetch a node's edge list when it is queued | run-82974c9e960f | 0.168 | 74.6 | 727 | 67.5 |
| 19 | 2ddf225 | Experiment: the BFS writes the relabelled graph (no separate copy) | run-326de33ed648 | 0.167 | 77.0 | 808 | 67.1 |
| 20 | 2d45234 | Compact Dial queue: 16-byte entries, no overflow heap, edges >= W skipped exactly; step 19 reverted | run-45e743567e28 | 0.166 | 77.8 | 669 | 67.7 |
| 21 | 303ba88 | 626 formatted lines (leaner pages, Dial by Default); escaped edges carry the node's u16 so the bucket finish never reads edges | run-9341674210a3 | 0.161 | 74.1 | 627 | 68.4 |
| 22 | 8cba48c | Search step generic over the side (constants in the hot loop); x86-64-v3 + static binary; exit right after writing | run-efed718978e5 | 0.148 | 70.0 | 631 | 69.6 |
| 23 | cc9faa7 | Edge streams through a trait, so the builder's emitter is inlined into the parse loop | run-910e201fd701 | 0.145 | 76.5 | 633 | 69.4 |
| 24 | a7961c3 | No 2 MB pages for the graph arrays either (only the NOHUGEPAGE advice left) | run-4f9060930961 | 0.142 | 64.3 | 623 | 70.5 |
| 25 | bd5f56e | One prctl(PR_SET_THP_DISABLE) + one malloc arena instead of the custom allocator (-25 lines) | run-39e2f47ac593 | 0.142 | 64.2 | 598 | 70.6 |
| 26 | cdf5158 | Branch-free label check (one signed compare); query side -40 lines; BFS prefetches claim words 4 ahead; calloc'd id/degree arrays | run-f73ce8bfc7b4 | 0.134 | 64.1 | 562 | 71.4 |
| 27 | 64d75ea | Build side in 47 fewer lines (layout returns an edge-less Graph, bfs_order through par(), branch-free store) | run-fb48d5373f7f | 0.136 | 64.3 | 515 | 71.5 |
| 28 | b4be5d5 | 33 more lines off both sides (tagged spans(), checked_shl in num, Pages literal, shorter pop) | run-0ca006a8a756 | 0.145 | 63.8 | 482 | 71.1 |

## How it works now

1. **Parse** `graph.txt` from an `mmap` (page cache, not charged to us) in two parallel passes:
   count each node's slots, then fill them through per-node countdown cursors.
2. **Relabel** nodes in breadth-first order: each thread grows its own region from a seed and
   claims nodes with a CAS; then the graph is copied into the new order. Neighbours in the road
   network become neighbours in memory.
3. **Reverse graph** built from the relabelled one, for the backward search.
4. **Queries**, 12 threads pulling from an atomic counter: two-ended Dijkstra, forward from s and
   backward from t, expanding the side with fewer queued entries, stopping when the two queue
   minimums together reach the best meeting length mu.

### Memory layout (n = 1,740,512, m = 5,000,000)

| Component | Bytes | Why this size |
|---|---|---|
| Edge slot | 4 B: `target:21 \| weight:11` | 99.93% of weights fit 11 bits; a larger weight puts the escape value 2047 in the slot and the full u32 in the next slot (3,496 such edges) |
| Node offset | 1 B + 1/16 of a u32 | u32 base per block of 16 nodes + u8 offset inside the block (max in-block offset measured: 61) |
| Forward + reverse graph | 2 x 22.2 MB | |
| Labels, per thread | pages of 64 node ids, per side, from a pool | BFS order keeps a search inside few pages; measured pool over 12 threads: about 9 MB |
| Queue, per thread | radix heap, 8-byte entries | about 3.5 MB over 12 threads |

## The steps, with what each one taught

**Step 2: the graph is the memory.** The baseline held 60 MB of edge lists plus a 14 MB distance
array per thread. A settled node needs one bit, and only the frontier (measured max 3,375 nodes)
needs a distance, so per-thread state fell from 14 MB to about 0.4 MB. 230.6 MB -> 43.7 MB.

**Step 3: memory latency, not instructions.** One thread spent 260 ns per settled node; the node
ids are a random relabelling, so every step of the search jumped to a far place in 22 MB.
- Radix heap (Dijkstra keys are monotone, push is O(1)): single-thread query phase 2.81 s -> 1.65 s.
- BFS relabelling: query phase 520-630 ms -> 235-280 ms (12 threads).

**Steps 4-6: why the board said 77 MB when the laptop said 59 MB.** The scoring box runs
transparent huge pages "always" (Debian 13); the laptop runs "madvise". memory.peak charges a whole
2 MB page for the first touched byte of a sparse allocation. Step 4 switched THP off: 58.9 MB, but
0.489 s instead of 0.418 s, because the graph lost its 2 MB pages too (TLB misses). Step 5 (small
stacks, one arena) changed nothing: 76.7 MB. Step 6 marks every allocation >= 256 KB
MADV_NOHUGEPAGE and opts only the graph arrays back in: board memory matched local again.

**Step 6: batching around locked instructions.** A locked x86 instruction waits for every earlier
store to land. The fill did one `fetch_sub` and one scattered store per edge, so the stores could
never overlap their misses. Taking 16 cursors first and then doing 16 stores: fill 64-82 ms ->
45-50 ms.

**Step 7: a bug that the cheaper fallback exposed.** Two-ended labels are 31 bits. The first
version abandoned the two-ended search whenever a label would not fit, and redid the query
one-ended. The backward search meets one of the 34 weight-2^32 edges out of the no-in-edge nodes in
about half of the large queries, so 5.0M of the 11.3M settled nodes were fallback work. Skipping
such a label keeps the search exact for answers below 2^31 (every path through it is longer);
only 10 queries on the full set still fall back. Single-thread query phase 0.90 s -> 0.64 s.

**Step 11: parsing is arithmetic too.** On a 2.5 GHz box, two passes over 88 MB of text cost real
CPU. The SWAR parse loads 8 bytes, finds the first non-digit with one add and mask, shifts the
digits to the top (the empty low bytes act as leading zeros) and combines pairs, quads and octets
with three multiplies. Local build phase about 149 -> 138 ms; checked against a scalar parse on
200,000 random numbers.

**Step 13: keep every store near the last one.** The builder did one random locked update and
one random store into a 20 MB array per edge. Now each work item counts its slots per bucket of
8,192 nodes (213 counters, no atomics), appends each slot to its bucket's region (213 sequential
streams per thread, with the node's place in the bucket in a u16 side array), and each bucket is
then sorted by node through a 94 KB scratch copy. Local build 133-152 -> 108-114 ms, board
0.200 -> 0.178 s. The u16 side array costs about 5 MB while building.

**Steps 17-23: the queue and the last microseconds.** A Dial queue (one bucket per distance
modulo W = 4096, weights in the main body are below ~8,100) beat the radix heap by 3.5% on the
board but added 82 lines, which cost exactly what it gained; the compact version (16-byte entries,
heavy edges skipped with a recorded lower bound, see the proof in `bidir`) kept the speed at 626
lines. Making one search step generic over the side (forward/backward are compile-time constants)
cut instructions per query by 13%. Process exit was 13 ms of the local wall time: the kernel unmaps
everything on one core after the last write, and freeing the big arrays one by one came before
that; `std::process::exit(0)` right after writing the answers removed about 5 ms of it.

**Step 24: 2 MB pages stopped paying.** Step 4 measured +17% time without 2 MB pages, when the
search jumped across 22 MB of randomly numbered nodes. After BFS relabelling the searches are
local, and without 2 MB pages the board ran faster (0.145 -> 0.142 s) and 12 MB lighter (76.5 ->
64.3 MB). With nothing left that wants them, a 25-line allocator became one prctl call; per-thread
malloc arenas still cost ~10 MB locally, so one arena stays.

**Pull-transpose for the reverse graph (rejected).** In-edges of u can be found by scanning the
out-edges of u's neighbours (97% of edge pairs are two-way), with no atomics and no u16 side
array: exact, and 5 MB lighter, but the reverse phase took 128 ms instead of 33 ms on 12 threads,
because every pulled edge reads a neighbour's adjacency (88% of edges span 64 or more ids).

**Locality does not depend on the number of BFS regions.** Label pages touched over all queries,
counted deterministically: 1 region 362,199; 4 regions 362,355; 12 regions 363,088; 24 regions
370,259; 48 regions 366,506. The parallel BFS costs no locality.

**Settled nodes over all 2,000 queries:** one-ended 23.2M; two-ended 7.38M; expanding the side
with fewer queued entries instead of the smaller key, 7.15M.

## Failure museum (measured, then rejected)

| Idea | Expected | Measured | Why it lost |
|---|---|---|---|
| No frontier map: push every improvement, drop stale entries on pop | fewer hash operations | 1.49 pushes per settle instead of 1.14; slower with the 4-ary heap (3.04 s vs 2.81 s) and with the radix heap (2.00 s vs 1.78 s) | the queue work grew more than the hashing shrank |
| Rebuild the relabelled graph from the text instead of copying it | 45 MB instead of 59 MB peak | refill 130-150 ms vs copy 28 ms | two random lookups per line into the 7 MB id table |
| Per-thread private counters instead of atomic ones | no locked ops | count 22 -> 80-104 ms, fill 45 -> 106-134 ms | 12 private 3.5 MB arrays (42 MB) thrash L3 and must be zero-faulted; one shared array stays cached |
| `MAP_POPULATE` on graph.txt | fewer page faults during parsing | build phase equal or worse | the population runs on one thread before the parallel passes |
| THP off for the whole process (step 4) | match local memory | 58.9 MB but +17% time | the graph needs its 2 MB pages |
| Two-ended labels in two hash tables (step 6) | | 101 ns per settled node | replaced by paged labels: 80 ns |
| Pages of 128 ids holding both sides | one lookup for both labels | pool 17.4 MB | the two balls rarely share pages; per-side 64-id pages: about 9 MB |
| Every malloc block >= 4 KB in its own mapping (step 12) | remove the board's +10 MB | 72.1 MB, unchanged | the gap is not malloc's heap; it has been about +10 MB since step 2, so it looks like the box's own accounting |
| Pruning in the two-ended search (skip labels with nd + other side's minimum >= mu) | fewer pushes near the meeting point | query phase unchanged within noise | one more lookup on the other side for every edge cost what the pruning saved |
| Queries sorted by source id | the 12 threads search nearby parts of the map, warm L3 | no gain (228-282 vs 254-286 ms) | |
| Both sides' directory entries interleaved in one cache line | one line for both lookups | single-thread queries 512-567 vs 493-610 ms | each side now gets half as many pages per line |
| 6 or 8 threads instead of 12 | more L2 per search | 12 threads fastest | the search waits on memory, and hyper-threads hide that |
| Prefetch the next node a side will settle (step 14) | overlap its offset miss with the current edge loop | board 0.180 s vs 0.178 s | the next pop is already likely in cache; the extra loads cost as much |
| The BFS writes the relabelled graph directly (step 19) | skip the copy | -0.9% time, +2.4 MB, +81 lines: net loss | the copy is cheap once prefetched; the fused version needed more code than it saved |
| 256-node buckets with a u8 side array in the CSR builder | -2.7 MB | M 63.4 -> 60.7 MB, but text fill 15.5 -> 31.4 ms, reverse fill 8.6 -> 17.6 ms | ~6,800 append streams per work item thrash the write path |
| Serial layout() | -5 lines | ~5-6 ms per call x 3 calls on one thread | the parallel version is 1-2 ms per call |
| Fixed M_MMAP_THRESHOLD of 1 MB | freed blocks return to the OS | M 67.1 -> 64.1 MB locally, reverse phase min 25 -> 32 ms | fresh mappings fault on every reuse |
| Reverse graph through atomic countdown cursors (step 16) | 5 MB less than the bucketed builder's u16 side array | board 0.208 s vs 0.178 s, 72.2 vs 77.0 MB | on the scoring box the locked updates cost far more than the memory saved |

## Correctness

- Before every push: the worked example, the four tiny seeds, the four sample seeds and full
  seed 1 all match the published sha256.
- Concurrency: full seed 1 with 1, 2, 7 and 12 threads (`FW_THREADS`).
- Distances above 31 bits (from a no-in-edge node, the max-weight and over-uint32 island queries)
  go through a one-ended u64 Dijkstra.
- Nothing depends on the seed: the bit split comes from the header n, the offset block size
  shrinks if a block ever needs more than 255 slots, and degrees above 65k stop with a message.
