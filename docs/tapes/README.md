# Recordings

The images in the top-level README are generated from the tapes here with
[vhs](https://github.com/charmbracelet/vhs), so they can be regenerated
rather than re-staged by hand when the output changes.

```bash
cargo build --release
vhs docs/tapes/hero.tape
```

Each tape drives the release binary against `--demo`, which needs no
external server. They pass `--no-history`: without it reedline offers
fish-style hints from whatever the author last typed, which makes the
recording depend on the machine it was made on and can pre-fill lines the
tape means to type out.

| Tape | Produces |
| --- | --- |
| `hero.tape` | `docs/media/hero.gif` |
| `describe.tape` | `docs/media/describe.png` |
| `elicitation.tape` | `docs/media/elicitation.png` |
| `scripting.tape` | `docs/media/scripting.png` |

`scripting.tape` expects `mcp-repl` and `jq` on `PATH`:

```bash
PATH="$PWD/target/release:$PATH" vhs docs/tapes/scripting.tape
```

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

A tape that shells out to `mcp-repl` by name runs whatever `PATH` resolves,
and vhs starts a login shell that re-reads your profile. An older
`cargo install`ed copy in `~/.cargo/bin` will win, and the recording will
show a version of the tool that is not the one you built. Check with
`bash -lc 'command -v mcp-repl'` if a recording shows something you do not
recognise.
