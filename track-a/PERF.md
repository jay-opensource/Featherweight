# Track A: performance log

Every step from the reference to the shipped version, with the live-board run ID that measured it.
Local numbers come from a laptop and are labelled as such. We used them to choose between variants;
the run IDs are the record.

## Result

| | Run ID | Commit | T | M | S | Points |
|---|---|---|---|---|---|---|
| Reference (Python) | | | 32.047 s | 228 MB | 56 | 0 |
| First correct entry (v1) | run-4bc3f917569d | fe231bc | 32.3 ms | 19.3 MB | 486 | 78.6 |
| **Best** | **run-ac86c3ef1a30** | **477f874** | **18.92 ms** | **20.45 MB** | **440** | **84.48** |
| Shipped (same code paths, 3 lines fewer) | run-9237e503e115 | bbee872 | 19.15 ms | 20.26 MB | 437 | 84.41 |

The best run is 1,694 times faster than the reference with 11.1 times less memory. Points are the
board's: `50·log10(K)` with K = (T_ref/T)^0.5 × min(50, M_ref/M)^0.2 × min(1, S_ref/S)^0.15. At
about 20 ms, 1% of T is worth about 0.11 points, 1% of M about 0.04, and 10 lines about 0.07.

## How we measured

**The board.** One run per push on the scoring box: an i5-13500 with 12 threads at 2.5 GHz,
Debian 13 (kernel 6.12), transparent huge pages "always", and `/out` on a tmpfs, so the 10.1 MB
`summary.txt` is charged to memory. S is the code lines of `src/main.rs` after rustfmt plus the 7
lines of the Dockerfile. Identical A code gave these times, so a single run resolves about ±4%:

| A code | Board runs (ms) |
|---|---|
| v2 (e3964b4) | 32.18, 32.02, 32.08 |
| r5-r7 (cc9faa7) | 22.26, 22.32, 22.29 |
| r9 (8c1f26b) | 21.78, 23.21 |
| r10 (5b3321f) | 22.79, 21.80 |
| start of the A2 line (b1f2ed7) | 20.95, 19.96 |
| shipped code paths (477f874, bbee872) | 18.92, 19.15 |

**Local.** An i5-11400H laptop shared with other teams' benchmarks; its wall times move 2-3× with
the load. What we used:
- **Early steps (v0 to r9):** a runner with the board's container flags (`--read-only`, `/out` as
  a tmpfs, user 65534, `memory.peak` read inside the container); A and B run in alternation in
  one session, minimum over N runs. Instruction counts from `valgrind --tool=cachegrind` on one
  thread and the full seed-1 file (3.1M lines).
- **The A2 line (from r9 on):** an instrumented copy of `main.rs` in which each thread opens
  `perf_event_open` counters (cycles, instructions, branch misses, LLC misses) and reads them at
  every phase boundary. Variants run interleaved (A B B A) and are compared by median and by
  per-round paired ratio. Instruction counts are deterministic, and user cycles agree within about
  2% between rounds. `perf record` (privileged, 20-40 runs aggregated) shows where the cycles go,
  kernel included.

## What the program does, and where the time goes

One parse pass turns every line of `requests.log` (239 MB, 3.1M lines) into a 4-byte record filed
in its hour's memory region. Then, hour by hour: count per endpoint, scatter the latencies in name
order, select p50/p95/p99, and keep six words per group where the records were. Last, the output
is sized, mapped and formatted in parallel. The README describes the design in prose.

**Per-phase profile** (local, 12 threads, full seed 1, medians of 6-8 runs; cycles summed over
threads; "u+k" adds kernel cycles):

