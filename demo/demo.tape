# The README demo. Run demo/record.sh, not this file: it needs DEMO_ENV.
Output demo/out/demo.mp4

Set Shell "bash"
Set FontFamily "Menlo"
Set FontSize 20
Set Width 1360
Set Height 680
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms
Set Framerate 15
Set WindowBar Colorful

Hide
Type `source "$DEMO_ENV"; export PS1='\[\e[2m\]~/code/weather-app\[\e[0m\] $ '; clear`
Enter
Sleep 500ms
Type `python3 -c 'import time; print(f"start {time.time():.3f}")' >> "$DEMO_EVENTS"; clear`
Enter
Sleep 300ms
Show

Sleep 1s
Type "agp"
Sleep 500ms
Enter
Sleep 3.5s
Type "add retries with backoff to the API client"
Sleep 700ms
Enter
Sleep 17s
