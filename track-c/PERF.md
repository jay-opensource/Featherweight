# Track C: performance log

Every step from the reference to the shipped version, with the live-board run ID that measured it.
Local numbers come from a laptop and are labelled as such. We used them to choose between variants;
the run IDs are the record.

## Result

| | Run ID | Commit | T | M | S | Points |
|---|---|---|---|---|---|---|
| Reference (Python) | | | 14.444 s | 367.4 MB | 108 | 0 |
| First correct entry | run-0c6e9d04253d | fe231bc | 51.4 ms | 11.7 MB | 549 | 70.9 |
| Before the final rounds (C2) | run-be0296de8c66 | bbee872 | 30.44 ms | 12.04 MB | 378 | 77.67 |
| C4 (stored names, 3.15 MB dictionary) | run-5feec0fbae92 | 96c67ed | 28.38 ms | 11.82 MB | 373 | 78.56 |
| C6 fingerprint dictionary (68391ae) | run-fa0a945bb7b4 | 2a04827 | 25.49 ms | 8.97 MB | 362 | 81.02 |
| **Best, and the shipped code (8509525: locked-OR bits)** | **run-52a91432fca0** | **1f2babf** | **24.94 ms** | **8.63 MB** | **349** | **81.54** |
| The same code, another run | run-02fa2d778aad | ccff89d | 26.13 ms | 9.24 MB | 349 | 80.74 |
| Dropped: startup change (ee7290e) | run-9b9bbe26dc06 | 01feb2f | 25.66 ms | 10.71 MB | 350 | 80.28 |

The best run is 579 times faster than the reference with 42.6 times less memory. The two runs of
the final C code span 24.9 to 26.1 ms. The C6 changes (steps 32-39) took the track from 28.4 ms /
11.8 MB / 78.56 to 24.9 ms / 8.6 MB / 81.54: the fingerprint dictionary alone was -2 MB and -3 ms.
Points are the board's:
`50·log10(K)` with K = (T_ref/T)^0.5 × min(50, M_ref/M)^0.2 × min(1, S_ref/S)^0.15. At this
operating point 1% of T is worth about 0.11 points, 1% of M about 0.04, and 10 lines about 0.09.

## How we measured

**The board.** One run per push on the scoring box: an i5-13500 with 12 threads at 2.5 GHz,
Debian 13 (kernel 6.12), transparent huge pages set to "always". `/data` is already in the page
cache when the run starts. M is the cgroup's `memory.peak`. S is the code lines of `src/main.rs`
after rustfmt plus the 7 lines of the Dockerfile.

**Board noise is large on this track.** Identical C code gave these times:

| C code | Board runs (ms) |
|---|---|
| 2ddf225 (the 02:56 state) | 43.33, 43.60, 43.52, 43.67 |
| eaefee5 (C2 parse core), alderlake + static build | 48.04, 42.15 |
| eaefee5, x86-64-v3 dynamic build | 42.70, 44.42, 45.48 |
| 34f8892 (variant P) | 32.68, 32.07 |
| 4b5a3b2 (the final C code; steps 28-31) | 29.12, 31.77, 28.38, 30.78 |

A single run cannot resolve a change much below 10%. We decided on local counters and interleaved
A/B runs, and used the board to confirm the large steps.

**Local.** An i5-11400H laptop (6 cores, 12 MB L3) running Docker, shared with other benchmarks
(load average 4 to 22), so its wall times are noise. What we used instead:
- `perf stat` in a container on full seed 1: instructions (deterministic), user and kernel
  cycles, branch misses, page faults. "1 thread" means `FW_THREADS=1`; per-record figures divide
  by the 3,320,000 records.
- `perf record -g` with kernel symbols, in a privileged container, for the kernel side.
- Several binaries in one container, run in alternation for 20-25 rounds and compared by median.
- `memory.stat anon` / `RssAnon` for memory: the laptop's `memory.peak` is polluted whenever other
  workloads evict the page cache and our re-reads get charged.

## What the program does, and where the time goes

`regions.csv` (200,000 lines) becomes a byte table from region id to country number. Then
`records.csv` (192.6 MB, 3.32 million records) is cut into 48 byte ranges per thread. Each range is
read in 32 KB pieces into a per-thread buffer and parsed. Each record is added to its country's
per-thread aggregates, and its name goes to one shared dictionary and sets one bit per
(country, name). One thread pool (spawned once) runs the regions jobs, waits at a barrier, parses the records, waits at the barrier again and popcounts the bitset. The README describes the design in prose.

**Timeline** (local, 12 threads, 23e1807: the shipped code before its line pass):

| Phase | Time |
|---|---|
| exec, start, `regions.csv` | done at 1.4-1.8 ms |
| `records.csv`: parse, aggregate, distinct names | about 35 ms; threads finish within 0.8 ms of each other |
| merge and write `aggregates.txt` | 0.2-0.3 ms |
| exit | part of about 2 ms for exec plus exit |

**Instructions per record** (local, 1 thread):

| Version | Total | Parse | Flush + region | Lookup + aggregate | Bit queue + apply |
|---|---|---|---|---|---|
| Before C2 (the 04:12 state, step 14) | 546 | 309 | 38 | 165 | 44 |
| C2 v2 (shape parse, SIMD numbers, AES hash) | 379 | 228 | 19 | 90 | 48 |
| Shipped (5c8dd35) | 399 | | | | |
| C3 (5671ab1): popcount after the barrier, bucket index kept in the record | 383 | | | | |
| C6 fingerprint dictionary (68391ae) | 352 | | | | |
| C6 locked-OR bits (8509525, the shipped code) | 340 | | | | |

