# Track D: performance log

Thesis: **minimum memory per unit of speed.** A shortest-path query on a road network is a walk
through memory; the smaller and more local that memory, the faster the walk. Every step below is
one change, measured on the live board (run ID) and, for its mechanism, locally with phase timers.

Local box: i5-11400H laptop (6 cores, 12 MB L3) running Docker; the scoring box is an i5-13500
(24 MB L3, 2.5 GHz fixed). Local times are for A/B decisions only; the run IDs are the record.

## Result

| | Run ID | Commit | T | M | S | Points |
|---|---|---|---|---|---|---|
| Reference (Python) | | | 72.69 s | 893 MB | 43 | 0 |
| First correct entry | run-811fef4b3870 | 941a1e3 | 1.340 s | 230.6 MB | 141 | 45.37 |
| Before the final rounds (b1046aa) | run-3e0a39c47769 | b1046aa | 0.1337 s | 64.07 MB | 454 | 72.15 |
| **Best, and the shipped code** | **run-ad2a7cbc4bf1** | **b7db2f2** | **0.1223 s** | **62.39 MB** | **454** | **73.23** |
| Earlier best (before D6) | run-b28ea11afb16 | 96c67ed | 0.1253 s | 62.50 MB | 458 | 72.93 |
| The same build code, other runs | run-665c3b98b9ee, run-1a26c84a5351, run-29a3a688b9e1 | 48198af, b5f8df8, a8fb336 | 0.1331, 0.1361, 0.1402 s | 63.28, 62.68, 62.52 MB | 430 | 72.43, 72.23, 71.92 |

The best run is 580 times faster than the reference with 14.3 times less memory; the shipped code, 572 times and 14.3 times. Points are the
board's: `50·log10(K)` with K = (T_ref/T)^0.5 × min(50, M_ref/M)^0.2 × min(1, S_ref/S)^0.15. At this
operating point 1% of T is worth about 0.11 points, 1% of M about 0.04, and 10 lines about 0.07.

## Board runs

"Same D code" rows re-ran unchanged D code (another track had changed); they sample the noise.

