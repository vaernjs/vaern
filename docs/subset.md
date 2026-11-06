# Vaern runtime subset

This is the exact surface `vaern run` provides in v0.0.6. Anything not listed
here is absent. A section's **Limits** list says where an API differs from the
web or Node.js behaviour of the same name.

## Running files

- `vaern run <file>` runs `.js`, `.mjs` and `.ts` files as ES modules. Top-level
  `await` works.
- `.ts` files have their types removed before they run. No type-checking is
  done. Removed types are overwritten with spaces, so line and column numbers
  in stack traces and errors match the `.ts` source.
- Columns count UTF-8 bytes, as the engine reports them, in `.js` and `.ts`
  files alike. On a line with non-ASCII text before the reported position,
  the column is higher than the one an editor shows: one more for each extra
  byte, so 1 for `é`, 2 for `文` and 3 for an emoji.
- TypeScript constructs that emit runtime code are rejected with a
  `file:line:col` error: `enum`, `const enum`, `namespace`/`module` blocks and
  decorators. Their `declare` forms (`declare enum`, `declare namespace`,
  `declare module`, `declare global`) hold types only and are removed.
- Auto-accessor fields (`accessor x = 1`) are rejected the same way. The
  engine cannot parse them, and removing the keyword would turn a getter and
  setter on the prototype into a plain field.
- A syntax error the engine finds in a `.js` file is reported as
  `SyntaxError: <message>` with its `file:line:col`.
- Imports used only as types are removed, as TypeScript removes them, so a
  module that exports nothing at runtime can still be imported for its types.
- A module where types cannot be overwritten in place is re-printed from the
  syntax tree instead. It runs the same code, but reported line numbers can
  differ from the source. This applies to constructor parameter properties
  (`constructor(private x: number)`), `import x = require(...)`, `export =`,
  `export as namespace`, a `<T>value` type assertion that spans lines, and
  any removal that would leave a line ending before `(`, `[`, a template,
  `+`, `-` or `/`.

## Modules

- Relative imports (`./x.ts`, `../lib/y.js`) with the exact file name,
  extension included. There is no extension or `index` lookup.
- `vaern:http`, which exports `serve`.
- `vaern:fs`, which exports `readFile`, `writeFile` and `appendFile`.
- Packages already installed in `node_modules`, imported by name (`pkg`,
  `@scope/pkg`, `pkg/sub`). Static and dynamic `import()` both work.

### Package resolution

- `node_modules/<name>` is looked for in the importer's directory, then each
  parent up to the filesystem root, as Node.js does. The first package
  directory found is the only one used.
- With `exports` in `package.json`: a string, a conditions object or a
  subpath map, including patterns with one `*` (the longest matching prefix
  wins). Conditions are read in the order the package declares them, and
  the first of `vaern`, `import` or `default` is used. `node` and `require`
  never match. A nested conditions object where nothing matches is skipped
  and the next sibling condition is tried, as in Node.js. A `null` target or
  an unlisted subpath is not exported.
- Without `exports`: `pkg/sub` names an exact file in the package, and `pkg`
  uses the `module` field, then `main` only when `"type": "module"` is set.
  Otherwise the package has no ES module entry point and the import fails.
- Every target must stay inside the package directory and outside any
  nested `node_modules`.
- The resolved file is reached through its real path, so a linked package
  (as a pnpm store creates) is one module however it is imported. `.ts`
  files inside packages have their types removed as usual.
- A failed import reports the specifier, the importer, the reason, every
  path or `package.json` field tried, and what to do next.

Limits: QuickJS shortens a thrown message to 256 bytes, so a script that
catches a failed import sees only the reason; the full list of paths
appears when the failure ends the run.

Absent: `node:*` modules (importing one fails with its own message), the
`imports` field (`#name` specifiers), a package importing itself by name,
array targets in `exports`, `require` and CommonJS, JSON imports, absolute
paths and URL imports, and downloading packages from a registry.

## Globals

### console

`console.log`, `console.info`, `console.warn` and `console.error`. `log` and
`info` write to stdout; `warn` and `error` write to stderr. Arguments are
joined with a space.

- Strings print as they are. `undefined`, `null`, symbols and bigints (`10n`)
  print as their names or values.
- An `Error` prints as `Name: message`, then its stack frames, then
  `Caused by: ...` for each `cause`, up to eight levels deep.
- Other objects and arrays print as `JSON.stringify` output. Inside them,
  errors print as `"Name: message"` and bigints as `"10n"`. Functions and
  `undefined` values are left out, and a cyclic object prints as
  `[object Object]`.

Absent: `console.debug`, `trace`, `table`, `dir`, `time`, `count`, `assert`,
`group` and format specifiers such as `%s`.

### Timers and scheduling

`setTimeout`, `setInterval`, `clearTimeout`, `clearInterval` and
`queueMicrotask`. Extra `setTimeout` arguments are passed to the callback. An
exception thrown in a timer callback ends the run with exit code 1.