The phase columns come from builds that skip parts of the work. The 64-byte buckets added after v2
cost 15 instructions per record and roughly halved the branch misses.

**The kernel side.** At 437ba8c (12 threads), `__pv_queued_spin_lock_slowpath` was 11.7% of all
cycles: page faults on our shared tables, queueing on the page-table lock. Mechanisms 7 and 8 below
removed it.

| 12 threads, local medians | 437ba8c | Variant M (step 22) | Variant P (step 23) |
|---|---|---|---|
| User cycles | 1.25-1.33G | 1.17-1.24G | 1.05-1.10G |
| Kernel cycles | 0.20-0.25G | 0.15-0.16G | 0.26-0.30G |
| Page faults | 9.2k | 5.3k | 2.3k |

## The mechanisms, each with the measurement behind it

**1. Record structure from bit masks, not a byte loop.** Each 64-byte block gives AVX2 masks of
quotes and LFs. A carry-less multiply of the quote mask by all ones (`pclmulqdq`) is a prefix XOR:
bit k is set inside a quoted field. An escaped `""` toggles twice and changes nothing. The LFs
outside quotes end records. The first parser scanned quotes byte by byte: 500-680 ms on 1 thread,
against 380 ms for the first mask parser and 270 ms once the lookups were batched (local).

**2. Fields found by the shape of a record.** Inside a record the layout is fixed,
`[BOM] id , name , note , region , amount [CR] LF`, so the parser does not look for every comma:
- the id's digits run from the front (a digit has bit 4 set, a comma does not);
- the name ends at the first comma outside quotes, found with one 32-byte quote mask;
- the amount and the region contain no comma, so they sit after the last two commas before the
  terminator;
- the note is what remains.

Quotes are counted once per record. The name's quote count comes from its window, so the note's is
the record's minus the name's, and both byte counts follow from lengths and quote counts, with no
unescaping and no copy. Parse cost: 546 → 430 instructions per record (C2 v1).