<!-- New runs: append a row with the next step number, and update the Result table above. -->

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
| 29 | bb33a01 | Same D code as step 28 (documentation change): a second sample of the board's noise | run-9e4cb2237b67 | 0.140 | 64.1 | 482 | 71.4 |
| 30 | 8c1f26b | plain() on a BTreeSet that stops at t; exact empty output; bidir verdict in one line | run-adf13f996262 | 0.136 | 64.8 | 477 | 71.7 |
| 31 | c8c57cd | Same D code as step 30 | run-e8593678e368 | 0.138 | 63.4 | 477 | 71.7 |
| 32 | 5b3321f | 00480a6: pick the side with a match on the length test (one line fewer, instructions 1.000) | run-0a78b7adb065 | 0.137 | 64.2 | 476 | 71.7 |
| 33 | b1f2ed7 | Same D code as step 32 | run-2b9a8fa8b15f | 0.138 | 64.5 | 476 | 71.6 |
| 34 | 1803e34 | 19 fewer lines (c198d4e: BFS instructions 0.966, the rest 1.000); text work items are plain byte ranges, no `chunks()` | run-be28cd019adf | 0.139 | 64.2 | 450 | 71.7 |
| 35 | 79c2804 | Same D code as step 34 | run-0bcb81b96186 | 0.144 | 63.4 | 450 | 71.4 |
| 36 | 65ff880 | Experiment: parse graph.txt once (estimated room per segment, exact retry); reverted | run-8ae511632bee | 0.149 | 64.2 | 457 | 70.9 |
| 37 | b1478c2 | Experiment: each thread faults in its own slice of the random-write arrays first; reverted | run-1a45246e7ac5 | 0.140 | 64.8 | 464 | 71.6 |
| 38 | a50cee9 | Both experiments reverted (the step-34 code) | run-924782b8b087 | 0.140 | 64.1 | 450 | 71.7 |
| 39 | d163280 | Same D code as step 34 | run-a0fc8314797f | 0.141 | 63.4 | 450 | 71.7 |
| 40 | b1046aa | Text and both graphs released on all threads before exit; queries from sources without in-edges first; one label directory per side; the reverse graph reuses g0's edge array; the search loop steps inside its condition | run-3e0a39c47769 | 0.1337 | 64.1 | 454 | 72.15 |
| 41 | 477f874 | Same D code as step 40 | run-2e5b37934e90 | 0.1337 | 64.7 | 454 | 72.11 |
| 42 | bbee872 | Same D code as step 40 (the shipped commit) | run-74ad01ee4c63 | 0.1432 | 64.9 | 454 | 71.35 |
| 43 | 8a67e30 | Same D code as step 40 (documentation change) | run-8b3afbc8368f | 0.1344 | 64.4 | 454 | 72.07 |
| 44 | 48198af | Build rework (a9974c9): per-bucket offsets in the builder's finish, relabelled graph built by the same builder (no copy, no degree arrays, no 65k degree limit), builder split so the ids are freed first | run-665c3b98b9ee | 0.1331 | 63.3 | 430 | 72.43 |
| 45 | 9477275 | Same D code as step 44 (another track changed), re-run at 10:29 UTC | (run ID not recorded: the board JSON lists only best and latest runs) | 0.1327 | 62.5 | 430 | 72.51 |
| 46 | b5f8df8 | Same D code as step 44 | run-1a26c84a5351 | 0.1361 | 62.7 | 430 | 72.23 |
| 47 | a8fb336 | Same D code as step 44, on a slow stretch of the box (unchanged C code ran 31.8 ms instead of 29-30 ms in the same push) | run-29a3a688b9e1 | 0.1402 | 62.5 | 430 | 71.92 |
| 48 | 96c67ed | Text parsed once with overflow restart; BFS claims in a bitset; helper inlining (7f21d9f) + far pairs first and the capped `via` fallback (D4) | run-b28ea11afb16 | 0.1253 | 62.5 | 458 | 72.93 |
| 49 | d9663e9 | 18 fewer lines (b764d34): `store`, `Dial::min`/`clear`, `Pages::clear` inlined into their one caller, the query loop a while-let; behaviour unchanged | run-14368b934a65 | 0.1270 | 62.4 | 440 | 72.92 |
| 50 | b7db2f2 | D6 (a604076): the relabelled build counts slots per node from g0's offsets; the reverse graph's counts are collected per 8192-node bucket in the relabel finish (nb x nb table, 181 KB), so the reverse count pass is gone. Local: instructions 0.964, user cycles 0.969 | run-ad2a7cbc4bf1 | 0.1223 | 62.4 | 454 | 73.23 |

## How it works now

1. **Parse once** `graph.txt` from a read-only `mmap` (the page cache is not charged to us). The
   text is split into 48 byte ranges (threads x 4); each work item parses the lines that start
   inside its range, numbers 8 bytes at a time (SWAR), and appends each slot to its bucket's region
   (buckets of 8,192 nodes) with the node's place in the bucket in a u16 side array. The text is
   read once: each (item, bucket) segment gets room for its share of the header's m plus 6 standard
   deviations, the unused room is holes (place 0), and a segment that would overflow restarts the
   build with an exact counting pass. No atomics, and no store outside the bucket being worked on.
   Then each bucket is finished on its own: count its nodes' slots from the u16s, lay the offsets
   out from the bucket's first slot (blocks of 16 nodes, falling back to smaller blocks if an
   in-block offset does not fit in a byte) and reorder the slots through a 94 KB scratch copy,
   packing in place and waiting only for earlier buckets whose staged slots it overwrites. There is
   no degree array and no separate layout pass.
2. **Relabel** nodes in breadth-first order: each thread grows its own region from a seed and
   claims nodes with one bit in a 218 KB bitset (the 7 MB id array is written once, at the end, so
   the walk no longer faults it in with a CAS per node). The BFS prefetches offsets 16 nodes ahead
   and edge lists 8 ahead. The relabelled graph is not copied: it is built by the same bucketed
   builder from g0, with edges read as `id[u] -> id[v]` (the id lookups are prefetched in the fill
   pass only). Neighbours in the road network become neighbours in memory.
3. **Reverse graph**, built from the relabelled one with the same builder, written into g0's
   already-mapped edge array (it has exactly as many slots). The builder is split into count/fill
   and finish so that the ids and g0's offsets are freed before the relabelled finish reuses that
   heap.