Absent: `setImmediate`.

### process

- `process.argv`: `[executable, script, ...args]`.
- `process.env`: the host environment, read at startup.
- `process.exit(code)` and `process.exitCode`.

Nothing else on `process` exists.

### TextEncoder and TextDecoder

- `TextEncoder.encode(string)` returns UTF-8 bytes. Lone surrogates encode as
  U+FFFD.
- `new TextDecoder(label, { fatal, ignoreBOM })` accepts only the labels
  `utf-8`, `utf8` and `unicode-1-1-utf-8`. Any other label throws
  `RangeError`.
- `decode(bytes, { stream })` takes an `ArrayBuffer` or any view of one.
  Invalid bytes decode to U+FFFD, following the WHATWG Encoding Standard; with
  `fatal: true` they throw `TypeError`. A leading byte order mark is removed
  unless `ignoreBOM` is set. With `stream: true`, a code point split across
  calls is held until the next call completes it.
- An omitted input decodes to `""`; `null` or any other non-buffer throws
  `TypeError`. Options may be omitted or `null`; any other non-object throws
  `TypeError`.

Absent: `TextEncoder.encodeInto`, every encoding other than UTF-8,
`TextEncoderStream` and `TextDecoderStream`.

### URL and URLSearchParams

`URL` is parsed by the same WHATWG parser that `fetch` uses for request URLs.
It supports credentials, IPv6 hosts, relative resolution against a base,
percent-encoding and default-port normalisation.

- `new URL(input, base)` throws `TypeError` for an invalid URL. `URL.canParse`
  and `URL.parse` return `false` and `null` instead of throwing.
- `href`, `origin`, `protocol`, `username`, `password`, `host`, `hostname`,
  `port`, `pathname`, `search` and `hash`, each with the standard setter
  except `origin`.
- `searchParams` is live: changing it updates `search` and `href`, and setting
  `search` or `href` replaces its contents.
- `toString()` and `toJSON()` return `href`.

`URLSearchParams` accepts a query string (with or without a leading `?`), a
record object, an iterable of pairs or another `URLSearchParams`. It provides
`append`, `delete` (with an optional value), `get`, `getAll`, `has` (with an
optional value), `set`, `sort`, `forEach`, `keys`, `values`, `entries`,
iteration, `size` and `toString`. Encoding follows
`application/x-www-form-urlencoded`: a space is written as `+`.

Absent: `URL.createObjectURL` and `URL.revokeObjectURL`.

### Headers

`get`, `set`, `append`, `has`, `delete`, `forEach`, `entries` and iteration.
Names are case-insensitive.

### ReadableStream

`new ReadableStream({ start(controller) })`, where the controller has
`enqueue`, `close` and `error`, and `getReader()`, whose reader has `read` and
`cancel`.

Absent: `pull`, queuing strategies, `pipeTo`, `pipeThrough`, `tee`, async
iteration, `WritableStream` and `TransformStream`.

### Request and Response

- Bodies can be a string, `Uint8Array`, `ArrayBuffer` or `ReadableStream`.
- Body methods are `text()`, `json()` and `arrayBuffer()`. Each body can be
  read once; a second read throws `TypeError`. Streamed bodies are read in
  full before decoding, so a code point split across chunks decodes
  correctly, and `arrayBuffer()` returns the exact bytes.

Absent: `blob()`, `formData()`, `bytes()`, `clone()`, and the static methods
`Response.json`, `Response.redirect` and `Response.error`.

### fetch

`fetch(url, init)` takes a string, or anything whose `String()` form is a URL
(such as a `URL`). It supports HTTP/1.1 over plain TCP or TLS 1.2 and 1.3,
trusting the public Mozilla root certificates.

- `init.method`, `init.headers` and `init.signal` (an `AbortSignal`).
- `init.body` can be a string, `Uint8Array` or a `ReadableStream` of
  `Uint8Array` chunks. Any other body type throws `TypeError`.
- `init.timeout` is a Vaern extension, not part of the web standard: the
  request deadline in milliseconds. It defaults to 30000, and `0` disables
  it.
- Redirects are always followed, up to 20 hops. A 303 response changes any
  method other than GET or HEAD to GET; 301 and 302 change POST to GET; 307
  and 308 keep the method and body. `Authorization` and `Cookie` are dropped
  when a redirect changes origin, and a redirect from `https` to `http` is
  refused.
- `init.redirect` accepts only `"follow"`. `"manual"` and `"error"` throw
  `TypeError`.
- `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Upgrade`, `TE`
  and `Trailer` are set by the runtime from the URL and body. Supplying one
  in `init.headers` throws `TypeError`.
- An abort stops the request whether it is sent before the request starts,
  while it is in flight, or between redirect hops.
- The whole response body is read before `fetch` resolves, and bodies over
  256 MiB are refused with a `read` error. A `Content-Length` over the limit
  is refused before any of the body is read.
