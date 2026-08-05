# Elicitation: the server asks the operator, and the REPL says which server
# is asking before anything is typed.
#
# Regenerate with:
#   cargo build --release && vhs docs/tapes/elicitation.tape
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
Type "clear && ./target/release/mcp-repl --demo --no-history"
Enter
Sleep 2.5s
Ctrl+L
Show

Type "sign_in"
Sleep 400ms
Enter
Sleep 1.5s
Type "staging"
Sleep 500ms
Enter
Sleep 700ms
Type "yes"
Sleep 500ms
Enter
Sleep 700ms
Type "ada"
Sleep 500ms
Enter
Sleep 1.5s
Screenshot docs/media/elicitation.png
