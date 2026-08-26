# One-shot mode: NDJSON on stdout, one value per command, for piping.
#
# Regenerate this README media asset with:
#   cargo build --release && vhs docs/tapes/scripting.tape
# VHS writes a frame *directory* when Output ends in .png, which would
# collide with the Screenshot below. Send the recording to a throwaway
# GIF and keep only the still.
Output "target/vhs-scratch-scripting.gif"

Set Shell bash
Set FontSize 15
Set Width 1080
Set Height 260
Set Padding 14
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms

Hide
# VHS starts a login shell, which can replace an outer PATH override. Set and
# verify the just-built binary inside that shell before anything is recorded.
Type 'clear && PS1="$ " && export PATH="$PWD/target/release:$PATH" && test "$(command -v mcp-repl)" = "$PWD/target/release/mcp-repl"'
Enter
Sleep 500ms
Ctrl+L
Show

Type "mcp-repl --demo --json -e 'convert value=100 from=celsius to=kelvin' | jq -C -r '.content[0].text'"
Sleep 500ms
Enter
Sleep 2s

Type "mcp-repl --demo --json -e tools | jq -C -c '[.[].name]'"
Sleep 500ms
Enter
Sleep 2s

Type "mcp-repl --demo --color always -e 'echo msg=hi'; echo exit=$?"
Sleep 500ms
Enter
Sleep 2s
Screenshot docs/media/scripting.png

Sleep 500ms
