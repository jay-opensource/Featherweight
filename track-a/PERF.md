# Track A: performance log

Every step from the reference to the final version. Each row has the live-board run ID that measured it.

## How we measured

**Board.** The scoring box: i5-13500, 12 threads at 2.5 GHz, THP "always", and `/out` on a tmpfs, so the 10.1 MB `summary.txt` is charged to memory.

**Local T and M.** These come from a runner that matches the board's container flags (`--read-only`, `/out` as a tmpfs, user 65534, `memory.peak` read inside the container).
- The laptop is an i5-11400H shared with other teams' benchmarks. Its wall times move 2-3x with the load, so local times are only comparable **within one row's A/B**, which ran in alternation in one session.
- We quote the minimum over N runs.

**Instructions.** A deterministic proxy: `valgrind --tool=cachegrind`, one thread, the full seed-1 file (3.1M lines), whole program.

**S.** Code lines of `src/main.rs` after `rustfmt --edition 2021`. Blank lines and comments are not counted.

## Steps

| Step | Change | Run ID | Board T (ms) | Board M (MB) | S | Local A/B (min ms) | Local M (MB) | Instr. (M) |
|---|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32,047 | 228 | 56 | | | |
| 1 | **v0, correct baseline:** threads parse their chunks into per-hour `Vec`s of (id, latency, 5xx); then sort every group | | | | | 232 | 50-51 | |
| 2 | **v1, one pass to 4-byte records:** id11 / 5xx bit / latency20, plus an escape word. Records go to one memory region per hour, in 2 KB blocks taken from an atomic counter. Then per hour: count, scatter, `select_nth_unstable` ×3, format into a per-hour buffer, ordered writer | | 32.3 | 19.3 | 486 | 29 (median 41) | 20-23 | 1,168 |
| 3 | **v2, statistics in place of records, mapped output.** Each hour's six words per group are written over its dead records and the rest of the region is released. Then the output is sized, mapped `MAP_SHARED` (opened read-write) and formatted in parallel by hour | | 32.2 / 32.0 | 19.1 / 19.7 | 545 | v1 69 → v2 57 | 20.6-24.7 → 17.2-18.3 | 1,035 |
| 4 | **r5, parse rewrite.** AVX2 separator mask from byte 31; latency length by a SWAR non-digit test; CRC32 hash; names compared against a 64-byte-padded arena; one thread pool with barriers instead of three spawns; 2-digit LUT itoa; instrumentation removed | | 22.3 | 20.8 | 451 | v2 46.6 → r5 35.4 | 18.1 (v2 17.2) | 796 |
| 5 | **r6.** Count and 5xx count packed in one u64 per endpoint; unconditional scatter (padding words go to a dummy id); sort groups of ≤ 32 instead of three selects | | | | 437 | r5 35.4 → r6 30.2 | 18.1 | 757 |
| 6 | **r7, exit work moved into the parallel phase.** Each thread `MADV_DONTNEED`s the input pages it has parsed, with `MADV_SEQUENTIAL` on the input. Also B = 128, largest hours first, `+crt-static` | | | | 444 | r6 59.9 → +B128/LPT 57.6 → +zap 48.1 → +static 46.8 | 18.0 | 761 |
| 7 | **r8.** Each hour's output pages are dropped from our page table after formatting (the data stays in the file); `process::exit(0)` once the output is complete; rustfmt-friendly trims | | | | 426 | r7 30.6 → r8 28.3 (medians 38.6 / 38.0) | 18.0 | 758 |
| 8 | **r9.** The parser keeps the current hour and its record cursor in registers while the timestamp's "MM-DDTHH" repeats. Lines come in long single-hour runs, so both are recomputed only when it changes | | | | 431 | r7 30.7, r8 28.9, r9 26.3 (medians 39.3 / 37.7 / 32.3) | 17.8 | 715 |

### Inside step 4: parse instructions

Measured on s2-sample, whole program, one thread.

| Variant | Instr. (M) |
|---|---|
| SSE2, a 128-byte u128 mask that also found the LF | 164 |
| The same with AVX2 | 144 |
| 64-byte mask for the endpoint only; SWAR digit length gives the line end; CRC32; a one-pointer cursor per hour | 129 |

On the full file, step 4 went 1,035M (v2) → 820M (long endpoints continue the SIMD scan, fewer bounds checks) → 796M (r5).

## Where the time goes

r7, inside the process. The laptop was loaded, so read these as proportions:
- parse: 55-60%;
- stats (count, scatter, select): 25-30%;
- formatting into the mapped output: 8-10%;
- exec and exit: the rest.

Instructions per input line: about 150 in the parse, about 70 per record in the stats phase, about 15 per record in formatting.

## Where the memory is

Seed-1 full, local `memory.current`:

| Moment | Contents | MB |
|---|---|---|
| End of the parse | All 3,136,447 record words (12.55 MB), block slack, the name arena (0.15 MB), page tables and the base | 16.7-18.0 |
| The peak, early in the stats phase | The end of the parse plus 12 scatter scratch buffers (touched almost at once), minus the first hours' records | +0.3-0.8 |
| After the stats phase | Six words per group in place of the records | 7-8.7 |
| End | The 10.1 MB output on tmpfs, statistics released hour by hour | 13.6-15.4 |