- Idle connections are kept for reuse for 90 seconds, at most 8 per host.
- A failed request rejects with an error whose `stage` names the step that
  failed: `dns`, `connect`, `tls`, `send` or `read`. Timeouts and aborts
  report the step they interrupted.

Absent: `Request` objects as input, `mode`, `credentials`, `cache`,
`referrer`, `integrity`, `keepalive`, a cookie store, HTTP/2, proxies and
client certificates. `ArrayBuffer`, `Blob`, `FormData` and `URLSearchParams`
are not accepted as request bodies.

### AbortController and AbortSignal

`new AbortController()` with `signal` and `abort(reason)`. A signal has
`aborted`, `reason`, `addEventListener("abort", ...)` and
`removeEventListener`. `AbortSignal.abort(reason)` and
`AbortSignal.timeout(ms)` are also provided.

Absent: `AbortSignal.any`, `throwIfAborted` and the `onabort` property.

### From the engine

QuickJS-ng provides the standard ECMAScript built-ins, plus `performance.now`,
`atob`, `btoa`, `WeakRef` and `FinalizationRegistry`.

Absent: `crypto` (including `getRandomValues` and `randomUUID`),
`structuredClone`, `Blob`, `File`, `FormData`, `EventTarget`, `Event`,
`WebSocket`, `navigator`, `Buffer` and `global`.

## vaern:http

`serve(handler, { port, hostname })` starts an HTTP/1.1 server and returns
`{ addr: { hostname, port }, close() }`. The defaults are `127.0.0.1` and port
`8000`; pass port `0` to let the system choose one. `handler(request)` returns
a `Response`, or a promise of one.

- Request bodies are capped at 1 MiB. A larger body is answered with 413
  before the handler runs.
- Headers must arrive within 10 seconds (408 otherwise), and the body within
  30 seconds.
- A server keeps at most 1024 connections open; another one is answered with
  503 and closed. At most 128 requests can wait for the handler at once;
  another one is answered with 503 before its body is read.
- A handler that throws is answered with 500.
- A `ReadableStream` response body is sent as the client reads it. While a
  slow client has the buffer full, only that response waits; timers, other
  requests and fetches keep running. A client that reads nothing for 30
  seconds, or disconnects, ends the response with an error logged to stderr,
  and the body is aborted rather than ended short.
- A response carrying `Host`, `Content-Length`, `Transfer-Encoding`,
  `Connection`, `Upgrade`, `TE` or `Trailer` throws `TypeError` when the
  handler returns it, and the request is answered with 500. The runtime sets
  the framing headers from the body.
- `close()` waits up to 5 seconds for requests in flight. Ctrl+C (SIGINT)
  closes servers the same way; SIGTERM is not handled.

## vaern:fs

Whole-file reads and writes, as named and default exports. Every function
returns a promise; the work runs off the JavaScript thread.

- `readFile(path, options)` resolves to a string when `options` is `"utf8"`,
  `"utf-8"` or `{ encoding }` with one of those, and to a `Uint8Array`
  otherwise. Invalid UTF-8 reads as U+FFFD; a byte order mark is kept.
- `writeFile(path, data, options)` creates or replaces the file. `data` is a
  string (written as UTF-8), an `ArrayBuffer` or any view of one. With
  `{ flag: "wx" }` it fails if the file already exists; `"w"` is the default
  and the only other flag.
- `appendFile(path, data)` creates the file or adds to its end.
- A relative path resolves against the current working directory. Parent
  directories are not created.
- Files larger than 64 MiB, a quarter of the heap cap, are refused with
  `EFBIG` before any byte is read. A read under that limit whose result
  still does not fit in the heap rejects with `InternalError: out of
  memory`.
- A failure rejects with an `FsError` whose `stage` is `resolve`, `open`,
  `read` or `write`, whose `code` is `ENOENT`, `EACCES`, `EEXIST`, `EISDIR`,
  `ENOTDIR`, `EFBIG` or `EIO`, whose `path` is the absolute path, and whose
  `action` says what to do next. A bad argument (a non-string path, an
  unsupported encoding or flag, data of another type) rejects with
  `TypeError`.
- No policy applies yet: a script can read and write anything the process
  can.

Absent: synchronous and callback variants, `stat`, `readdir`, `mkdir`, `rm`,
`rename`, file handles, streams, `file:` URLs as paths, and the `node:fs`
name.

## Limits

- The JavaScript heap is capped at 256 MiB. Exceeding it throws
  `InternalError: out of memory` at the allocation's location, and the run
  exits with code 1.
- Each module source, the entry file included, must be valid UTF-8 and at
  most 64 MiB, a quarter of the heap cap. A larger file fails to load with
  an error naming the file and the limit, before any of it is read. A
  leading UTF-8 byte order mark is ignored, so it does not shift reported
  columns.
