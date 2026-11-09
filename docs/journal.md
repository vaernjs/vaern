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
| `run.start` | `vaern` (version), `script`, `script_sha256`, `argv`, `budget_caps` {`nano_usd`, `tokens`} (from `--budget-usd` and `--budget-tokens`; `null` when not given), `random_seed` (32 hex digits: the four 32-bit words, in order, that seed `Math.random`) |
| `module.load` | `path`, `sha256` of the source as read. One per module evaluated, the entry script first |
| `console` | `fd` (1 stdout, 2 stderr), `text`: exactly what was written, newline included |
| `fetch` | `eid`, `dur_us`, `turn`, `req` {`method`, `url`, `headers`, `body`}, then `res` {`status`, `status_text`, `url` (after redirects), `headers`, `body`} or `error` {`stage`, `url`, `message`, `action`} |
| `fs` | `eid`, `dur_us`, `turn`, `req` {`op`, `path` as passed; writes add `len` and `sha256` of the data}, then `res` (`text` or `bytes` for reads, `{}` for writes) or `error` {`stage`, `code`, `path`, `message`, `action`} |
| `model.call` | `eid`, `dur_us`, `turn`, `req` {`base_url`, `model`, `body`: the request body as sent}, then `res` {`status`, `body`: the response text as received (for a streamed call, the event stream as received), `usage` {`input_tokens`, `output_tokens`}, `charged_nano_usd`} or `error` {`stage`, `url`, `status`, `message`, `action`}. A call refused before dispatch (`stage` `config`) is not recorded |
| `tool.call` | `eid`, `req` {`name`, `args`}. Written when the tool starts, so the effects it makes follow it |
| `tool.result` | `eid` (the same as its `tool.call`), `dur_us`, `req` {`name`}, then `res` {`value`} or `error` {`stage`, `message`} |
| `budget.reserve` | `rid`, `run`, `for` (`model.call` or `tool.call`), `nano_usd`, `tokens`, `remaining_nano_usd`, `remaining_tokens` (before this reserve; `null` without a limit). Synced to disk before the call is sent |
| `ledger.open` | `name`, `spent_nano_usd`, `spent_tokens`: a named ledger's balance when this run opened it, unsettled reserves included |
| `budget.settle` | `rid`, `run`, `nano_usd`, `tokens`: the amount charged for that reserve |
| `run.end` | `exit_code`, `error` (when the run failed), `records` (count including this one), `transcript_sha256` |

The transcript is the run's `console` records in order. `transcript_sha256`
is SHA-256 over, for each of them, `<fd>:<UTF-8 byte length of text>:<text>`.

Effects (`fetch`, `fs`, `model.call`, and a tool call's two records):
- `eid` numbers effects in the order the script started them. The record
  is written when the effect completes, so records appear in completion
  order.
- A body is stored as `{"utf8": text}` when it is valid UTF-8, else as
  `{"b64": base64}`. Response bodies are stored whole, up to the 256 MiB
  fetch limit, so a journal can be large.
- The values of the `Authorization`, `Proxy-Authorization`, `Cookie`,
  `Set-Cookie`, `X-Api-Key` and `Api-Key` headers are stored as
  `"[redacted]"`. Secrets carried elsewhere, such as in a URL or a body, are
  recorded as sent.
- Money is integer nano-USD (10^-9 USD). `charged_nano_usd` is computed
  from the reported usage, or is the call's estimate when the server
  reported none (see `vaern:ai` in the runtime subset).
- The API key of a model call is not recorded.
- If an effect completes but its record cannot be written, the script gets
  the journal error in place of the result.

Tool arguments that do not match the tool's `parameters` are refused before the tool starts and are not recorded. Effects made inside a tool's `run` are recorded as usual; nothing in their records links them to the tool call.

Incoming `serve()` requests are not recorded.

A named budget's ledger, `<home>/budgets/<name>.jsonl`, uses the same line format and the same two record kinds, without `remaining_*`. `vaern journal verify` takes run ids only; the ledger is verified each time a run loads it.

## Durability

Each line is written and flushed when it is appended. `budget.reserve` and
`run.end` are also synced to disk. `process.exit()` seals the journal with its exit code.

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

## Replaying

Runs are replayable: every effect is recorded, and `vaern replay`
reproduces the transcript.

```
vaern replay <run-id|latest>
```

Run it from the directory the run was recorded in, since the journal keeps
the script and module paths as they were given. The replay:

1. Verifies the chain and requires `run.end`.
2. Hashes every module in `module.load` and stops with "has changed since
   the run was recorded" if one differs, before any code runs.
3. Runs the script with the recorded `argv`, the recorded `random_seed`, and
   the recorded `budget_caps` and `ledger.open` balances. No request is sent,
   no file is read or written, and no ledger is opened.
4. Matches each effect the script starts against the recorded effect with the
   same `eid`: same kind, same `req` bytes. Its recorded outcome is handed
   back in journal order, with outcomes that share a `turn` handed back
   together. A tool's `run` runs again; the effects it makes are served the
   same way.
5. Compares each console write with the recorded one, byte for byte, and
   prints only what matches. At the end it compares the exit code and error.

| Exit code | Meaning |
|---|---|
| 0 | The transcript, exit code and error match. A summary line goes to stderr |
| 1 | The first difference, with its record `seq` and both sides cut to 200 bytes, or a reason the run cannot be replayed |

Read live during a replay, so a script whose output depends on them can
diverge: `Date.now()`, `performance.now()`, `process.env`, and the order of
timers against effects. Fragments of a streamed model reply are handed back
together with its completion. Redacted header values come back as
`"[redacted]"`. A stream that failed part-way records only its error, so the
fragments it delivered first are not handed back. `serve()` stops a replay,
because incoming requests are not recorded.