| Phase | User cycles, r9 | User cycles, shipped | × r9 | Instructions, shipped | u+k cycles, shipped | × r9 |
|---|---|---|---|---|---|---|
| parse | 365M | 278M | **0.76** | 447M | 406M | 0.79 |
| count | 31M | 15M | **0.47** | 34M | 17M | 0.52 |
| scatter | 37M | 26M | **0.69** | 56M | 32M | 0.73 |
| select + size | 89M | 70M | **0.78** | 122M | 75M | 0.78 |
| format | 32M | 30M | 0.92 | 41M | 57M | 0.95 |
| **total** | **557M** | **419M** | **0.750** | 701M | **599M** | **0.784** |

Thread CPU time fell from 230 to 171 ms. At 1 thread, user cycles fell to 0.743 of r9.

**Where the cycles go in the shipped code** (`perf record`, 12 threads, user + kernel):
- **Kernel, about 32%,** almost all per-page work rather than contention: input fault-around 6.2%,
  the input zap 3.8% plus helpers, anonymous and tmpfs first touch about 7%, and the page-table
  spinlock 2.3-2.6% in total.
- **Parse, about 40%:** the interner (slot probe and name compare) about 15%, the separator load
  5.8% (13.4% before the prefetch of mechanism 4), the latency SWAR about 6%.
- **Select** 6%, then count, scatter and format.
- **Exit** does not show: `exit_mmap` is below 0.05%.

**The parse is memory-bound, not instruction-bound.** Removing 6-7% of its instructions (an SSE
latency parse, see the failures) left cycles flat or worse; a second prefetch per line cut its
cycles by 8%.

**Memory** (local `memory.current`, full seed 1, measured on r9; the A2 changes left the local peak
at 18.4-19.4 MB, 10-run medians):

| Moment | Contents | MB |
|---|---|---|
| End of the parse | 3,136,447 record words (12.55 MB), block slack, the name copies (0.15 MB), page tables and the base | 16.7-18.0 |
| The peak, early in the stats phase | the end of the parse plus 12 scatter buffers (touched almost at once), minus the first hours' records | +0.3-0.8 |
| After the stats phase | six words per group in place of the records | 7-8.7 |
| End | the 10.1 MB output on tmpfs; statistics released hour by hour | 13.6-15.4 |

The board reads 20.3-20.9 MB, 1-2 MB above the local peak; we have not broken that gap down. Below
this, only smaller records would help: streaming hours during the parse would save at most about
2 MB, because the 10.1 MB output still accumulates to the end.

## The mechanisms, each with the measurement behind it

**1. One pass to 4-byte records, one region per hour (v1).** A record is an 11-bit endpoint id, a
5xx bit and a 20-bit latency. Records go to their hour's region in blocks of 128 words taken from
a per-hour counter. The 72 regions are one large reservation, charged only where touched. Against
v0 (per-hour `Vec`s of (id, latency, 5xx)), locally 232 → 29 ms (min) and 50-51 → 20-23 MB.

**2. Statistics in the place of the records, output mapped (v2).** Each hour's six words per group
are written over its dead records, and the rest of the region goes back to the kernel. The output's
exact size is then known, so `summary.txt` is sized, mapped `MAP_SHARED` and formatted by all
threads at once. Locally 69 → 57 ms and 20.6-24.7 → 17.2-18.3 MB. On the board v2 ran 32.0-32.2 ms
and 19.1-19.7 MB, the same as v1 (steps 1-4 below).

**3. A parse without byte loops (r5, r9).**
- **Separators:** one AVX2 compare of the 64 bytes from the method's third byte gives a mask of
  spaces and LFs; the first ends the method, the second the endpoint.
- **Hour:** one 8-byte load of "MM-DDTHH", the digit pairs combined in one multiply. Lines come in
  long runs of one hour, so since r9 the hour and its record cursor stay in registers until that
  word changes (758M → 715M instructions).
