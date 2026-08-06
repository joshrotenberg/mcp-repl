# One-shot mode: NDJSON on stdout, one value per command, for piping.
#
# Regenerate with:
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
Type "clear && PS1='$ '"
Enter
Sleep 500ms
Ctrl+L
Show

Type "mcp-repl --demo --json -e 'convert value=100 from=celsius to=kelvin' | jq -r '.content[0].text'"
Sleep 500ms
Enter
Sleep 2s

Type "mcp-repl --demo --json -e tools | jq -c '[.[].name]'"
Sleep 500ms
Enter
Sleep 2s

Type "mcp-repl --demo -e 'echo msg=hi'; echo exit=$?"
Sleep 500ms
Enter
Sleep 2s
Screenshot docs/media/scripting.png

Sleep 500ms
