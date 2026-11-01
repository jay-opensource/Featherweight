# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

The program is built around using little memory per unit of speed. Both input files are mapped
read-only rather than read into memory, so the page cache that already holds them is never
copied and never charged to us. Every larger working array comes straight from fresh anonymous
memory, marked so that the kernel will not back it with 2 MB pages, which means we pay only for
the 4 KB pages we actually touch.

`regions.csv` is parsed by all threads at once into a byte table from region id to country
number. A missing region reads as 0, which is the `_UNMATCHED` group.

`records.csv` is split into 48 byte ranges per thread, and threads claim them one at a time. A
range starts at a guessed record boundary: a line feed followed by a digit or a byte order mark.
Inside a quoted note a line feed is always followed by a letter or a quote, so the guess is right
on generated data, but nothing relies on that. The first range begins right after the header,
which is a true record start, so its parse is exact. If it ends exactly where the next range
begins, that start is a true record start too, and so on down the file. If any range ends
anywhere else, the whole run is redone on one thread and a warning is printed.

Parsing looks at 64 bytes at a time. AVX2 compares give bit masks of quotes, commas and line
feeds, and a carry-less multiply turns the quote bits into an "inside quotes" mask, carried from
block to block. The commas and line feeds outside quotes are the field ends, and every record has
exactly five of them, so their positions go into a small ring buffer without branches and each
record takes the next five. Numbers are read from their known end with one 8-byte load and three
multiplies; for the amount, the dot is squeezed out so the digits read directly as cents. The
quotes in a record are counted once, at its line feed. The name's `""` pairs are stored with the
name, so whatever quotes remain belong to the note. Every byte count follows from field lengths
and those quote counts, with no unescaping and no copying.

Names are compared as their raw, still-escaped bytes. Escaping is a bijection, and `""` and an
empty unquoted field both give the empty key, so this counts exactly the distinct unescaped
values. One dictionary is shared by all threads. Each 24-byte slot holds the name inline, and
names over 16 bytes keep the rest in a side store. A thread fills an empty slot after claiming it
with a compare-and-swap, and takes dense ids in blocks of 64. Records are handled in batches of 32:
while a batch is parsed, its dictionary slots and region bytes are prefetched, and the batch then
does its lookups.

Distinct names per country are one bit per (country, name id), about 3 MB for 2.98 million pairs.
Most of those bits are new, and their cache lines are usually held by another core, so setting
them with a locked OR per record stalled every thread. Instead, each thread queues its bits by
region of the array and applies a full queue of 64 with plain loads and stores while holding that
region's lock. Each bit turns on exactly once, and that is when its country's distinct count goes
up. Each thread keeps its own count, id sum, amount sum, min, max and byte totals per country,
and these are merged at the end. Countries are sorted by code, and `_UNMATCHED` goes last.

Correctness is checked on every change against the published hashes: the worked example, four
tiny seeds, four sample seeds, and full seed 1 at 1, 2, 7 and 12 threads. A torture test also
compares against the reference implementation on inputs the generator never writes but spec 2.1
allows: unterminated last records, unquoted text, long and zero-padded numbers, long names full of
escapes, control bytes inside quotes, and notes that imitate records.

## Dependencies

None.

## With another 24 hours

- Profile-guided optimisation, trained on generator output at sample scale.
- A dictionary probe that tests 16 one-byte hash tags at once, so most lookups touch a single slot.
- Parsing the id, region and amount of a record in one AVX2 register instead of three scalar ones.