4. **Queries**, 12 threads pulling from an atomic counter, in order of |s - t| (BFS ids)
   descending, so the long searches start early: two-ended Dijkstra, forward from s and backward
   from t, expanding the side with fewer queued entries, stopping when the two queue minimums
   together reach the best meeting length mu. A query that bidir gives up on (its answer could need
   a skipped edge or more than 31 bits; a source without in-edges leaves through edges near 2^32)
   goes to `via`: the minimum over the source's out-edges (s, v, w) of w + d(v, t), each by bidir
   with a cap that grows round by round, so no sub-search grows past what the bound allows. A
   one-ended u64 Dijkstra (`plain`) remains only for sub-searches that give up again.
5. **Exit.** The answers are written, then every thread drops its share of the text mapping and of
   both edge arrays (`MADV_DONTNEED`), and the process exits without freeing anything else.

### Memory layout (n = 1,740,512, m = 5,000,000)

| Component | Bytes | Why this size |
|---|---|---|
| Edge slot | 4 B: `target:21 \| weight:11` | 99.93% of weights fit 11 bits; a larger weight puts the escape value 2047 in the slot and the full u32 in the next slot (3,496 such edges) |
| Node offset | 1 B + 1/16 of a u32 | u32 base per block of 16 nodes + u8 offset inside the block (max in-block offset measured: 61) |
| Forward + reverse graph | 2 x 22.2 MB | |
| Labels, per thread | pages of 64 node ids per side from a pool, found through a u16 page number per 64 ids (one directory per side) | BFS order keeps a search inside few pages; measured pool over 12 threads: about 9 MB |
| Queue, per thread and side | Dial queue: 8,192 bucket heads (32 KB) and a pool of 16-byte entries [label index, first edge slot, end slot, next] that reuses popped entries | a pop starts on the label and the edges without a lookup |
| Build only | a u16 bucket position per slot (about 10 MB, plus holes: the text staging is about 27% larger than the final slot count), the 7 MB id array | freed before the queries; the peak is in the text phase (about 41 MB) and the relabel finish |

### Where the time goes

Local, 12 threads, on the loaded laptop, from the commits that measured each phase:

| Phase | Time | Measured at |
|---|---|---|
| Text parse + CSR | 22.1 ms (min of 30 in-process rounds) | 1803e34 |
| Relabel copy | 22.5 ms (median) | a68518e |
| Reverse CSR | 10.8 ms (min of 30) | 1803e34 |
| Whole build: parse, BFS, copy, reverse | 97.2 ms (median) | a68518e |
| Whole build after steps 44-48 | about 65 ms (77 ms at a9974c9 minus 12.4 ms) of a 107.7 ms process; at step 42 it was 89 of 137 ms | 7f21d9f |
| Queries | 56.0 ms (308.1 ms on one thread) | after step 30 |
| From the write to process end | 3.6 ms (median; 8.5 ms before step 40) | f29f0fa |

The board's T for the shipped code is 0.134-0.143 s.

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
modulo W = 4096, with a heap for heavier keys) beat the radix heap by 3.5% on the board but added
82 lines, which cost exactly what it gained. The compact version of step 20 kept the speed at 626
lines: W = 8192, above the largest main-body weight (about 8,100), 16-byte entries, no heap, and
edges of weight W or more skipped with a recorded lower bound (see the proof in `bidir`). Making one search step generic over the side (forward/backward are compile-time constants)
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

**Step 34: fewer lines, same instructions.** c198d4e cut 19 formatted lines with the same code
paths (perf counters against 00480a6: BFS instructions 0.966, every other phase 1.000). 1803e34
made the text work items plain byte ranges: an item parses the lines that start inside its range,
so the separate `chunks()` pass is gone (text-phase instructions 0.996). 469 → 443 lines.

**Step 40: the last milliseconds were at the edges of the run.** Five changes, each measured
locally as a whole-process A/B (12 threads, ABBA rounds in one container), shipped together:
- **Exit on all threads.** The kernel's exit unmaps every page of the process on one core. A
  timed serial `madvise` took 10.7 ms for the 92 MB text mapping and 3.2 + 2.6 ms for the two edge
  arrays. Now every thread drops its share of those three regions (`MADV_DONTNEED`) right after the
  answers are written: from the write to process end 8.45 → 3.62 ms (median of 16 rounds, paired
  ratio 0.44), process 147.3 → 140.3 ms (paired 0.977), every earlier phase unchanged.
