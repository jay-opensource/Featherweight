# Track C: performance log

Thesis: **little memory per unit of speed.** One shared name dictionary and one bit per
(country, name) replace a hash set per thread; the input is mapped, never copied; parsing finds
field ends from SIMD bit masks, and threads start at guessed record boundaries that are proved
afterwards.

Local box: i5-11400H laptop (6 cores, 12 MB L3) running Docker under WSL2, shared with other
benchmarks (load average often 15-20), so local wall times are only good for gross A/B. The
stable local metric is `perf stat` instructions on one thread (`FW_THREADS=1`); the scoring box is
an i5-13500 (6 P-cores with HT, 2.5 GHz fixed, 24 MB L3). Board run IDs are the record.

## Board runs

| Step | State | Change | Run ID | T (ms) | M (MB) | S | Points |
|---|---|---|---|---|---|---|---|
| 0 | reference | Python `csv` module, one thread | | 14444 | 367 | 108 | 0 |
| 1 | 02:23 | Baseline: parallel speculative chunks, scalar parser, shared interner (4-byte table + arena), one locked OR per record | | 47.4 | 10.8 | 549 | 72.1 |
| 2 | 02:56 | Bit-mask parser (AVX2 + carry-less multiply), batched lookups with prefetch, bits queued per region (no locked OR), names inline in 32-byte slots | | 43.3-43.6 | 12.2-12.6 | 600 | 72.2 |
| 3 | 03:06 | + positions-only flatten, quotes counted per record, line-aligned probes with two prefetched lines, numbers from known field ends, code compacted to rustfmt widths | | 50.9 | 12.7 | 358 | 72.0 |
| 4 | 03:43 | + money-specialised number parse, 48 chunks per thread, static-pie + `alderlake`, distinct names counted when a bit turns on, `MADV_SEQUENTIAL`, 24-byte slots, lean probe loop | | | | 350 | |

## Local measurements per step (one thread, `perf stat`)

| State | Instructions | Note |
|---|---|---|
| 02:23 baseline | 1.23G, IPC 0.3 | 3.7-4.4G cycles: every record waits on a locked OR and three dependent misses |
| 02:48 bit-mask parser + batch | 2.13G | cycles 1.9G: the parse no longer serialises on its own positions |
| 02:48 ring of positions | 1.99G | `copy_within` (memmove) of the leftover positions per block was ~40 instructions per record |
| 02:56 inline 32-byte slots | 1.98G | one prefetched line per lookup instead of table + arena |
| 02:58 positions-only flatten | 1.80G | quotes counted once per record at its LF; a name's `""` pairs stored in its slot |
| 03:06 compacted | 2.02G | the generic `num` (sign and dot handled for every number) cost +240M |
| 03:27 `num(.., money)` | 1.84G | the flag is constant per call site, so ids and regions skip the money logic |
| 03:34 24-byte slots | 1.855G | -1.05 MB (3.1 MB of slots instead of 4.2 MB) |
| 03:43 lean probe loop | 1.767G | common path is the header's length test + 2 word compares; 2.55 probes per lookup measured |

Where the 1.77G go (one thread, per record, from builds that skip each phase): block scan and
flatten ~112, field parse ~200, aggregates ~40, dictionary lookup ~140, bit queue and apply ~50.

## Failure museum (measured, then rejected or replaced)

| Idea | Expected | Measured | Why it lost |
|---|---|---|---|
| One `lock or` per record into the shared bitset (baseline) | 3M atomics are cheap | 12 threads: +20 ms over the same run with a plain racy OR (+1 ms) | the line was usually last written by another core, and a locked op blocks every later load until it lands |
| Scalar state machine (quote scan per field) | simple, branch-light | 1 thread 500-680 ms vs 380 ms for the bit-mask parser | every field's position depends on the previous one: one long dependency chain per record |
| Per-position quote counts in the flatten | exact pair counts per field | +180M instructions vs counting once per record | 8 popcounts per block for 3 useful values per record |
| `copy_within` to compact the position buffer | small copies | ~40 instructions per record (two memmove calls per block) | replaced by a 128-entry ring |
| Generic `num` (sign/dot detected per number) | fewer lines | +240M instructions (+13%) | ids and regions paid for money parsing; a constant flag removed it |
| `MADV_POPULATE_READ` per chunk | fewer page-fault traps | no gain within noise (103-117 ms vs 80-100 ms) | fault-around already maps 16 pages per fault |
| Inline 32-byte slots vs 4-byte table + arena (1 thread) | one fewer dependent miss | cycles equal within noise; LLC misses +27% on the laptop | 4.2 MB of slots vs 3.3 MB; kept the one-line lookup but shrank slots to 24 bytes |
| Two prefetched lines per lookup | fewer second-line misses | 1 thread: lower cycles in 6 of 6 pairs; box: suspect in the 43 → 51 ms step | board A/B pending |
| Hash-only identity for names | no compare | not tried | not exact; `names_distinct` is in the hash |

## Correctness

- Gates before every ship: the worked example and four tiny seeds (G1), four sample seeds (G2),
  full seed 1 (G3), and full seed 1 at 1, 2, 7 and 12 threads. Hidden seeds 11 and 12 were checked
  by the lead on the board snapshot.
- Chunk boundaries are guessed (a LF followed by a digit or a BOM) and proved: the first chunk
  starts after the header, so its parse is exact; if it ends exactly where the next chunk starts,
  that start is a true record start, and so on by induction. Any mismatch reruns on one thread.
- Every byte of the input outside the generator's shape still parses by spec 2.1: unquoted text,
  numbers longer than 8 bytes and names over 16 bytes take slower paths, never wrong ones.
- The dictionary compares full names (the raw, still escaped bytes: escaping is a bijection, and
  `""` and an empty unquoted field are the same key). A dictionary that fills up reruns larger,
  with a warning on stderr.