- **Latency:** a SWAR test for the first non-digit gives its length, then three multiplies.
- **Endpoint id:** a CRC32 of the length, the first 16 and the last 8 bytes picks a slot in a
  shared table of 8,192 64-bit slots (hash, length, id, and where the name's copy is). The name is
  compared against that copy, padded to whole 64-byte lines, so a compare reads one line for most
  names and never bytes another thread may be writing. A new name is added under a lock, 1,842
  times in all.

Parse instructions on the sample (whole program, one thread): an SSE2 128-byte mask that also found
the LF 164M; the same in AVX2 144M; a 64-byte mask for the endpoint only, SWAR length, CRC32 and one
cursor per hour 129M. On the full file, 1,035M (v2) → 796M (r5).

**4. Prefetch two lines 1 KB ahead.** The hardware prefetcher stops at every 4 KB page, and page
cache pages are scattered, so each new page started with demand misses: the separator load was
13.4% of all cycles. One `prefetcht0` at p + 1024 per line: parse cycles ×0.81 (12 threads, 8 of 8
rounds at 0.78-0.82). Lines average 77 bytes, so one prefetch per line skipped about one cache line
in six; a second at p + 1088: parse ×0.918, total ×0.952. On the board, one prefetch took r10 from
21.80-22.79 ms to 21.49 ms (run-0658770c8cc4), and the second took the A2 line from 20.69 ms to
19.15 ms (run-91feae67de7b).

**5. Chunks in a strided order.** The input is cut into 16 line-aligned chunks per thread, and the
claim order sends concurrent threads to chunks far apart. Before, they parsed neighbouring chunks,
so they shared input page-table pages and filled the same hours' regions. The page-table spinlock
fell from 4% to 2.6% of cycles and thread CPU time to ×0.94.

**6. Records that stand alone.** Latencies from ESC = 2^20 − 2048 up are escapes: the record keeps
the latency's low 11 bits, and the next word (an escape word with endpoint id 0) holds the rest.
The parse stores the escape word every time and advances by one word, or by two when it is needed.
The count and scatter loops then step one word at a time; before, their stride `1 + escape` was a
load-to-load chain of about 7 cycles per word. Escape words and block tails have id 0, so they
count and scatter like padding, after the real groups. Count ×0.47, scatter ×0.70, total ×0.946,
and 7 lines fewer.

**7. Percentiles without branches for small groups.** 90% of groups have n ≤ 32, and sorting them
cost 1.26M branch misses. Now every value counts, in AVX2 lanes, how many values are below and
above it, and the value at rank r is one with "below ≤ r" and "above ≤ n − 1 − r". One, two or four
vectors cover n ≤ 8, 16 and 32. Larger groups use three narrowing `select_nth_unstable` calls, each
above the previous rank. Select ×0.80, branch misses ×0.56. Board: 20.69 ms (run-9cae0f41d214),
within the noise of the 19.96-20.95 ms before it.

**8. Exit work done inside the parallel phases (r7, r8).** At exit the kernel unmaps every page of
the process on one core. Each thread now `MADV_DONTNEED`s the input pages of each chunk it finishes
(the input is mapped with `MADV_SEQUENTIAL`), and each hour's output pages after formatting them
(the data stays in the file). The process then calls `exit(0)` without freeing anything. Locally:
the input zap took 57.6 → 48.1 ms; without the output zap the median rose from 32.3 to 38.4 ms. In
the shipped profile `exit_mmap` is below 0.05% of cycles.

**9. Small codegen wins.** The scatter store is unchecked (its index is below the hour's word count,
which is at most the buffer's length), and output digits are written two at a time from a table of
"00" to "99": scatter ×0.875, format ×0.90, total about ×0.985.

## Findings specific to the scoring box

- **THP "always" and memory.peak.** The box backs anonymous memory with 2 MB pages, and
  `memory.peak` charges a whole 2 MB page for its first touched byte. All big buffers are our own
  `mmap` with `MADV_NOHUGEPAGE` (and `MAP_NORESERVE`), so only touched 4 KB pages count. Thread
  stacks are 256 KB and malloc keeps one arena, so neither can cost a 2 MB page.
- **The tmpfs output is memory.** `/out` is a tmpfs, so the answer itself is charged. The output is
  formatted straight into the mapped file, so it exists once. Opened write-only, the shared mapping
  fails with EACCES; our fallback then formatted into anonymous memory and wrote that, and the end
  of the run held 24.8 MB instead of 14.1 MB. The file is opened read-write.
- **The page-table lock.** In r9's profile the page-table spinlock was about 4% of cycles:
  fault-around on input page-table pages shared by neighbouring chunks, faults on record pages that
  all threads were filling, and the zap. Strided chunks (mechanism 5) cut it to 2.3-2.6%.
- **pread against mmap.** Reading each chunk into a 64 KB per-thread buffer (no input mapping, no
  zap) lost on the board: 23.21 ms (run-51d1a447cef1) against 21.49-21.98 ms for the mapped
  versions around it. Locally its parse cost ×1.25 user + kernel cycles: the kernel copy ran at
  about 2 GB/s per thread. The same change won for Track C, so each track keeps what the board
  measured.
- **2 MB page-cache folios (a probe).** If the box's page cache held 2 MB folios, a 2 MB-aligned
  input mapping with `MADV_HUGEPAGE` would map each 2 MB with one entry and shrink fault-around and
  the zap about 500×. On the board it was neutral, 19.14 ms (run-7a659e0640c9) against 19.15 ms
  without it, so it was dropped (run-ac86c3ef1a30, 18.92 ms).

## Board runs

"Same code" rows re-ran unchanged A code (another track had changed); they sample the noise.

<!-- New runs: append a row with the next step number, and update the Result table above. -->

| Step | Commit | Change | Run ID | T (ms) | M (MB) | S | Points |
|---|---|---|---|---|---|---|---|
| 0 | | The reference, unchanged | | 32,047 | 228 | 56 | 0 |
| 1 | fe231bc | v1 (10c1060): one pass to 4-byte records per hour region; per hour count, scatter, three selects; output written in place through a mapping | run-4bc3f917569d | 32.3 | 19.3 | 486 | 78.6 |
| 2 | e3964b4 | v2: statistics over each hour's dead records; output sized, mapped and formatted in parallel | run-f976695475b9 | 32.18 | 19.09 | 545 | 78.31 |
| 3 | 2d45234 | Same code | run-d625fdc7de86 | 32.02 | 19.73 | 545 | 78.23 |
| 4 | 8cba48c | Same code | run-60004f20b3a1 | 32.08 | 19.35 | 545 | 78.29 |
| 5 | cc9faa7 | r5-r7: AVX2 separator mask, CRC32 interner over 64-byte padded names, one thread pool with barriers, count and 5xx in one u64, sort for groups ≤ 32, input pages released per chunk, largest hours first, static binary | run-b71860836de5 | 22.26 | 20.85 | 451 | 82.55 |
| 6 | bd5f56e | Same code | run-c9ac45f43750 | 22.32 | 20.19 | 451 | 82.66 |
| 7 | cdf5158 | Same code | run-1fddb2573562 | 22.29 | 20.50 | 451 | 82.61 |
| 8 | 64d75ea | 0750a4e, r8: output pages released per hour; `exit(0)` once the output is complete; 451 → 433 lines | run-c78d1b835aaa | 22.34 | 20.77 | 433 | 82.66 |
| 9 | bb33a01 | Same code | run-778444731d9e | 22.44 | 20.67 | 433 | 82.63 |
| 10 | 8c1f26b | r9: the hour and its record cursor stay in registers while "MM-DDTHH" repeats | run-00d7cf56087c | 21.78 | 20.56 | 438 | 82.94 |
| 11 | c8c57cd | Same code | run-2f9af774ebe7 | 23.21 | 20.00 | 438 | 82.37 |
| 12 | 5b3321f | r10: the leader sorts the names while the others start on the hours | run-71486cd03462 | 22.79 | 20.00 | 441 | 82.55 |
| 13 | 1caefee | Same code | run-fef0d786ee5b | 21.80 | 20.16 | 441 | 83.00 |
| 14 | 6a59621 | r10 + one prefetch 1 KB ahead per line | run-0658770c8cc4 | 21.49 | 20.14 | 442 | 83.15 |
| 15 | 43abc73 | r11 (r10 without the prefetch): two parse cursors per thread; `fdatasync` before exit | run-5ff4dc6de742 | 21.98 | 20.02 | 439 | 82.95 |
| 16 | d952819 | Same code | run-33662b5b17ce | 21.91 | 20.96 | 439 | 82.78 |
| 17 | fcc5579 | Experiment: `pread` each chunk into a 64 KB per-thread buffer (no input mapping, no zap); reverted | run-51d1a447cef1 | 23.21 | 20.85 | 456 | 82.06 |
| 18 | b1f2ed7 | The A2 line, built on r9: one prefetch 1 KB ahead, strided chunks, escape records that count and scatter like padding | run-8ab1ca411c7e | 20.95 | 20.73 | 433 | 83.37 |
| 19 | 79c2804 | Same code | run-b097136a42f5 | 19.96 | 20.58 | 433 | 83.92 |
| 20 | 65ff880 | 34f8892: branch-free AVX2 ranks for groups of 32 or fewer | run-9cae0f41d214 | 20.69 | 20.09 | 436 | 83.62 |
| 21 | a50cee9 | Two prefetched lines per input line | run-91feae67de7b | 19.15 | 20.37 | 438 | 84.38 |
| 22 | d163280 | Unchecked scatter store, two output digits at a time; probe: input mapped 2 MB-aligned with `MADV_HUGEPAGE` | run-7a659e0640c9 | 19.14 | 20.52 | 442 | 84.32 |
| 23 | 477f874 | The probe dropped (neutral) | run-ac86c3ef1a30 | 18.92 | 20.45 | 440 | 84.48 |
| 24 | bbee872 | A2 final (9c0770e): three lines fewer, same code paths | run-9237e503e115 | 19.15 | 20.26 | 437 | 84.41 |

**r10 and r11 are not in the shipped code.** Both ran within the noise of r9 (steps 12-16). The A2
line was built on r9 instead and beat both (steps 18-23), so the shipped parse has one cursor per
thread and the name sort waits at the barrier.

**Local measurements of the early versions** (min over N runs of an alternated A/B in one session;
instructions from cachegrind on one thread and the full file):

| Version | Local A/B (min ms) | Local M (MB) | Instructions |
|---|---|---|---|
| v0: per-hour `Vec`s, then sort every group | 232 | 50-51 | |
| v1 | 29 (median 41) | 20-23 | 1,168M |
| v2 | v1 69 → v2 57 | 20.6-24.7 → 17.2-18.3 | 1,035M |
| r5 | v2 46.6 → r5 35.4 | 18.1 | 796M |
| r6: packed counters, unconditional scatter, sort ≤ 32 | r5 35.4 → r6 30.2 | 18.1 | 757M |
| r7: input zap, 128-word blocks, largest hours first, static | r6 59.9 → 57.6 (blocks, order) → 48.1 (zap) → 46.8 (static) | 18.0 | 761M |
| r8: output zap, `exit(0)` | r7 30.6 → r8 28.3 | 18.0 | 758M |
| r9: hour in registers | r7 30.7, r8 28.9, r9 26.3 | 17.8 | 715M |

At r7, about 150 instructions per input line went to the parse, about 70 per record to the stats
and about 15 per record to formatting.

## What didn't work

| Idea | Expected | Measured | Why it lost |
|---|---|---|---|
| `File::create` plus a shared mapping of the output | output written in place | EACCES; the fallback wrote from anonymous memory: 24.8 MB at the end instead of 14.1 | a write-only fd cannot back a shared writable mapping; fixed with a read-write open |
| Per-hour heap buffers plus an ordered writer (v1) | simple output | 16-24 ms of CPU flushing, mostly `munmap` TLB shootdowns while 12 threads ran | replaced by the mapped output (v2) |
| `writeln!` formatting | | 75-95 ms of a 146 ms phase-2 CPU | a hand-written itoa: about 15 instructions per record |
| Comparing names against their first occurrence in the input | no copy | the padded copies cut interning-only parse time 100 → 83 ms (min, 1 thread) | those input bytes are spread over 239 MB and cold |
| A 128-byte u128 separator mask that also found the LF | one mask per line | about 30 uops per line in bit-clear and tzcnt; 144M vs 129M instructions on the sample | a 64-byte endpoint mask plus a SWAR length instead |
| Scalar `position()` for endpoints past the 64-byte window | rare case, simple code | 6.6% of lines; 22 instructions per line in iterator code | continuing the SIMD window: 595M → 503M parse instructions |
| `select_nth_unstable` on tiny groups | | sort threshold 0/8/16/32: 772/762/759/757M instructions | sort ≤ 32, later the branch-free ranks |
| Freeing record pages during the scatter | lower peak | no lower peak possible | the peak is set at the end of the parse, and the scatter buffers are touched almost at once |
| Per-(endpoint, bucket) counting or a histogram multi-select | fewer passes | rejected on paper: +0.8-1.5 MB of counters at the peak, or three partition passes | |
| A custom `GlobalAlloc` advising `MADV_NOHUGEPAGE` | | not needed | every big buffer is our own `mmap` with the advice, and no heap allocation reaches 2 MB; −23 lines |
| Three `thread::scope`s (36 spawns) | | | one pool with barriers |
| `MADV_POPULATE_READ` per input chunk plus `MADV_POPULATE_WRITE` on the scatter buffers and output ranges | one syscall instead of many faults | wall median 36.1 vs 33.8 ms, sys CPU 90 vs 75 ms (10 alternations); repeated on the A2 line: thread CPU ×1.175 | worse |
| A 16 KB interner table of 32-bit slots, to stay in L1 | fewer misses | D1 misses −5.6%, instructions +6M; the A/B flipped sign when the order was swapped | |
| Escapes in one shared side list | a fixed stride in count and scatter | the loops saved 17M instructions, the per-hour list scan cost 10M: −1% for +145 KB and +4 lines | mechanism 6 does it without a list |
| Two parse cursors per thread (r11, step 15) | overlap two dependency chains | locally 7-12% faster per thread; board 21.98 / 21.91 ms, within the noise of r9 | not carried into the A2 line, which won on the board |
| `pread` into a 64 KB buffer per thread (step 17) | no fault-around, no zap | board 23.21 ms vs 21.49-21.98; locally parse u+k ×1.25 | for this track the kernel copy costs more than the mapping it saves |
| 2 MB-aligned input with `MADV_HUGEPAGE` (step 22) | 500× fewer fault-around entries if the page cache holds 2 MB folios | board 19.14 vs 19.15 ms | neutral; dropped |
| Prefetch distance 512, 1536 or 2048; two lines 32 B apart | | within 1.5% of 1024; +2% | |
| Prefetch in the count loop (256 or 512 words ahead) | | ±1% | noise |
| Touching the next 64 KB fault-around window early | faults ahead of the parse | ×1.00-1.01 | |
| Branch-free hash masks (no `n >= 8` / `n >= 16` branches) | fewer mispredicts | parse instructions ×1.05, cycles ×1.036, branch misses unchanged | |
| SSE latency parse (length and value) | fewer instructions | instructions ×0.93, cycles ×1.02; a hybrid ×0.94 / ×0.99, worse on 1 thread | the length moves onto the line-to-line dependency chain |
| Interner with 4,096 or 16,384 slots | | parse ×1.034 / ×1.006 | 8,192 kept |
| Name copies indexed by slot, so the compare does not wait for the slot | a shorter chain | parse ×1.032, +0.5 MB | |
| Auto-vectorised rank select (plain loops over lanes) | fewer lines than intrinsics | instructions ×1.20 / ×1.36, cycles worse | |
| Sorting before the ranks for small n (thresholds 0, 8, 16) | | select ×1.09 / ×1.06 / ×1.03 against ranks up to 32 | |
| Ranks with 4 vectors only (no 1- and 2-vector forms) | −10 lines | select ×0.86 vs ×0.79-0.80 | |
| Ranks with 8 vectors for n = 33-64 | | select ×0.99, +4% instructions | |
| Page-sized record blocks (1,024 words), one owner per page | no shared first touch | cycles ×1.00-1.01; peak up to +3 MB | each thread touches 50-70 hours |
| `pwrite` from a 32 KB buffer per thread instead of the mapped output | | format cycles ×2.05, CPU ×2.5 | tmpfs writes to one file serialise on the inode lock |
| `MADV_POPULATE_WRITE` per output hour range | | format cycles ×0.99, CPU ×1.19 | |
| PGO (profile from generator seeds 21 and 22) | a few % | instructions −1.0%; cycles +1.7% on 1 thread (1 of 12 rounds won), +1.9% at 12 threads (1 of 15) | slower |

## Correctness

Checked on every kept change:
- the worked example, the 4 tiny and 4 sample seeds and full seed 1, by sha256 against the published
  hashes, at 1, 2, 7 and 12 threads and the default count; the unseen generator seeds 11 and 12;
- 4 synthetic files against an independent reference at 1, 3 and 12 threads: an empty input (a
  0-byte output); the 177-byte longest line (OPTIONS, the 125-byte template, status 599, latency
  4294967295); one hour of 60,000 lines over 1,513 endpoints of awkward lengths (1, 8, 16, 17, 63,
  64, 65, 125) with every method, escapes, 0 and u32::MAX, and statuses 499/500/599; all 72 hours
  fully shuffled, with duplicates, both span ends and both sides of every hour boundary;
- 20k lines with latencies at every escape boundary (0, 1, 2047, 2048, ESC ± 1, 2^20 ± 1,
  99,999,999, 10^8, 2^31 ± 1, 2^32 − 1, 2^32 − 2048 ± 1) and statuses 200/404/499/500/599;
- 151k lines in 4,320 groups of n ∈ {1..9, 15-17, 19-21, 31-34, 40, 64, 100, 101, 257}, drawn from
  tie-heavy pools ({0, u32::MAX}, {5, 5, 5, 7}, 0-9, around 2^31, around ESC) and random values,
  fully shuffled, against an independent nearest-rank reference.

Design points that carry correctness:
- **Endpoint identity:** an exact byte compare of the whole name; the CRC only picks the slot.
- **Escapes:** latencies from ESC up keep their low 11 bits in the record and the rest in the
  escape word, so every u32 latency is exact.
- **Percentiles:** nearest rank as `(q·n + 99)/100 − 1`, zero-based.
- **5xx:** the status's first digit is `5`, so 499 is not an error and 500-599 are.
- **Regions:** sized for the worst case, every line in one hour at two words each.

## How to reproduce

```
# build exactly as the board does and check the full set against its expected hash
python3 check.py a --data data/a --expected <sha256>

# our bench: builds the image, runs it with the board's container flags (read-only root,
# /out on tmpfs, user 65534), prints wall time inside the container and memory.peak
python -I tools/bench.py a --gate G1                         # worked example + 4 tiny seeds
python -I tools/bench.py a --gate G2 --no-build              # 4 sample seeds
python -I tools/bench.py a --gate G3 --runs 5 --no-build     # full seed 1, 5 runs after a warm-up
python -I tools/bench.py a --gate G3 --threads 7 --no-build  # the same with FW_THREADS=7
```

The data comes from the public generator (`a_ingest.py --seed 1 --scale full`). `FW_THREADS` sets
the thread count; the default is the available parallelism.
