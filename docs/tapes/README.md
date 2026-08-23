# Recordings

The images in the top-level README are generated from the tapes here with
[vhs](https://github.com/charmbracelet/vhs), so they can be regenerated
rather than re-staged by hand when the output changes.

Build the release binary, then run the tapes you want from the repository root:

```bash
cargo build --release
vhs docs/tapes/hero.tape
vhs docs/tapes/describe.tape
vhs docs/tapes/elicitation.tape
vhs docs/tapes/scripting.tape
```

The interactive tapes call the binary as `./target/release/mcp-repl`.
`scripting.tape` keeps the shorter
commands visible in its screenshot, but prepends and verifies the same binary
inside VHS's login shell before recording anything.

Each tape uses the bundled demo, which needs no external server. Interactive
tapes pass `--no-history`: without it reedline offers fish-style hints from
whatever the author last typed, which makes the recording depend on the
machine it was made on and can pre-fill lines the tape means to type out.
Human-readable invocations also pass `--color always`, so a contributor's
`NO_COLOR` or terminal detection cannot remove semantic colors from committed
media. JSON stays uncolored until a successful parse; `jq -C` owns color in
those pipelines.

| Tape | Produces |
| --- | --- |
| `hero.tape` | `docs/media/hero.gif` |
| `describe.tape` | `docs/media/describe.png` |
| `elicitation.tape` | `docs/media/elicitation.png` |
| `scripting.tape` | `docs/media/scripting.png` |

Three details worth knowing before editing a tape, all of which fail
silently rather than loudly.

`Output` ending in `.png` makes vhs write a *directory* of frames, so the
still tapes send `Output` to a throwaway GIF and keep only the `Screenshot`.

`Screenshot` on the last line of a tape writes nothing, and vhs still exits
0. Every still tape therefore ends with a `Sleep`, so there is a frame after
the one being captured. Without it the old image stays on disk and the run
looks like it worked, which is exactly how `scripting.png` went stale.

`Type` cannot parse escaped quotes, so a line containing double quotes has to
be wrapped in single ones.

Elapsed times and generated task ids are deliberately live output. They make
the assets differ byte-for-byte across correct regenerations, so review the
visible commands and results rather than treating a binary diff as a
freshness test.
