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

Recognized secrets are masked before a frame is stored, so only the scrubbed
form reaches the trace or `last`. Masking is by key name, normalized so `X-Api-Key`,
`x_api_key`, and `apiKey` all match: the enumerated names (authorization,
cookie, password, client secret, and the rest), plus anything ending in
`token`, `secret`, `password`, `passphrase`, or `credential`, plus `*key`
next to a qualifier like `api`, `access`, or `private`. A credential inside
a string is masked by its scheme, covering `Bearer`, `Basic`, `Digest`, and
`token`. If a frame is malformed JSON, recognizable secret key/value pairs
and HTTP header lines are scrubbed conservatively as raw text; the remaining
malformed content stays visible for diagnosis.

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

`help bench` is the flag reference. The reporting choices are intentional:
percentiles are nearest-rank over successful calls, so every latency shown
actually happened; failures are counted separately because a fast rejection
is not a fast call. Any failure, including an `isError` tool result, sets a
non-zero status, making the same command usable as a scripted health check.
JSON carries the counts, first error, concurrency, and latency fields; latency
is `null` when nothing succeeded rather than pretending the failed run was
instant.

## Logs

`wire on` shows what was sent and received. Some of what mcp-repl does never
becomes a frame, and that is what logging is for. Both mcp-repl and the
tower-mcp client library log through [`tracing`], and `RUST_LOG` turns them
on:

```bash
# mcp-repl's own decisions
RUST_LOG=mcp_repl=debug mcp-repl .mcp.json:prod

# the client library underneath it
RUST_LOG=tower_mcp=debug mcp-repl --http https://example.invalid/mcp

# both, for one session
RUST_LOG=mcp_repl=debug,tower_mcp=debug mcp-repl --server prod
```

mcp-repl's own records cover the decisions a frame cannot explain: which
config file and entry a selector resolved to, which approval let a stdio
child be spawned, which OAuth path a session took, why a reconnect fired and
which command won the race, and what triggered a surface refresh. If the
question is "why did it connect to *that*", start here.

Records go to stderr, so `--json` stdout stays one value per command with
logging on.

The tower-mcp records are off by default, including their warnings. The client narrates failures
mcp-repl already reports in its own words, and warns about responses to
requests mcp-repl cancelled deliberately, so leaving it on meant the same
event arrived twice and framework text landed at a prompt that otherwise
controls its own rendering.

Reach for `RUST_LOG` when a frame never appears at all: a request that was
never sent, a connection that failed before the first message, a
`subscriptions/listen` stream that ended without a terminal response. Once
frames are flowing, `wire on` is the better view, since it redacts secrets,
pairs each response with its request, and reports elapsed time.

Prefer `tower_mcp=debug` over a bare `RUST_LOG=debug` or `RUST_LOG=trace`.
The latter enable hyper, reqwest, and mio as well, whose connection-pool
records outnumber the protocol ones by roughly ten to one and interleave with
the prompt. Narrow further with `RUST_LOG=tower_mcp::client=debug`.

`--color never` applies to these records too, so a redirected stderr stays
free of escape sequences.

[`tracing`]: https://docs.rs/tracing
