# Running the tool: artefacts, exit status and diagnostics

The runtime behaviour of a pseudosync invocation, as distinct from the conversion it performs. The
algorithms are in the other documents under `docs/`; this one covers what a run produces, in what
order, and what it reports. `README.md` states the same facts for a user of the command line.

Reading and writing live in `src/liberty_io.rs`, which also resolves the destinations.

## 1. Two artefacts, resolved before anything is read

A run produces up to two things: the converted library, and optionally the reconstruction report
(`--report`).

Both destinations are resolved **before the input is parsed**. A run whose two artefacts would land
in the same place is refused there, before any work is done, rather than after an expensive
conversion has already succeeded.

`-` names the standard stream belonging to the artefact it is given to, which is a different stream
for each:

| Artefact | `-` means |
|---|---|
| the input file | standard input |
| `--output` | standard output |
| `--report` | standard error |

Collision is decided by comparing resolved paths — the canonicalised parent directory plus the file
name. Paths that alias the same file by other means, such as a symbolic link or a hard link, are not
detected.

## 2. The library is written before the report

The library is the product; the report is a diagnostic about how it was built. They are written in
that order, and the order is load-bearing.

Were the report written first, a report path that could not be opened would discard the whole run
after every expensive step had already succeeded — the conversion done, the library in hand, and
nothing kept. A report that cannot be written is still an error, but it no longer takes the library
with it.

No write error is discarded anywhere. A buffered writer's own flush-on-drop throws its error away, so
a library small enough to fit the buffer could otherwise be announced as written when the disk had
refused every byte. A report that was not fully stored is never reported as written.

## 3. Exit status

**Exit 0 — the run completed.** Some cells or outputs may have been skipped; they are named on
standard error and in the report.

Skipping is not a fault the caller committed. There is no way to tell pseudosync *convert only these
cells* or *ignore those*, so converting what is convertible is what the tool does rather than a
degraded mode of it, and a non-zero status would assert an error the caller did not make. If a cell
selection facility is ever added, this is the reasoning to revisit.

Because the status stays 0, the report and the standard-error warnings are the only signal that
anything was left alone — which is why every skipped output and every left-alone candidate cell
reaches both. `limits-of-the-model.md` covers what gets skipped and why.

**Exit 1 — the run could not produce its product at all.** Four reasons:

- the input could not be read or parsed;
- an artefact could not be written;
- the command line was invalid;
- the library is broken, a candidate cell referencing an `lu_table_template` the library does not
  declare. Such a file reads and parses without complaint, so it fits none of the other three.

## 4. What reaches standard error

Progress is reported as each library is processed. Every warning takes one form: a shared prefix, and
the library it happened in named alongside whatever else identifies the arc, output or cell it
concerns.

Naming the library on every message is not redundant. A Liberty file holds one library block, but a
run's diagnostics are read alongside those of other runs — across corners, or in a build log — and a
warning naming only a cell leaves the reader to work out which library it came from.

## 5. The reconstruction report

`--report` writes, per cell and per arc: the original table, the constraint and reference arcs it was
split into, the table rebuilt from that split, and the residual between them, followed by per-arc and
per-cell error statistics.

It is how the cost of a given `--reference-mode`, `--when-merge` and `--anchor` is read off a real
library rather than assumed. `the-pseudo-synchronous-split.md` §5 covers what the residual is and why
it is inherent in the method.

`--report-summary-only` limits the report's tables to the per-arc and per-cell statistics. Refusals
are still listed: what it drops is bulk, never the record of what the conversion left alone.
