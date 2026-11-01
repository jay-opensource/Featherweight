# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Thesis: **minimum memory per unit of speed.** A shortest-path query on a road network is a walk
through memory, so the program spends its effort on making that memory small and local.

1. **A 4-byte edge.** Each edge slot is `target:21 | weight:11` (the split comes from the header's
   n). 99.93% of weights fit 11 bits; a larger weight stores the escape value in the slot and the
   full u32 in the next slot. A node's slots start at a u32 base per block of 16 nodes plus a u8
   offset inside the block, so a node costs about 1.25 bytes of offsets. Both graphs (forward and
   reverse) are 22 MB each.
2. **Parsing without contention.** `graph.txt` is mmapped (page cache is not charged to the
   process). Numbers are parsed 8 bytes at a time (SWAR). The CSR is built in buckets of 8,192
   nodes: each work item counts its slots per bucket, appends slots to its bucket's region, and
   each bucket is then sorted by node in a 94 KB scratch copy. No atomics, and no store lands
   outside the bucket being worked on.
3. **Breadth-first relabelling.** Node ids are a random relabelling of a road lattice, so a
   search on the raw ids jumps all over memory. Each thread grows a breadth-first region from
   its own seed (claiming nodes with a CAS) and the graph is copied into that order; neighbours
   in the network become neighbours in memory.
4. **Two-ended Dijkstra.** Forward from s and backward from t on the reverse graph, always
   expanding the side with fewer queued entries, stopping when the two queue minimums together
   reach the best meeting length. Over the 2,000 queries this settles 7.15M nodes instead of
   23.2M for a one-ended search. Labels are 31-bit distances (top bit = settled) in per-thread
   pages of 64 node ids, one set of pages per side, allocated on first touch; BFS order keeps a
   search inside few pages. Queues are radix heaps of 8-byte entries.
5. **Exact for every weight.** A label that would need more than 31 bits is skipped (every path
   through it is at least that long), and the rare query whose answer could be that long is
   answered by a one-ended u64 Dijkstra.
6. **Memory accounting.** The scoring box runs transparent huge pages "always", which charges a
   whole 2 MB page for the first touched byte of a sparse allocation. A small global allocator
   marks every allocation of 256 KB or more MADV_NOHUGEPAGE and lets only the dense graph arrays
   opt back in.

The measurements behind each step, the board run IDs and the experiments that lost are in
[PERF.md](PERF.md).

## Dependencies

None (Rust standard library only).

## With another 24 hours

- Build the forward and reverse graphs in one pass over the relabelled order instead of two.
- A Dial-style bucket queue for the search (edge weights in the main body are below ~8,100).
- Better locality orderings than BFS regions (e.g. recursive bisection), if they can be computed
  inside the time budget.