**3. Three numbers in one vector.** The id, the region and the amount (as cents: the dot is
squeezed out of the amount's last 8 bytes) are joined in one AVX2 register. The bytes below the
digits are masked off, then `maddubs`, `madd` and `mul_epu32` combine digits in pairs, fours and
eights. Together with the AES hash below: 430 → 379 instructions per record (v2).

**4. One shared fingerprint dictionary, one cache line per lookup (C6, 68391ae).** The dictionary
stores no names. A bucket is 64 bytes, eight 64-bit entries `fingerprint | id + 1` (0 is empty):
44 bits of a 128-bit hash of the whole raw, still-escaped name, the dense id + 1 in the low 20 bits,
and bit 63 always set. The hash is three AES rounds over 32-byte windows of the name, zero-padded
(escaping is a bijection, and `""` and an empty unquoted field both give the empty name, so this
counts exactly the distinct unescaped names). The bucket is chosen by 14 other bits of the hash:
16,384 buckets, 1 MB, for the 100,520 names. A lookup reads one line (prefetched during the parse)
and compares all eight entries at once with two AVX2 compares; a full bucket goes on to the next,
and a bucket counts as full at 7 of 8 slots. An insert claims its slot with one compare-exchange,
and ids come in blocks of 64 per thread.
- Identity is 44 stored bits plus 14 bucket bits of the hash: for 1e5 names a collision has
  probability about 3e-8 per run. Names of up to 120 bytes (31, 32, 33, 63, 64, 65, escapes,
  trailing-space twins) matched the reference on generated files at 1, 2 and 12 threads.
- Before (C2 to C5): 49,152 buckets (3.15 MB) of three slots, each a 32-bit header and a 16-byte
  key, with the rest of a long name in a side store (one name in seven went to the next bucket),
  and before that linear probing over 24-byte slots (2.55-2.65 probes per lookup, about 1.2 branch
  misses per record).
- Fingerprints against stored names, 12 threads local: instructions 383 → 352 per record, user
  cycles 0.898G → 0.736G (-18%), branch misses 2.26M → 1.43M, about -2 MB. Board: 25.49 ms,
  8.97 MB, S 362, 81.02 (run-fa0a945bb7b4), from 28.56 ms and 11.46 MB.

**5. Lookups in batches of 32, prefetched during the parse.** While a batch is parsed, each
record's bucket and region byte are prefetched; the batch then does its lookups. Removing the
bucket prefetch cost +0.5G cycles, removing the region prefetch +0.25G (1 thread). Looking each
record up right after parsing it saved 9% of instructions but cost 15-25% more cycles.

**6. Distinct names: bits set by a locked OR on words prefetched for writing (C6, 8509525).**
Distinct names per country are one bit per (country, name id), laid out as
[page of 4096 ids][country][64 words], about 3 MB. 90% of the records set a new bit, and its cache
line is usually owned by another core. A `lock or` per record straight after the lookup cost +20 ms
at 12 threads (step 1), because it waits for the line and blocks every later load meanwhile. Between
C2 and C5 the bits went through per-region queues (64 regions × 64 pairs) applied under a mutex per
region. Now `flush` computes each record's bit word as its id is found and issues a prefetch for
writing on it, and after the batch's 32 lookups ORs the bits in with `fetch_or`: by then the lines
have arrived. The prefetch must be `prefetchw`: `_MM_HINT_ET0` compiled to `prefetcht0` until
`+prfchw` went into `.cargo/config.toml` (7c8b895, user cycles -2.3%), and a read prefetch makes each
store upgrade the line again.
- Removed: the region queues, their locks and `apply`; 192 KB of stack; 353 → 340 instructions
  per record, user cycles -2.2%, S 362 → 349. Board: 24.94 ms, 8.63 MB (run-52a91432fca0).
- Distinct names are counted once, after the parse (C3, 16e30ca): after the last bits are set the
  threads wait at the barrier a second time and each popcounts a contiguous 1/T of the used bitset
  pages (those below the id counter) into its per-group counts: 399 → 394 instructions per record
  and 12-thread user cycles -3.1% (better in 14 of 20 interleaved rounds).
- Deferring the ORs by one batch (so the RFOs have surely completed) gave 0.675G against 0.677G
  cycles: what remains is the locked OR's own cost, not its miss.

**7. Map the shared tables before the threads touch them (variant M, step 22; reworked in 10 and 11).** The 11.7% in the
page-table lock came from first-touch faults on the dictionary and the bitset (`apply` 6.3%,
`insert` 3.3%, `flush` 2.5%). Many came in pairs: a read maps the zero page, then the first write
copies it. Now:
- each thread maps its slice of the dictionary with `MADV_POPULATE_WRITE` when it starts (since C3,
  at the start of the one pool, before the barrier; see 11);
- each 4096-id page of the bitset is mapped by the thread that hands out that page's first id;
- the region table ends at the largest region id, so the 13k unmatched records with larger ids
  read no untouched pages (that cost 2.1k zero-page faults per run);
- each job finds its own first record start, in parallel (a serial scan of all 576 cuts on the
  main thread took 1.5-4.8 ms locally).

Page faults fell from 9.2k to 5.3k. Board: **36.82 ms** (run-f75d12327f2e), against 42.15-48.04 ms
for the same parser before it.

**8. Read the records instead of mapping them (variant P, step 23).** Each thread reads pieces
with `pread` into its own buffer (64 KB pieces at first, 32 KB since C3: 8dfd66a lowered the
local memory.peak median 12.26 → 11.73 MB at neutral time, while 16 KB cost +8% kernel cycles): 64 bytes in front (a record looks back up to 32), the piece, then
zeros. The buffer stays in L2, and there are no page-table entries to fault in while parsing or to
tear down at exit. A record cut by the end of a piece is read again at the start of the next. On
the laptop, P had the lowest user cycles and the highest kernel cycles (the copy: `_copy_to_iter`
6-8%), so only the board could decide. It chose P: **32.68 ms** (run-6c8cfb2db4cb), and 32.07 ms on
a second run (run-665e917d324d). An earlier P build mapped each thread's buffer separately and
contended on the address-space lock; all buffers are now one allocation made before the threads
start.

**9. 2 MB pages where every byte is used (step 25; extended in 13; now the first 4 MB).**
Allocations are 2 MB-aligned, and the first 2 MB of the dictionary (then 3.15 MB, all touched) and
of the bitset (ids below 16,384, all touched at full scale) got `MADV_HUGEPAGE`. With the 1 MB
fingerprint dictionary the mapping is the dictionary then the bits, and its first 4 MB (the
dictionary and the bits of ids below 98,304) are on two 2 MB pages, all used at full scale. Both ranges are fully used, so resident memory does not change
(RssAnon 8,952 kB either way; AnonHugePages 0 → 4,096 kB) while the TLB covers more. Locally:
1 thread −4% cycles; 12 threads user −4.5%, kernel −9%, task-clock −9%. Shipped together with a
line pass (399 → 378 lines): board **30.44 ms** (run-be0296de8c66).

**10. Parallel parsing that proves its own cuts.** A job's records start at the first "LF followed
by a digit or a byte order mark" after its cut. Inside a quoted note an LF is always followed by a
letter or a quote (spec 2.2), but nothing relies on that. Job 0 starts right after the header, a
true record start, so its parse is exact and stops at the first record start after its range. If
that equals where job 1 started, job 1 also started at a true record start, and so on by
induction. Any mismatch, or a full dictionary, reruns everything on one thread with a 4× larger
dictionary and a warning on stderr. 48 jobs per thread balance the tail: 8 per thread was 51.6
against 49.4 ms (local).

**11. One thread pool, with a barrier (C3, 512ac07).** The region table is sized before any thread
starts (region ids ascend, so the last line of `regions.csv` holds the largest id). Each thread
then maps its share of the shared tables, runs regions jobs (dynamic, so threads that mapped more
do fewer), waits at a `Barrier`, and parses records. Before, the regions phase and the records phase
each spawned 12 threads, and starting a thread (a stack mapping under the address-space lock)
competed with page faults. Startup-only builds, 30 interleaved rounds: kernel cycles 16.0M → 13.2M
(better in 29 of 30), task-clock 8.4 → 7.0 ms, about 390 fewer page faults per run (the second
round of spawns got fresh stacks because the first round's were not yet back in glibc's cache).
Threads also add their aggregates into one shared `Mutex`-guarded table at the end instead of
returning 12 packets (36c909f: memory.peak −0.13 to −0.26 MB, 114 fewer faults).

**12. The record carries its bucket (C3, efac31d).** `record` computes `bucket << 32 | tag` once,
for its prefetch, and `id` and `insert` reuse it instead of the hash; a bounds check on the shared
table went too. 394 → 383 instructions per record (more than the multiply and shift alone), cycles
flat on 1 thread, user cycles −1.3% at 12 threads (weak: 12 of 20 rounds).

**13. 2 MB pages by one thread each, in one mapping (C3 ce783b0, C4 4b5a3b2).**
`MADV_POPULATE_WRITE` on a 2 MB page allocates, charges and clears the whole folio before it takes
the page-table lock, and frees it if another thread installed one first. With 8 threads populating
the first 2 MB of the dictionary together, each racer cost a 2 MB clear and a transient 2 MB
charge in memory.peak. Now exactly one thread maps each huge page before the barrier (the regions
jobs balance the load), and the other slices are rounded to 4 KB (an unaligned slice makes the
call fail silently with EINVAL: +527 faults). Memory.peak median 13.03 → 12.52 MB, runs over 14 MB
6 of 20 → 1 of 20. C4 then put the dictionary and the bitset in one 2 MB-aligned mapping and
advised its first 6 MB (the whole dictionary and the bits of ids below 98,304, all used at full
scale): three huge pages, three mapping threads. RssAnon is unchanged (8,504 vs 8,508 kB),
AnonHugePages 4,096 → 6,144 kB; 12 threads user cycles −0.9% to −3.7%, kernel −2.6% to −9%,
task-clock −3.5% to −4.8%; board 30.89 → 29.12 ms (run-b1d4224ea8b9). Since the fingerprint
dictionary the advised range is 4 MB and two threads map the two pages.

## Findings specific to the scoring box

- **THP "always" and memory.peak.** The box backs anonymous memory with 2 MB pages on first touch,
  and `memory.peak` charges a whole 2 MB page for its first touched byte. Every table comes from
  `mmap` with `MADV_NOHUGEPAGE`, so only the 4 KB pages we touch count. The one exception is
  mechanism 9: 2 MB pages only where all 2 MB are used anyway. Thread stacks are 256 KB
  (`RUST_MIN_STACK`).
- **The page-table lock on first touch** was the largest single cost after the parser itself (11.7%
  of cycles; mechanism 7). One `MADV_POPULATE_WRITE` call by one thread replaces thousands of
  faults from twelve.
- **Zero-page faults from an oversized table.** A 16M-entry region table cost 2.1k extra faults per
  run, all on the few page-table pages above the real id range, from all threads. Cutting it at the
  largest id: page faults 9.1k → 7.1k, kernel cycles 220M → 192M (local, min of 3).
- **pread against mmap.** Locally `pread` raised kernel cycles (0.15 → 0.28G), yet on the box it
  took 4 ms off variant M. Mapping the input costs more on the box than on the laptop. The same
  idea lost for Track A (see its PERF.md), so each track keeps what the board measured.
- **Exit teardown is cheaper than a timed munmap suggests.** A timed `munmap` of the 192 MB input
  mapping at the end of `main` took 7.0-7.6 ms, but the wall time after the last instruction was
  only 1.7-2.4 ms: exit tears down the whole address space at once. Unmapping in parallel before
  exit lost (step 14 and the table below). With `pread` there is no input mapping left to tear
  down.
- **Thread start contends with page faults.** Run alone, the regions phase takes 2.4-2.8 ms, of
  which user cycles are only 7.6-9.4M. The kernel time is the address-space lock (`rwsem`,
  `osq_lock`) between thread-stack mappings and page faults. The single thread pool of C3
  (mechanism 11) removed one of the two rounds of spawns.

## Board runs

"Same code" rows re-ran unchanged C code (another track had changed); they sample the noise.

<!-- New runs: append a row with the next step number, and update the Result table above. -->

| Step | Commit | Change | Run ID | T (ms) | M (MB) | S | Points |
|---|---|---|---|---|---|---|---|
| 0 | | The reference, unchanged | | 14,444 | 367.4 | 108 | 0 |
| 1 | fe231bc | First entry (10c1060): speculative parallel chunks, scalar parser, shared name table with an arena, `lock or` into per-country bitsets | run-0c6e9d04253d | 51.4 | 11.7 | 549 | 70.9 |
| 2 | e3964b4 | Same code | run-8b2732863cf3 | 47.41 | 10.81 | 549 | 72.11 |
| 3 | 018ba3b | Same code | run-37875c84f7c9 | 48.00 | 11.71 | 549 | 71.63 |
| 4 | 2ddf225 | Bit-mask parser, batches of 32 with prefetch, region-queued bits (no locked op), names inline in 32-byte slots | run-7bdcfa4a8e0c | 43.33 | 12.56 | 600 | 72.15 |
| 5 | 2d45234 | Same code | run-99b338dcd14d | 43.60 | 12.33 | 600 | 72.16 |
| 6 | 8cba48c | Same code | run-01cee393bcd8 | 43.52 | 12.23 | 600 | 72.22 |
| 7 | cc9faa7 | Numbers read from known field ends, line-aligned probes with a second line prefetched, alderlake + static build; 600 → 358 lines | run-86bef7f2179f | 50.92 | 12.75 | 358 | 72.01 |
| 8 | bd5f56e | 9e5d1f2: money-specialised number parser, 48 chunks per thread, distinct names counted as bits turn on, 24-byte slots; 2.02G → 1.84G instructions (1 thread) | run-b59e8f0da090 | 48.68 | 11.54 | 355 | 72.96 |
| 9 | cdf5158 | Same code | run-dd5f58d1aff7 | 46.65 | 12.52 | 355 | 73.07 |
| 10 | 64d75ea | 0750a4e: lean probe loop (header length test, two word compares): 1.86G → 1.77G instructions | run-07cac53c86f9 | 47.40 | 11.64 | 353 | 73.23 |
| 11 | bb33a01 | Same code | run-e57a4cdeefac | 48.22 | 11.40 | 353 | 73.14 |
| 12 | 8c1f26b | Same code | run-5ff051454f8f | 48.30 | 11.46 | 353 | 73.10 |
| 13 | c8c57cd | Experiment: one malloc arena (`M_ARENA_MAX = 1`); reverted | run-8d5d7fc6784a | 48.88 | 11.64 | 355 | 72.88 |
| 14 | 5b3321f | Strided chunk order; each thread unmaps its share of the input before exit | run-2c17f3b4af9d | 48.23 | 11.92 | 354 | 72.93 |
| 15 | b0d00b1 | Experiment: the step-4 code again, to check its 43 ms | run-f1380e11305f | 43.67 | 12.17 | 600 | 72.20 |
| 16 | 1caefee | Bisect: the 03:02 state (step 4 plus line-aligned probes, two lines prefetched) | run-9055d8911ccb | 46.46 | 12.57 | 600 | 71.39 |
| 17 | 6a59621 | eaefee5, the C2 parse core: fields by shape, three numbers in one AVX2 vector, AES hash, 64-byte buckets of three slots; 546 → 400 instructions per record | run-9f4d4e49d3b6 | 48.04 | 12.06 | 362 | 72.85 |
| 18 | 43abc73 | Same code | run-62845bf401f3 | 42.15 | 12.21 | 362 | 74.22 |
| 19 | d952819 | Build: x86-64-v3 + `pclmulqdq` + `aes`, dynamically linked (the step-4 flags) | run-6ff252fd014a | 42.70 | 11.68 | 362 | 74.27 |
| 20 | fcc5579 | Same code | run-150983606380 | 44.42 | 11.79 | 362 | 73.80 |
| 21 | 1803e34 | Same code | run-e03c1dd8e247 | 45.48 | 12.19 | 362 | 73.40 |
| 22 | 79c2804 | Variant M: shared tables mapped up front, per-job chunk starts, exact-size region table | run-f75d12327f2e | 36.82 | 12.08 | 376 | 75.61 |
| 23 | 65ff880 | 34f8892, variant P: records read with `pread` in 64 KB pieces into a per-thread buffer | run-6c8cfb2db4cb | 32.68 | 12.21 | 399 | 76.66 |
| 24 | 477f874 | Same code | run-665e917d324d | 32.07 | 12.43 | 399 | 76.79 |
| 25 | bbee872 | C2 final (5c8dd35): first 2 MB of the dictionary and of the bitset on 2 MB pages; 399 → 378 lines | run-be0296de8c66 | 30.44 | 12.04 | 378 | 77.67 |
| 26 | 8a67e30 | Same C code as step 25 (documentation change) | run-5bea9473e1b6 | 31.40 | 12.84 | 378 | 77.06 |
| 27 | 48198af | C3: one thread pool for regions and records with a `Barrier`; popcount after the barrier; bucket index in the record; one thread maps each 2 MB page; 32 KB pieces; one shared aggregate table; 378 → 374 lines | run-2ba4c7084215 | 30.89 | 11.76 | 374 | 77.65 |
| 28 | b5f8df8 | C4 (4b5a3b2): dictionary and bitset in one mapping, its first 6 MB on 2 MB pages, one mapping thread per page | run-b1d4224ea8b9 | 29.12 | 11.61 | 373 | 78.35 |
| 29 | a8fb336 | Same C code as step 28, on a slow stretch of the box (the same push ran D at 140 ms instead of 133) | run-97705ac8c0bf | 31.77 | 11.50 | 373 | 77.45 |
| 30 | 96c67ed | Same C code as step 28 | run-5feec0fbae92 | 28.38 | 11.82 | 373 | 78.56 |
| 31 | d9663e9 | Same C code as step 28 (the C4 commit) | run-53e6d6a5450b | 30.78 | 11.80 | 373 | 77.68 |
| 32 | 2a04827 | C6 (68391ae): fingerprint dictionary, 1 MB of 16,384 buckets of 8 entries, AES-hashed whole names, AVX2 compare; 373 → 362 lines | run-fa0a945bb7b4 | 25.49 | 8.97 | 362 | 81.02 |
| 33 | a5424af | C6 (7c8b895): `+prfchw` so the write prefetch compiles to `prefetchw` (neutral on the board) | run-0c2e708e887d | 25.48 | 9.05 | 362 | 80.98 |
| 34 | 2969af0 | Experiment 8c015d1: all 4 KB first touches on main's CPU, against memcg charge batches; reverted | run-2891fc5ceb19 | 25.81 | 10.17 | 405 | 79.97 |
| 35 | 0e6cbba | Back to 7c8b895 | run-a4ff52bbb541 | 25.84 | 8.86 | 362 | 80.92 |
| 36 | 99b3888 | Same C code as step 35 (run ID not recorded) | | 25.34 | 9.00 | 362 | 81.07 |
| 37 | ccff89d | C6 (8509525): bits set by a locked OR on words prefetched for writing; region queues and `apply` removed; 362 → 349 lines | run-02fa2d778aad | 26.13 | 9.24 | 349 | 80.74 |
| 38 | 1f2babf | Same C code as step 37 (the shipped code) | run-52a91432fca0 | 24.94 | 8.63 | 349 | 81.54 |
| 39 | 01feb2f | Experiment ee7290e: the last threads to start populate the 2 MB pages, main runs the thread body; dropped | run-9b9bbe26dc06 | 25.66 | 10.71 | 350 | 80.28 |

**What steps 4 to 16 taught.** The 02:56 code (step 4) ran 43.3-43.7 ms four times. Every later
version ran 46.5-50.9 ms, even with fewer instructions locally. Steps 15 and 16 bisected that on the
board, and the kernel profile found two costs that only the later code had: the 16M-entry region
table (zero-page faults) and the serial cut scan. The line pass of step 7 still paid: 600 → 355
lines was worth more points than the time it lost. Steps 22 and 23 then removed the kernel costs.

## What didn't work

| Idea | Expected | Measured | Why it lost |
|---|---|---|---|
| `lock or` per record into the shared bitset (step 1) | cheap atomics | +20 ms at 12 threads; a plain racy OR +1 ms | the line is usually owned by another core, and a locked op blocks later loads meanwhile |
| Scalar quote-scanning parser | few branches | 1 thread 500-680 ms vs 380 ms | serial dependency on field positions |
| Per-position quote counts in the structural flatten | exact quote pairs per field | +180M instructions | 8 popcounts per block for 3 useful values |
| `copy_within` to compact the position buffer | small copies | ~40 instructions per record in memmove calls | replaced by a ring buffer, later by the shape parse |
| Generic number parser (sign and dot checked for every number) | fewer lines | +240M instructions | a constant `money` flag folds the checks away |
| 1-byte hash tags, 8 per word (SWAR match) | 1 slot check instead of 2.55 | 1.90G vs 1.79G instructions; lookup ~166 vs ~139 per record | the tag scan and claim protocol cost more than the probes saved |
| 3-slot branch-free window before the probe loop | fewer mispredicts | 1 thread: mispredicts 5M → 3-4M; 12 threads: +7% instructions, median 59.7 vs 57.3 ms | at 12 threads the hyper-thread sibling hides mispredicts |
| Lookups right after each parse, no batch | −50 instructions per record | 1 thread −9% instructions but +15-25% cycles; 12 threads 60.8 vs 54.8 ms | the misses are no longer prefetched |
| Batches of 16 or 64 instead of 32 | better prefetch distance | medians 49.3 / 50.7 vs 49.2 ms | flat |
| Double-buffered batches (lookups at least 32 records after their prefetch) | fewer stalls | 403 → 414 instructions per record; cycles 0.71 vs 0.70G | the prefetches already arrive in time |
| 8 chunks per thread | less chunk overhead | 12 threads 51.6 vs 49.4 ms | worse tail balance |
| `#[inline(always)]` on `flush` | no call per batch | +60M instructions; 46.7 vs 44.3 ms | more spills in the big loop |
| 32-byte inline slots instead of a 4-byte table + name arena | one dependent miss fewer | 1 thread cycles equal within noise; LLC misses +27% | bigger footprint; superseded by the 64-byte buckets |
| `MADV_POPULATE_READ` on each input chunk (mmap era) | fewer fault traps | no gain within noise | fault-around already maps 16 pages per fault |
| `MADV_POPULATE_READ` per job on top of M (b16557c) | fewer input faults | page faults 2.5k, but kernel cycles 0.18-0.23G vs M's 0.15-0.16G | not sent to the board; P removed the input mapping instead |
| Parallel unmap of the input before exit (step 14) | save the "7 ms" exit | unmap phase 0.9-1.7 ms, but 12-thread median 49.0 vs 45.4 ms | real exit teardown is only ~2 ms; the barrier and TLB shootdowns cost more |
| `MADV_DONTNEED` after every chunk | spread the unmap | median +13% | 576 calls, each with TLB shootdowns to 11 CPUs |
| One malloc arena (step 13) | less memory | board 48.88 ms / 11.64 MB against 48.30 / 11.46 for the same code | no change beyond noise: the big tables are not malloc'd |
| 256 KB pieces instead of 64 KB (ca99495) | fewer `pread` calls | about +2.4 MB locally (~14.8 MB) | +2.4 MB costs about 0.8 points, which would need 7% less time; not run on the board |
| Mapping the region table writable before the regions phase | fewer faults | no measurable change | dropped |
| 16 KB pieces instead of 32 KB (C3) | less memory | kernel cycles +8% (better in 3 of 20) | more `pread` calls |
| Dictionary of 40,960 buckets (C3) | −0.5 MB | +2.6% instructions, +0.55M branch misses, user cycles +2.9% (2 of 16) | a fuller table has longer probe chains |
| Static build, `+crt-static` (C3) | faster start | startup user+kernel −0.7M cycles, −45 faults, but full runs and memory neutral | never sent to the board |
| Tab lookup and the numeric aggregates at parse time; the batch record keeps only key, start, length, bucket/tag and group (C4) | smaller records, fewer instructions | 384 vs 383 instructions per record; cycles +26% on 1 thread, +18% at 12 | the unprefetched `tab` load stalls each aggregate update: the record traffic was not the cost |
| Long-name prefetch pass in `flush` (C4) | fewer stalls | +9 instructions per record, +0.48M branch misses, user cycles +1-2% | |
| SIMD full-key lookup, no tags (C4) | fewer instructions | −7 instructions per record, cycles ±0 at 12 threads, +0.5M branch misses | |
| Owner-computes bitset, no locks (C4) | no cross-core RMW | what-if with a read-only bitset and no lock: 0-3% | not worth ~40 lines |
| `tab` or the bitset confined to 16 KB (C4, a what-if only) | show whether their misses matter | no change | their misses do not matter |
| `tab` and all bits on 2 MB pages (C4) | ~−5% user cycles | measured | +0.9 MB |
| Private per-thread bitsets (C4) | no sharing | +18% cycles | 36 MB does not fit in L3 |
| Trim `record` with a `KEEP[n]` digit-mask table, `pext` for the dot, an `EDGE` table for the name mask (C5, v2/v3) | −10 instructions per record | 383 → 373-375 instructions; 12-thread user cycles 0.905G / 0.892G against 0.90G (−1%, noise) | the parse is stalled, not instruction-bound |
| The record loop in a call-free `fill()` returning each full batch (C5, v4) | fewer spills | +5 instructions per record (388) | LLVM still spills around ~10 live scalars; the state load and store per batch cost more than they save |
| One-CPU first touches (C6, 8c015d1): main pins itself and takes all 4 KB first touches | memcg per-CPU charge batches stop inflating memory | locally memory.current minus (anon + kernel + file) fell 1.4-2.0 → 0.6-0.8 MB; board 25.81 ms, 10.17 MB, S 405 (+43 lines after rustfmt), 79.97 (run-2891fc5ceb19) | memory went up on the box, not down. Reverted |
| Startup: the last threads to start populate the 2 MB pages, main runs the thread body (C6, ee7290e) | the populate holds the address-space lock that each thread start needs for its stack | counters neutral locally; board 25.66 ms, **10.71 MB**, S 350, 80.28 (run-9b9bbe26dc06) against 24.94 ms, 8.63 MB for 8509525 | memory +1.7 MB for no time. Dropped; the shipped code is 8509525 |
| Names partitioned by hash among threads, each owner doing lookups and bits in its L2, messages via ticketed inboxes (C6) | cache-local lookups | 6 threads +10% cycles, 1 thread +50%; 12 threads livelock with spin waits (3.6 s), +8% with yield | the drain is dominated by unprefetched lookups |
| Bit regions owned by threads, queues appended to the owner's inbox under a mutex (C6) | fewer RFOs | RFO misses 1.36M → 0.79M but L2 demand misses 0.22M → 1.55M; with a prefetch pass -1.3% cycles | noise; not worth the lines |
| `apply` without prefetch, or with a rolling distance-8 prefetch (C6) | | +3.5% / +2% at 1 thread, +2% / +1.5% at 12 | the batch-wide prefetch for write won |
| Prefetch of the previous record's bucket and tab (C6) | | +10 instructions per record, cycles -0.6% at 12 threads | |
| Dictionary of 20,480 or 24,576 buckets (C6) | keys off their home bucket 6.8% → 2.4% / 0.9% | cycles -1.3% / -1.4%, +0.25 / +0.5 MB | net about 0 |
| `tab` on its own 2 MB page; `tab`, thread state and read buffers on a third 2 MB page (C6) | fewer TLB misses | cycles -0.4%; -2..3% but +0.37 MB (the third page is partly used) | STLB hits unchanged (24M, about 7 per record, not from `tab`) |
| Yielding spin barriers instead of `Barrier` (C6) | | kernel cycles x2 under load | |
| `record` not inlined (C6) | fewer spills in the loop | +33 instructions per record, +10% cycles | |
| Threads started as a tree, log2 spawns on the critical path (C6) | shorter ramp | not judged: laptop too loaded (VM load 15-20) | |
| Distinct names counted by the setting thread from the `fetch_or` result (C6) | no end popcount | user cycles +1.9% | LLVM emits a `lock cmpxchg` loop, not `lock bts` |
| Faster end popcount (row sums, prefetch) (C6) | | the phase is only about 60 us per thread; -40 us | not worth a line |
| `pext` for the amount's dot, `batch[nb & 31]` (C6) | | -2 / -1 instructions per record, cycles within noise | |
| Overflow probe to the other bucket of the 128-byte pair first (C6) | L2 fetches pairs | +9 instructions per record (the probe counter leaks into the hot path), cycles +2% | |
| Compact (rank) region table, 0.45 MB instead of 1 MB (C6) | about +0.27 points of memory | not built: about +15 lines and a dependent load | tab misses are already hidden by the prefetch (tab in 16 KB: no change) |
| Batches of 20 or 48 instead of 32 (C6) | | flat | |
| Single-round AES, CRC32 hash, block-level comma queues (C5) | fewer instructions | not built | judged poorer mixing / the same instruction count after the extractions / branchy extraction that mispredicts |

## Measured but not in the shipped build

- **Profile-guided optimisation**, with a profile from generator seeds 21 and 22 at full scale and
  atomic counters (plain counters lost 15-40% of the updates at 12 threads, and that profile made C
  5% slower). On the C2 code: instructions −4.2%, branch misses −2.4% / −6.2%, cycles −0.8% on
  1 thread (16 of 25 rounds won) and −1.1% at 12 threads (18 of 25). Not in the shipped build.

## What is left: the floor, and why we stopped (C4, C5, C6)

**After C6 (68391ae, local, 12 threads, knobs that skip parts of the work; wrong outputs):**

| What-if | User cycles | Instructions per record |
|---|---|---|
| 68391ae | 0.728G | 352 |
| no `apply` (bits not set) | 0.620G (-15%) | 336 |
| no `apply` and no dictionary lookup | 0.521G (-28%) | 307 |
| no tab prefetch | +6.5% (1 thread +16%) | 352 |
| `tab` confined to 16 KB | no change | |

At 1 thread 68391ae runs 0.465G cycles (IPC 2.5); 12 threads cost 0.73G (the hyper-thread sibling
halves per-thread IPC to about 1.6, about 3.2 per core): the parse is throughput-bound per core,
and the dictionary and the bit misses are about 30% of the cycles. The memory floor seen locally is
an empty `main` at 4.43 MB in the bench container; our anonymous memory is about 6.2 MB
(dictionary and bits 4 MB on 2 MB pages, `tab` 0.98 MB, read buffers 0.39 MB, stacks 0.29 MB,
bits tail 0.13 MB). The rest of this section is the C4/C5 analysis at bbee872.

C is stall-bound, not instruction-bound. At 12 threads the parse runs at IPC about 1.5. Perf
annotate (12 threads, bbee872) put about 36% of the parse's samples on its two prefetch
instructions, about 16% of `flush` on the bucket line, about 12% of `flush` on the unprefetched
long-name side store, and the rest in read-for-ownership bursts in `apply`. Cutting 10 instructions
per record (2.6%, C5) moved user cycles by 0 to −1%, which is why the instruction targets of −15%
instructions and −8% cycles are out of reach by trimming. The roughly 190 instructions in `record`
itself: numbers ~40, key, hash and bucket ~30, name scan ~22, quote accounting ~25, spills ~30, the
rest the 32-byte name window, the backward comma search and the 80-byte `Rec` stores.

Where the 12 threads' user cycles go (bbee872, local, knobs that skip parts of the work):

| Part | User cycles | Instructions per record |
|---|---|---|
| `pread` only | 0.013G (kernel: 65 ms CPU for the copy, ~37 GB/s aggregate) | |
| Block scan | 0.05G | +26 |
| Record parse | 0.46G | 235 |
| Dictionary lookup | +0.35G | +93 |
| Aggregates | ~0 | +23 |
| Bit queue and apply | +0.2G | +46 |
| Total | 1.0G | 399 |

Speed-of-light floor for this box (6 P-cores with HT, 2.5 GHz), against the estimated time at
bbee872:

| Phase | Floor | bbee872 (est.) |
|---|---|---|
| exec → main | 0.3 ms (static) | ~0.5 |
| regions and spawns | 0.3 | 2-3 (two spawn rounds, mmap-lock contention) |
| thread ramp | ~0 | 1.5-2.5 |
| input, 202 MB at ~45 GB/s | ~4.5, overlapped with compute | copy ~5 ms per thread |
| per-record work, ~170 instructions at ~3.5 IPC | ~11 | 399 instructions, ~22 |
| merge, output, exit | 0.5 | ~1-1.5 |
| **Total** | **~12-13 ms** | 30.4 |

The gap was about half per-record instructions and about 15% startup; C3 and C4 took the startup
part (the single pool, the 2 MB pages). Getting to the floor needs about 200 instructions per
record, and restructuring the pipeline around the same parse cannot reach that. It needs a new parse
kernel, more than the hours we had. The design we would try: tab lookup and the numeric aggregates
at parse time, the name lookup alone deferred through a small ring, a full-key SIMD bucket compare.

## Correctness

- Every pushed version matches the published sha256 on the worked example, the four tiny and four
  sample seeds, and full seed 1 at 1, 2, 7 and 12 threads, and matches the reference
  implementation on the unseen generator seeds 11 and 12. No fallback warning appears on any of
  them.
- **What the shipped parser relies on.** The spec lets a solution rely on section 2.2 (what the
  generator writes) as well as 2.1. The C2 parser uses four of those guarantees:
  - every record ends with a terminator (the file always ends with CRLF);
  - an id or region id has at most 8 digits (region ids are at most 9,999,999; ids rise by at most
    11 per record over 3.32 million records);
  - an amount has at most 7 digits of cents (the range is −999.99 to 99,999.99);
  - no CR appears except in a CRLF.

  Outside these it is not exact. We checked on bbee872: a 9-digit id is read as its first 8 digits
  and shifts the field ends, a 9-digit region id reads as unmatched, 12345678.90 reads as 0.00,
  −123456.78 reads as −23456.78, and a final record without a terminator is dropped.
- **Torture test** against the reference implementation at 1, 2, 3, 5, 8, 13 and 31 threads, so
  chunk cuts land everywhere. Its 12 files include what spec 2.1 allows but the generator never
  writes: unquoted text; ids, regions and amounts longer than 8 digits with leading zeros; names of
  16/17/24/25 bytes and names full of `""`; NUL, CR, TAB and BOM bytes inside quotes; 200-byte
  notes and notes of 70 commas; random BOMs; mixed terminators; a missing final terminator; and
  two "guess-breaker" files whose notes hold an LF followed by text that looks like a whole record
  (they must trigger the single-thread rerun).
  - The first parser (the 04:47 state, with a slow path for long numbers and a virtual LF at the
    end of the file) passed all 84 runs.
  - The shipped C2 parser passes the three worked-example variants that stay inside 2.2 (21 runs)
    and fails the other 9 files at every thread count (63 runs). Each of those files has numbers
    beyond the limits above or an unterminated last record; we did not look for further causes.
    In the guess-breaker files the boundary proof still caught the false record starts and reran
    on one thread, as designed.

## How to reproduce

```
# build exactly as the board does and check the full set against its expected hash
python3 check.py c --data data/c --expected <sha256>

# our bench: builds the image, runs it with the board's container flags (read-only root,
# /out on tmpfs, user 65534), prints wall time inside the container and memory.peak
python -I tools/bench.py c --gate G1                         # worked example + 4 tiny seeds
python -I tools/bench.py c --gate G2 --no-build              # 4 sample seeds
python -I tools/bench.py c --gate G3 --runs 5 --no-build     # full seed 1, 5 runs after a warm-up
python -I tools/bench.py c --gate G3 --threads 7 --no-build  # the same with FW_THREADS=7
```

The data comes from the public generator (`c_join.py --seed 1 --scale full`). `FW_THREADS` sets
the thread count; the default is the available parallelism.
