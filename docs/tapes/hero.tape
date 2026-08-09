# The front-page recording: the server's surface is the command set.
#
# Regenerate all README media with:
#   ./scripts/recordings.sh
#
# --no-history keeps the recording deterministic: without it reedline offers
# fish-style hints from whatever the author happened to type before, which
# both changes the output and pre-fills lines the recording means to type.
Output docs/media/hero.gif

Set Shell bash
Set FontSize 16
Set Width 1180
Set Height 720
Set Padding 14
Set Theme "Catppuccin Mocha"
Set TypingSpeed 55ms

Hide
Type "clear && ./target/release/mcp-repl --no-history --color always"
Enter
Sleep 1.5s
Show

# The first-run path begins at a useful disconnected prompt. `connect` can
# select or switch a server without throwing away the line-editor session.
Type "connect demo"
Sleep 500ms
Enter
Sleep 2.5s

# The command word completes from the server's tools, with descriptions and
# the safety annotations it declared.
Type "ec"
Sleep 700ms
Tab
Sleep 2s
Enter
Sleep 500ms

# Argument names come from the tool's inputSchema.
Tab
Sleep 2s
Enter
Sleep 400ms
Type '"hello from the demo"'
Sleep 500ms
Enter
Sleep 1.4s

# Enum values complete as well, resolved through the schema's $defs.
Type "convert value=100 from=celsius to="
Sleep 600ms
Tab
Sleep 1.6s
# Tab again cycles the menu, so the conversion actually converts something.
Tab
Sleep 1.2s
Enter
Sleep 400ms
Enter
Sleep 1.4s

# A slow tool reports progress as it goes.
Type "scan steps=4"
Sleep 500ms
Enter
Sleep 2.6s

# A trailing & runs a task-capable tool in the background; the completion
# notification arrives on its own.
Type "slow_add a=2 b=3 &"
Sleep 500ms
Enter
Sleep 5s

Type "quit"
Enter
Sleep 900ms