- **Slow queries first.** A source without in-edges leaves through edges near 2^32, so its query
  takes the one-ended u64 search: 1.2 and 0.6 ms on seed 1, but 25-90 ms and 10-23 ms on holdout
  seed 11. If such a query comes late in the file, it runs alone at the end of the query phase. The
  queries are now pulled with those sources first (the answers stay in file order). On seed 11
  with those two queries moved to the end of the file: query phase 74.8 → 67.0 ms (median, paired
  0.83); seed 1 neutral (paired 0.97, instructions 1.000).
- **One label directory per side.** With both sides in one array, every backward lookup
  recomputed the half offset from the array's length. Query side: instructions 0.986, cycles
  0.980, thread CPU time 0.912.
- **The reverse graph takes over g0's edge array.** It has exactly as many slots, so the 20 MB
  already mapped are reused instead of unmapped on the main thread (1.6 ms) and faulted in again.
  Copy phase 24.70 → 22.52 ms (paired 0.892), build 100.3 → 97.2 ms (paired 0.964), memory.peak
  unchanged (64.0-65.2 MB both).
- **The search loop steps inside its condition** (no `go` flag): query instructions 0.981,
  cycles 0.982.

Board: 0.1337 s (run-3e0a39c47769), against 0.139-0.144 s for the step-34 code in four runs
(steps 34, 35, 38, 39); the same code ran 0.1337 s again at step 41.

**Steps 44-49: the build was two thirds of the run.** A phase budget of the step-42 code (laptop,
12 threads, median of 8-12 interleaved runs, quiet machine) put the build at about 89 ms of 137,
the queries at 42 ms, and kernel cycles at 14% of all cycles; 6-8% of all cycles sat in the
page-table spinlock (`do_anonymous_page`), mostly from the BFS id array and the two u16 degree
arrays, which two threads fault twice per page (read, then write). The changes, each decided on
interleaved counters:
- **Offsets per bucket, relabel through the builder (a9974c9, step 44).** The bucket's finish
  counts slots from the u16s and lays offsets out from its first slot; the degree array, the
  parallel layout pass and the serial prefix are gone. The relabelled graph comes from the same
  builder (id[u] -> id[v], id lookups prefetched in the fill pass only: prefetching both passes made
  the count pass +110M cycles, the fill pass -245M), so `relabel()` and the copy are deleted. The
  builder is split into count/fill and finish so the ids and g0's offsets are freed before the
  relabel's finish takes their memory. Process median 133.1 -> 121.8 ms (paired 0.924 for the
  fused layout alone); kernel cycles -37%, faults 25.6k -> 22.4k, instructions +3% (the relabel
  parses g0 twice). memory.peak 64.1 -> 63.0 MB; without the split it was 66.8 MB. Lines 447 -> 423.
  Side effect: the 65k-degree limit is gone (the old code panicked at degree 70,000).
- **BFS claims in a bitset (6ebced5).** Claiming nodes with a CAS on the 7 MB id array made every
  first claim a page fault under the page-table lock. One bit per node (218 KB, cache-resident)
  claims them, and the ids are written once at the end: BFS walk 24.6 -> 14-17 ms, user cycles
  -17%, kernel -76%.
- **Text parsed once (2df4a10, eae7387).** The same idea as the failed step 36, with the staging
  made cheaper: holes are place 0 (places are 1-based), the finish packs in place and waits only for
  earlier buckets whose staged slots it overwrites, and the overflow check runs only when room was
  estimated. Text build 25.6-27.3 -> 19.5-21 ms. Over a9974c9: process median 120.1 -> 107.7 ms
  (paired 0.899), user cycles -8.3%, kernel -17%; memory.peak unchanged (the text phase peaks at
  about 41 MB).
- **Far pairs first, `via` (D4, e79006f, 5903b96).** Queries are pulled by |s - t| in BFS ids,
  descending (the correlation with query time is 0.48): 40 alternating in-process pairs per seed,
  paired 0.981, 0.989, 0.978 on seeds 1, 11 and 12. This replaced the "sources without in-edges
  first" order of step 40, using the same two lines. `via` replaces the fallback to a one-ended
  search for those sources (step 40's failure-museum row said splitting was a loss; the capped
  rounds fix why): seed 11's fallback CPU 21.6 -> 2.5 ms, and it removes a tail risk (a rank-131k
  no-in query took 25-90 ms one-ended). Neutral on seed 1. Board, steps 44-48: 0.1253 s at 96c67ed.
