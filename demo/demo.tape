# The README demo. Run demo/record.sh, not this file: it needs DEMO_ENV.
Output docs/demo.gif

Set Shell "bash"
Set FontFamily "Menlo"
Set FontSize 15
Set Width 1600
Set Height 800
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms
Set WindowBar Colorful

Hide
Type `source "$DEMO_ENV"; export PS1='\[\e[2m\]~/code/weather-app\[\e[0m\] $ '; clear`
Enter
Sleep 500ms
Show

Type "agp status"
Sleep 400ms
Enter
Sleep 5s
Hide
Type "clear"
Enter
Sleep 300ms
Show

Type "agp"
Sleep 400ms
Enter
Sleep 2.5s
Type "add retries with backoff to the API client"
Sleep 500ms
Enter
Sleep 17s

Ctrl+\
Sleep 2s
Enter
Sleep 2.5s
Escape
Sleep 500ms
Escape
Sleep 1s
Type "/exit"
Enter
Sleep 1.5s
