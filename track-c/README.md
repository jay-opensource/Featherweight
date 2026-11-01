# Track C: Join

The task, the input files and the exact output format are in the
[spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md). In short:
parse `records.csv` (192.6 MB, 3.32 million records of a hostile CSV with quoted commas, escaped
quotes, embedded LFs, mixed CRLF/LF and stray byte order marks), join every record to its country
through `regions.csv`, and write one line per country: count, id sum, amount sum, min and max in
cents, distinct names, name bytes and note bytes.

Board result for the shipped code (bbee872): **30.44 ms, 12.04 MB, 378 lines, 77.67 points**
(run-be0296de8c66), against 14.444 s and 367.4 MB for the reference. [PERF.md](PERF.md) has every
step with its run ID and the experiments that lost.

## Approach

Thesis: **little memory per unit of speed.** Nothing large is copied, the shared tables are
sized to what the data needs, and the time goes into the parser and into avoiding cache and kernel
stalls.

**Regions.** All threads parse `regions.csv` at once into a byte table from region id to country
number, up to the largest region id. A missing region reads 0, the `_UNMATCHED` group. A country
code's three letters, 5 bits each, are packed by one `pext` into an index for numbering countries in
order of first sight.

**Splitting the records.** `records.csv` is cut into 48 byte ranges per thread, claimed one at a
time from a shared counter. A range starts at its first "LF followed by a digit or a byte order
mark". That guess is proved afterwards. Range 0 starts right after the header, a true record start,
so its parse is exact. If it stops exactly where range 1 started, range 1 started at a true record
start too, and so on down the file. If any range stops elsewhere, or the dictionary fills up, the
whole run is redone on one thread with a dictionary four times larger, and a warning goes to stderr.
On generated data this never happens.

**Reading.** Each thread reads its range in 64 KB pieces with `pread` into one buffer that stays in
its L2 cache, with 64 bytes of room in front and zeros behind, so short loads may run past either
end. This beat mapping the file on the scoring box: no page-table entries to fault in while parsing
(the faults of 12 threads queue on one lock), and none to tear down at exit.

**Parsing.** Each 64-byte block gives AVX2 bit masks of quotes and LFs. A carry-less multiply turns
the quote bits into an "inside quotes" mask, carried from block to block, and the LFs outside quotes
end the records. Within a record the fields are found by its fixed shape, not by scanning:
- the id from the front;
- the end of the name from one 32-byte quote mask;
- the region and the amount after the last two commas before the terminator (neither can hold a
  comma);
- the note is what is left.

The id, the region and the amount in cents are then read together in one AVX2 register. Byte
counts come from field lengths and quote counts (each `""` counts one byte), so nothing is unescaped
or copied. This relies on guarantees of spec 2.2, which the spec allows: ids and region ids of at
most 8 digits, amounts of at most 7 digits of cents, and a terminator after every record. PERF.md
lists what the parser does outside them.

**Names.** One dictionary is shared by all threads. Each bucket is one 64-byte cache line with
three slots: a 32-bit header per slot (a hash tag and the name's dense id) and the name's first 16
bytes; a longer name keeps the rest in a side store. The key is the raw, still-escaped name, which
identifies the unescaped value exactly (escaping is a bijection, and `""` and an empty field both
give the empty key). The hash is two AES rounds. A lookup reads one line and checks the three
headers with one compare. A new name claims a slot with a compare-exchange and takes an id from a
block of 64 that its thread holds.

**Batches.** Records are handled 32 at a time. While a batch is parsed, each record's dictionary
bucket and region byte are prefetched; then the batch does its lookups and adds each record to its
thread's aggregates for the country.

**Distinct names per country.** One bit per (country, name id), about 3 MB. Setting 3 million bits
that other cores just wrote with a locked OR stalled every thread. Instead each thread queues its
bits by region of the array and, when a queue of 64 is full, applies it with plain loads and stores
under that region's lock. Every bit therefore turns on exactly once, and that is when its country's
distinct count goes up.

**Memory.** All tables come from anonymous `mmap` that is never freed (the process ends first).
They are marked `MADV_NOHUGEPAGE`: the scoring box backs memory with 2 MB pages, and `memory.peak`
would charge a whole 2 MB page for its first touched byte. The exception is the first 2 MB of the
dictionary and of the bitset, which are always fully used: they go on 2 MB pages for the TLB at no
memory cost. Each thread maps its share of the shared tables with `MADV_POPULATE_WRITE` before
using it, so twelve threads do not fault on the same page tables. Resident anonymous memory is
8.95 MB (RssAnon, measured locally); the board reports 12.04 MB for the whole cgroup.

**Output.** The per-thread aggregates are merged, the countries sorted by code with `_UNMATCHED`
last, and the lines written in one call.

## Build and run

```
docker build -t featherweight/track-c track-c
docker run --rm --network=none --read-only -v "$PWD/data/c:/data:ro" --tmpfs /out featherweight/track-c
```

The binary reads `/data/records.csv` and `/data/regions.csv` and writes `/out/aggregates.txt`.
`FW_THREADS` overrides the thread count. `tools/bench.py c --gate G1|G2|G3` builds, runs with the
board's container flags and checks the published hashes (see PERF.md).

## Files

| File | What it is |
|---|---|
| `src/main.rs` | The whole program: regions, the parallel parse, the dictionary, the bitset, output |
| `Cargo.toml` | No dependencies; release profile with fat LTO, one codegen unit, `panic = "abort"` |
| `.cargo/config.toml` | Build flags: `target-cpu=x86-64-v3` plus `pclmulqdq` and `aes` (the i5-13500 has AVX2, BMI2, PCLMULQDQ and AES-NI) |
| `Dockerfile` | The board's two-stage Rust build, offline |
| `PERF.md` | Every board run, the measurements behind each choice, and what did not work |

## Dependencies

None: Rust standard library only. CPU features come from `.cargo/config.toml`, not crates.

## With another 24 hours

- One thread pool for the regions and the records phases. Thread start-up and page faults now
  contend on the address-space lock (the regions phase is 2.4-2.8 ms run alone, mostly kernel time);
  one pool saves a round of 12 spawns. Expected −0.5 to −1.5 ms.
- Profile-guided optimisation: measured −1.1% cycles at 12 threads on this code, not yet shipped.
- Count distinct names with one popcount pass at the end instead of a count in every apply
  (about −5 instructions per record).
- Keep each thread's aggregates in pre-mapped memory to avoid thread-stack faults.