- **Line cuts (b764d34, step 49).** 451 -> 433 lines, S 458 -> 440 (worth about 0.1 point), by
  inlining helpers with one caller; user cycles 0.998, instructions 1.006, query cycles 0.997.
Gates for each: the worked example, tiny, sample and full seeds at 1/2/7/12 threads, holdout seeds 11
and 12, and 12 adversarial graphs (fallback to smaller blocks and to the counting pass, degree
70,000, n = 8192/8193/16385, weights near W and above 2^31, a wrong header m, n = 1, an empty
graph) at 1/3/12 threads; D4 also passed a 300-graph, 120,000-query randomized self-test of `via`.

**Board noise.** Identical D code gave 0.1447 / 0.1405 s (steps 28-29), 0.1364 / 0.1381 s
(30-31), 0.1368 / 0.1378 s (32-33), 0.1394 / 0.1441 / 0.1402 / 0.1409 s (34, 35, 38, 39) and
0.1337 / 0.1337 / 0.1432 s (40-42) and 0.1344 s (43); the step-44 build code gave 0.1331 / 0.1327 / 0.1361 / 0.1402 s (44-47), the last while unchanged C code also ran 8% slow. A single run cannot resolve changes under about 4%, and one in
three runs is off by 7%. So query-side changes were decided on hardware counters (perf user-space
cycles, instructions and branch misses in docker, interleaved A/B), which reproduce within a few
percent on a loaded machine, and then confirmed on the board.

**Where the query phase went (interleaved counters, 1 thread, each against the previous commit).**
The radix heap made 32.5M redistributions per pass (3.76 per push); the Dial queue: CPU 0.846.
Prefetching a node's edges when it is queued: CPU 0.844 (1 thread), 0.923 (12 threads); after
it a warm rerun of the same query costs 89% of a cold one, so the loop is compute-bound. 16-byte
queue entries [label index, first slot, end slot] let a pop read the label and the edges without
any lookup: ~0.96. The step generic over the side: instructions 0.866, cycles 0.955 (the runtime
side index cost cmov selection and register spills). BMI2: instructions 0.900, cycles 0.910. One
signed compare for "settled or no better": branch misses 0.875, cycles 0.937. Overall since step
15: single-thread query 553.6 -> 308.1 ms, 12-thread query 104.1 -> 56.0 ms, with 642 -> 469 lines.
What remains per settled node: ~260 instructions, 2.3 branch misses, IPC ~1; the misses come from
the relaxation outcome and the edge-loop exit.

**Exactness of the 31-bit labels and the W = 8192 Dial window.** Edges of weight >= W are skipped;
the smallest skipped length is kept, and the query falls back to the u64 search only if that
length is below the answer found (proof in the comment on `bidir`). A randomized self-test (300
graphs of 50-4,050 nodes, 120,000 queries, 8% zero weights, weights around W, in [W, 2^31) and
>= 2^31, no-in-edge sources, s = t) compared bidir + fallback with an independent BinaryHeap
Dijkstra: no mismatch (33,400 fallbacks exercised). On the full set 10 queries fall back, ~1.3 ms
in total on one thread.

**Locality does not depend on the number of BFS regions.** Label pages touched over all queries,
counted deterministically: 1 region 362,199; 4 regions 362,355; 12 regions 363,088; 24 regions
370,259; 48 regions 366,506. The parallel BFS costs no locality.

**Settled nodes over all 2,000 queries:** one-ended 23.2M; two-ended 7.38M; expanding the side
with fewer queued entries instead of the smaller key, 7.15M.

## Findings specific to the scoring box

- **THP "always" and memory.peak** (steps 4-6, 24-25). The box backs anonymous memory with 2 MB
  pages on first touch, and `memory.peak` charges a whole 2 MB page for its first touched byte;
  the laptop runs THP "madvise". That was the board's 77 MB against the laptop's 59 MB. While the
  searches jumped across 22 MB of randomly numbered nodes, the graph needed its 2 MB pages (THP off
  cost 17% of time). After BFS relabelling it no longer did, and one `prctl(PR_SET_THP_DISABLE)`
  gave 0.145 → 0.142 s and 76.5 → 64.3 MB.
- **Page cache is charged to whoever faults it in.** `/data` is in the page cache before the run
  starts, so `graph.txt` is mapped read-only rather than copied: a heap copy of the 88 MB file
  would cost more than our whole M. `MAP_POPULATE` was equal or worse: it runs on one thread before
  the parallel passes.
