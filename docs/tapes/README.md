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

Two details worth knowing before editing a tape. `Output` ending in `.png`
makes vhs write a directory of frames, so the still tapes send `Output` to a
throwaway GIF and keep only the `Screenshot`. And `Type` cannot parse escaped
quotes, so a line containing double quotes has to be wrapped in single ones.