Below this, only smaller records would help. Streaming hours during the parse would save at most about 2 MB, because the 10.1 MB output still accumulates to the end.

## Failure museum

These are rejected or reverted experiments, with the numbers that decided them.

1. **`File::create` plus `MAP_SHARED`.**
   - The file is opened write-only, so the shared mapping fails with EACCES.
   - Our fallback then formatted into anonymous memory and `write()`-ed it, so the output was charged twice: 24.8 MB at the end instead of 14.1 MB.
   - Found with a `memory.current` probe after each phase. The fix is a read-write open.
2. **Per-hour heap buffers plus an ordered writer (v1).**
   - Flushing cost 16-24 ms of CPU, mostly `munmap` TLB shootdowns while 12 threads ran.
   - Replaced by statistics in the dead region plus one mapped output (step 3).
3. **`writeln!` formatting.** It was 75-95 ms of the 146 ms phase-2 CPU. Replaced with a hand-written itoa: the format phase is now about 15 instructions per record.
4. **Comparing names against their first occurrence in the input.** Those bytes are scattered over 239 MB and cache-cold. The 64-byte-padded name arena cut interning-only parse time from 100 to 83 ms (min, one thread).
5. **A 128-byte u128 separator mask that also found the LF.** The u128 bit-clear and tzcnt chain alone was about 30 uops per line. Replaced by a 64-byte mask for the endpoint plus a SWAR digit length: 144M → 129M instructions on the sample.
6. **A scalar `position()` for endpoints that end past the 64-byte window.** That is 6.6% of lines, and cachegrind showed 22 instructions per line spent in iterator code. Replaced by continuing the SIMD window: 595M → 503M parse instructions.
7. **`select_nth_unstable` on tiny groups.** The small-sort threshold sweep (0, 8, 16, 32) gave 772, 762, 759 and 757M instructions. We sort groups of ≤ 32.
8. **Freeing record pages during the scatter.** Rejected after measuring the memory timeline:
   - the peak is set at the end of the parse;
   - the scatter scratch is touched almost at once (1,800 groups spread over it), so releasing records as they are read cannot lower the peak.
9. **Per-(endpoint, latency-bucket) counting, or a histogram multi-select.** Rejected on paper:
   - the counters would be 64-128 KB per thread, so +0.8-1.5 MB at the peak;
   - the in-place variant needs three partition passes, about the cost of quickselect.
10. **The custom `GlobalAlloc` that advised `MADV_NOHUGEPAGE`.** Removed: every big buffer is now our own `mmap` with that advice, and no heap allocation reaches 2 MB. That saved 23 lines.
11. **Three `thread::scope`s (36 thread spawns).** Replaced with one pool and barriers.
12. **Populating pages ahead in batches (x13).** `MADV_POPULATE_READ` per input chunk, plus `MADV_POPULATE_WRITE` on the scatter scratch and on each hour's output range.
    - It is meant to replace fault exceptions with one syscall per range.
    - Measured on r9, 10 alternations: wall median 36.1 vs 33.8 ms, sys CPU median 90 vs 75 ms. Worse; rejected.
    - (A first A/B ran under memory pressure and was discarded: evicted page cache was re-charged to us, M up to 105 MB.)
13. **A 16 KB interner table of 32-bit slots (x9), so the table stays in L1.**
    - Simulated D1 misses fell 5.6% (8.34M → 7.87M), but instructions rose 6M from longer probes.
    - The wall A/B flipped sign when the order was swapped. Rejected.
14. **Escaped latencies in one shared side list (x11)**, so the count and scatter loops have a fixed stride.
    - The loops saved 17M instructions, but each hour scanning the list cost 10M back: −1% net, for +145 KB and +4 lines. Rejected.
15. **Without the per-hour output zap (x12).** r9 minus that one call: wall median 38.4 vs 32.3 ms. The zap stays.

## Correctness notes

These are checked on every candidate:
- the 9 public sets (`ex-tiny`, 4 tiny, 4 sample) and the full seed 1, by SHA-256 against `expected.sha256`;
- a thread sweep (1, 2, 7, 12) on the full set;
- 4 synthetic files checked against an independent reference at 1, 3 and 12 threads:
  - an empty input, which gives a 0-byte output;
  - the 177-byte longest line (OPTIONS, the 125-byte template, status 599, latency 4294967295);
  - one hour holding 60,000 lines over 1,513 endpoints of awkward lengths (1, 8, 16, 17, 63, 64, 65, 125), with every method, escapes, 0 and u32::MAX, and statuses 499/500/599;
  - all 72 hours fully shuffled, with duplicates, both span ends, and both sides of every hour boundary.

Design points that carry correctness:
- **Endpoint identity.** Exact byte compare of the whole name; the CRC only picks the slot.
- **Escapes.** Latencies of 2^20−1 or more get an escape word, so E2 and E7 are exact.
- **Percentiles.** Nearest rank as `(q·n + 99)/100 − 1`, zero-based.
- **5xx test.** The status's first digit is `5`, so 499 is not an error and 500-599 are.
- **Regions.** Sized for the worst case: every line in one hour, two words each.
- **Bad input.** An hour outside the spec's span panics instead of writing out of bounds.
