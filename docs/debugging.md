# Debugging a server

[mcp-repl](../README.md) · [Connecting](connecting.md) · [Commands](commands.md) · [Scripting](scripting.md) · [Debugging](debugging.md)


When the question is "is it the client, the server, or the network?".

## Wire tracing

Half of any "is it the client, the server, or the network?" question is
answered by the raw JSON-RPC frames. `--trace` prints every frame from the
start; `wire on` / `wire off` toggles it mid-session.

```text
demo> wire on
wire tracing on (frames print to stderr)
demo> echo message=hi
[wire ->] +4.512s
{
  "id": 6,
  "jsonrpc": "2.0",
  "method": "tools/call",
  "params": { "arguments": { "message": "hi" }, "name": "echo" }
}
[wire <-] +4.524s [12ms]
{
  "id": 6,
  "jsonrpc": "2.0",
  "result": { "content": [ { "text": "hi", "type": "text" } ] }
}
```

Each frame carries its direction, a session-relative timestamp, and, on a
response, the time its request was outstanding.

`last` reprints the previous request and its response whether or not tracing
was on: frames are always recorded, so the exchange you did not think to
trace is still there. Under `--json` it prints a
`{"request": ..., "response": ...}` object instead.

Frames print to stderr, so `--json` output on stdout stays pipeable with
tracing on.

Secrets are masked before a frame is stored, so nothing unmasked reaches the
trace or `last`. Masking is by key name, normalized so `X-Api-Key`,
`x_api_key`, and `apiKey` all match: the enumerated names (authorization,
cookie, password, client secret, and the rest), plus anything ending in
`token`, `secret`, `password`, `passphrase`, or `credential`, plus `*key`
next to a qualifier like `api`, `access`, or `private`. A credential inside
a string is masked by its scheme, covering `Bearer`, `Basic`, `Digest`, and
`token`.

Names that only look like credentials stay readable, because a trace with
its correlation ids blanked is hard to follow: `taskToken`,
`progressToken`, `nextToken`, `pageToken`, `publicKey`, and ordinary data
keys like `sortKey` are left alone. The bias runs the other way for anything
unrecognized, since a trace usually ends up pasted into an issue.

## bench

The `[142ms]` annotation answers "how slow was that call". `bench` answers
"how slow is this tool", which is the question behind a server sitting on a
network, a cold cache, or a rate limiter.

```text
cratesio> bench get_downloads crate=serde --n 50
50 calls  ok=50 err=0  min=88ms p50=104ms p95=190ms max=311ms
[5.42s]
cratesio> bench get_downloads crate=serde --n 50 --concurrency 8
50 calls  ok=50 err=0 concurrency=8  min=91ms p50=127ms p95=402ms max=655ms
[892ms]
```

- `bench <tool> [k=v...] [--n N] [--concurrency C]`. Arguments are coerced
  against the tool's `inputSchema` exactly as a direct call is, so
  `bench <tool> a=1` benchmarks the request `<tool> a=1` would send. Flags may
  appear anywhere after the tool name, in either spelling (`--n 50`,
  `--n=50`).
- `--n` defaults to 20 and is capped at 100000; `--concurrency` defaults to 1
  (serial) and never exceeds `--n`. Workers pull from a shared counter, so one
  slow call does not leave a worker's remaining share queued behind it.
- Percentiles are nearest-rank over the calls that succeeded, so every number
  reported is a latency that actually happened. Failures are counted
  separately, with the first message shown, rather than folded into the
  distribution: a fast rejection is not a fast call.
- A tool result with `isError` counts as a failure, and any failure makes the
  command exit non-zero, so `mcp-repl -e "bench <tool> --n 20" <server>` works
  as a scripted health check.
- Under `--json`, an object with `calls`, `ok`, `errors`, `concurrency`,
  `firstError`, and `minMs` / `p50Ms` / `p95Ms` / `maxMs` / `totalMs`. The
  latency fields are `null` when nothing succeeded, so a failed run cannot be
  read as an instant one.