- **Locked instructions cost more on the box.** Building the reverse graph through atomic
  countdown cursors saved 5 MB but ran 0.208 s against 0.178 s (step 16). Every builder since is
  lock-free.
- **Exit teardown runs on one core.** Exit was 13 ms of the local wall time. Exiting right after
  the write without freeing anything (step 22) removed about 5 ms of it; releasing the big regions
  on all threads first (step 40) took the rest from 8.45 to 3.62 ms.
- **Page-table lock on first touch.** Track C lost 11.7% of its cycles there. For D, faulting in
  each random-write array one slice per thread first (step 37) was neutral on the board, so D does
  not do it. The BFS id array was the worse case: claiming with a bitset instead (step 48) cut kernel cycles in the walk by 76%.

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
| Split a fallback at s (min over out-edges of w + bidir(v, t)) | avoid the u64 search | +9.6% instructions, +12% CPU; the two real cases took 41 and 53 ms vs 0.8 + 0.5 ms | bidir from the heads of 2^32-weight edges explores large balls |
| Branch-free free-list allocation in the Dial queue | fewer mispredicts | branch misses -2.6%, instructions +4.5% | |
| Side-choice hysteresis (switch only when queue sizes differ by H) | fewer mispredicts | unchanged at H = 1 and 8; every alternative side rule settled more nodes (7.138-7.351M vs 7.123M) | |
| Drop `#[inline(always)]` on the queue and step functions | -6 lines | +38% instructions, +12% cycles | LLVM does not inline them by itself |
| Page allocation out of line (`#[cold]`) | smaller hot loop | +3.5% cycles | |
| Dial window W = 4096 | smaller bucket array | cycles -1.2% (noise) | kept 8192 as margin for unseen weights 4096-8191 |
| PGO (profile from generator seeds 21 and 22, atomic counters) | -3..7% | instructions +0.1%, branch misses +6%, cycles -0.2..+2.4% | the hot loops are memory-bound and already inlined |
| Reverse graph through atomic countdown cursors (step 16) | 5 MB less than the bucketed builder's u16 side array | board 0.208 s vs 0.178 s, 72.2 vs 77.0 MB | on the scoring box the locked updates cost far more than the memory saved |
| Parse graph.txt once (step 36): each (item, bucket) segment gets room for its expected share plus 6 standard deviations, with an exact retry on overflow | skip the counting pass | in-process text build min 22.1 -> 15.9 ms but reverse 10.8 -> 11.7 ms; whole runs: build min 111.9 -> 98.0 ms, median 129.9 -> 130.2 ms; board 0.149 s vs 0.139-0.144 s for the same code without it | best guess: the 27% larger staging area, the hole filters, the waits and the slower reverse count ate the saving on a quiet box. **Retried in step 48 on top of the faster builder** (packing in place, estimate checked only with room): text build 25.6-27.3 -> 19.5-21 ms and it shipped |
| Fault in each random-write array one slice per thread first (step 37) | no page-table lock contention on random first touches | board 0.140 s vs 0.139-0.144 s, +14 lines | neutral |
| Release g0's pages on all threads before relabel drops it | parallel unmap | copy phase x1.024 | |
| Release the text mapping at the start of the query phase | earlier unmap | queries slower | |
| Split a fallback through s's out-neighbours, on top of the slow-queries-first order | faster fallbacks | seed 11's worst query 3-4x faster, but no gain on top of the order; +8 lines | the order already hides the slow queries |
| Relabel copy prefetch distance 16 or 12 instead of 8 | | in-process min 25.2 / 25.3 / 25.7 ms; perf cycles equal | no signal |
| Persistent spin-wait thread pool instead of spawning per `par()` (round 3) | spawn latency is 0.4-8 ms per phase | 1.6-2.8x slower in median on WSL2 (pause-loop exits, vCPU preemption) | too risky without box evidence; not committed |
| Thread pool on a Barrier (futex) (round 3) | skip the spawns | neutral | wake latency is still 2-5 ms on WSL2 |
| Base + offsets in one 32-byte record (round 3) | one miss instead of two for off(v) | query cycles +4.7%, instructions +6.6% | base[] is cache-resident, so the extra address arithmetic costs more |
| One-line offset record, malloc mmap threshold 64 MB, deeper BFS prefetch (32/16) or none at 16 ahead, k = threads x 8 or x 16 items, text staging in a separate packed array | each a small build win | +1.2k faults and 65.7 MB peak (mmap threshold); worse (prefetch depth); no signal (item count); +4.9k faults and +3 MB (separate staging array) | measured on counters, not committed |
| Prefetch in both passes of the relabel stream | | count pass +110M cycles, fill pass -245M | limited to the fill pass |
| Pre-pass prefetching the neighbours' `rel` lines before relaxing (D4) | overlap the offset misses | instructions 1.203, cycles 1.050 | the loads cost more than the overlap saves |
| Prefetch the last edge slot too when queueing (D4) | lists that straddle lines | instructions 1.040, cycles 1.042 | |
| Drop `#[inline(always)]` from `off`/`adj`/`slots`/`ix` (b764d34, -4 lines) | | BFS walk cycles +6%, reverse fill +6% | LLVM does not inline them |
| Reverse counts collected in the relabel finish; text u16 places reused for the relabel | -2 ms and -1 ms | not built | +10 and +6 lines for those gains; the text munmap only moves, and keeping the buffer would put a 10 MB array in the query-phase peak |
| Relabel copy in new-id order (sequential writes through the inverse permutation, three prefetch stages) | sequential stores | 12 threads min 23.4 -> 22.6 ms but q1 27.1 -> 27.2 ms; 1 thread +20% cycles, +9% instructions | each node now costs three random line reads instead of two reads and one random write |

