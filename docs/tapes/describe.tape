# `describe` on a tool: annotations, the schema with its field
# descriptions, and a ready-to-run example line.
#
# Regenerate with:
#   cargo build --release && vhs docs/tapes/describe.tape
# VHS writes a frame *directory* when Output ends in .png, which would
# collide with the Screenshot below. Send the recording to a throwaway
# GIF and keep only the still.
Output "target/vhs-scratch-describe.gif"

Set Shell bash
Set FontSize 16
Set Width 1000
Set Height 660
Set Padding 14
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

Hide
Type "clear && ./target/release/mcp-repl --demo --no-history"
Enter
Sleep 2.5s
Ctrl+L
Show

Type "describe echo"
Sleep 400ms
Enter
Sleep 1.5s
Screenshot docs/media/describe.png
