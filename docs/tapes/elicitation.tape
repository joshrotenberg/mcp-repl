# Elicitation: the server asks the operator, and the REPL says which server
# is asking before anything is typed.
#
# Regenerate all README media with:
#   ./scripts/recordings.sh
# VHS writes a frame *directory* when Output ends in .png, which would
# collide with the Screenshot below. Send the recording to a throwaway
# GIF and keep only the still.
Output "target/vhs-scratch-elicitation.gif"

Set Shell bash
Set FontSize 16
Set Width 1000
Set Height 340
Set Padding 14
Set Theme "Catppuccin Mocha"
Set TypingSpeed 60ms

Hide
Type "clear && ./target/release/mcp-repl --demo --no-history --color always"
Enter
Sleep 2.5s
Ctrl+L
Show

Type "sign_in"
Sleep 400ms
Enter
Sleep 1.5s
# Declared order: username, environment, remember_me. The schema's order is
# protocol-significant, so these answers are positional.
Type "ada"
Sleep 500ms
Enter
Sleep 700ms
Type "staging"
Sleep 500ms
Enter
Sleep 700ms
Type "yes"
Sleep 500ms
Enter
Sleep 1.5s
Screenshot docs/media/elicitation.png

# A trailing frame: vhs writes nothing for a `Screenshot` that is the
# last line of a tape, and still exits 0.
Sleep 500ms