## Correctness

- Before every push: the worked example, the four tiny seeds, the four sample seeds and full
  seed 1 all match the published sha256, at 1, 2, 7 and 12 threads (`FW_THREADS`). Since step 34
  every D commit also passes the unseen generator seeds 11 and 12 and the randomized self-test
  below.
- Randomized self-test: 300 graphs of 50-4,050 nodes and 120,000 queries against an independent
  BinaryHeap Dijkstra (details under "Exactness" above), with 3 work items per graph so that the
  text cuts fall mid-line.
- Distances above 31 bits (from a no-in-edge node, the max-weight and over-uint32 island queries)
  go through a one-ended u64 Dijkstra.
- Nothing depends on the seed: the bit split comes from the header n, the offset block size
  shrinks if a block ever needs more than 255 slots, and there is no degree limit (the old u16 degree array, which panicked above 65k, is gone).

## How to reproduce

```
# build exactly as the board does and check the full set against its expected hash
python3 check.py d --data data/d --expected <sha256>

# our bench: builds the image, runs it with the board's container flags (read-only root,
# /out on tmpfs, user 65534), prints wall time inside the container and memory.peak
python -I tools/bench.py d --gate G1                         # worked example + 4 tiny seeds
python -I tools/bench.py d --gate G2 --no-build              # 4 sample seeds
python -I tools/bench.py d --gate G3 --runs 5 --no-build     # full seed 1, 5 runs after a warm-up
python -I tools/bench.py d --gate G3 --threads 7 --no-build  # the same with FW_THREADS=7
```

The data comes from the public generator (`d_traverse.py --seed 1 --scale full`). `FW_THREADS`
sets the thread count; the default is the available parallelism.

## Memory floor (measured Nov 1, D7)

The peak (58.2 MB anon RSS at the phase ticks, ~59.7 MB sampled; board 62.4 MB) is in the relabel fill, when g0
(22.2 MB), the new ids (7 MB), the g1 staging (20 MB) and its place array (10 MB) are all live. Every later phase sits
at ~56.1 MB: g + r are 44.4 MB and ~12 MB of freed heap is reused by the reverse build and then the query state
(9.1 MB, ~760 KB per thread). Freeing memory early does not pay here: each MB released must be faulted back in by a
12-thread phase at ~3 µs per fault (~0.8 ms per MB), about what 1 MB is worth in points. Measured and rejected:
releasing a quarter of g0 during the fill (-2.1 MB, +1.5-3 ms, +11 lines), `malloc_trim` / mmap'd big buffers
(54.4-55.3 MB, queries +8 ms), a gather-based reverse build (54.9 MB, reverse build 7.6 -> 20.5 ms). The next step
would be 3-byte slots (98.8% of BFS-order deltas fit 14 bits, 98% of weights fit 10 bits): about -8 MB live.
