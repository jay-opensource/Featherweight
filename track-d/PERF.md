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

## Correctness

- Before every push: the worked example, the four tiny seeds, the four sample seeds and full
  seed 1 all match the published sha256.
- Concurrency: full seed 1 with 1, 2, 7 and 12 threads (`FW_THREADS`).
- Distances above 31 bits (from a no-in-edge node, the max-weight and over-uint32 island queries)
  go through a one-ended u64 Dijkstra.
- Nothing depends on the seed: the bit split comes from the header n, the offset block size
  shrinks if a block ever needs more than 255 slots, and degrees above 65k stop with a message.
