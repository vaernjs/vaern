# Run journal

`vaern run` records each run in an append-only journal whose records are
chained by SHA-256, so an edit to any record is detected. This page is the
on-disk format.

## Where it is

```
<home>/runs/<run-id>/journal.jsonl
<home>/runs/latest            the newest run id
```

`<home>` is `$VAERN_HOME` when set, else `.vaern` in the working directory.
`<run-id>` is the UTC start time and six hex digits, for example
`20261110-093012-4f1a2b`. `vaern run --no-journal` writes nothing. Runs are
never deleted by the runtime.

## Lines

One record per line:

```
{"h":"<64 lowercase hex>","r":<record JSON>}
```

- `h` = SHA-256 of: the previous line's `h` (64 zeros for the first line),
  a newline (`0x0a`), then the exact bytes of `<record JSON>`.
- The record starts at byte 76 of the line and ends one byte before the
  newline, so the bytes that were hashed can be taken from the line as
  written. Nothing needs to be re-serialised to check a hash.
- `jq .r journal.jsonl` prints the records.

## Records

Every record starts with `seq` (0, 1, 2, …), `kind` and `at_us`
(microseconds since the Unix epoch). Records contain no floating-point
numbers.

| kind | fields |
|---|---|
| `run.start` | `vaern` (version), `script`, `script_sha256`, `argv` |
| `module.load` | `path`, `sha256` of the source as read. One per module evaluated, the entry script first |
| `console` | `fd` (1 stdout, 2 stderr), `text`: exactly what was written, newline included |
| `run.end` | `exit_code`, `error` (when the run failed), `records` (count including this one), `transcript_sha256` |

The transcript is the run's `console` records in order. `transcript_sha256`
is SHA-256 over, for each of them, `<fd>:<UTF-8 byte length of text>:<text>`.

This version records the run, its modules and its output. Network, file and
model effects are not recorded yet.

## Durability

Each line is written and flushed when it is appended. `run.end` is also
synced to disk. `process.exit()` seals the journal with its exit code.

## Verifying

```
vaern journal verify <run-id|latest>
```

prints the record count and the final hash.

| Exit code | Meaning |
|---|---|
| 0 | Every hash verifies and the run ends with `run.end` |
| 2 | Every hash verifies but there is no `run.end`. The run was interrupted (Ctrl+C, a crash), or its last records were removed |
| 1 | A record does not verify. The error names the first bad record |

Edits, reordering, inserted or deleted records, and records after `run.end`
are all detected. Removing records from the end leaves a valid but unsealed
chain, which looks the same as an interrupted run. To rule that out, keep the
final hash that `vaern journal verify` prints and compare it later.
