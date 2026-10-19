# The README images. Run demo/record.sh, not this file: it needs DEMO_ENV
# from `demo/setup.py --onboarding`.
Output demo/out/screens.mp4

Set Shell "bash"
Set FontFamily "Menlo"
Set FontSize 16
Set Width 1640
Set Height 720
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms
Set WindowBar Colorful

Hide
Type `source "$DEMO_ENV"; export PS1='\[\e[2m\]~/code/weather-app\[\e[0m\] $ '; clear`
Enter
Sleep 500ms
Show

# First run: the onboarding wizard.
Type "agp"
Enter
Sleep 4s
Screenshot docs/images/onboarding-welcome.png
Sleep 700ms
Enter
Sleep 1.5s

# Use the two main installs and give every account a short name.
Space
Sleep 300ms
Enter
Sleep 500ms
Ctrl+U
Type "Personal"
Enter
Sleep 300ms
Down
Enter
Sleep 500ms
Ctrl+U
Type "Side project"
Enter
Sleep 300ms
Down
Enter
Sleep 500ms
Ctrl+U
Type "Work"
Enter
Sleep 300ms
Down
Space
Sleep 300ms
Enter
Sleep 500ms
Ctrl+U
Type "ChatGPT"
Enter
Sleep 1s
Screenshot docs/images/onboarding-accounts.png
Sleep 700ms

Right
Sleep 1.5s
Screenshot docs/images/onboarding-sharing.png
Sleep 700ms

Right
Sleep 1.5s
Screenshot docs/images/onboarding-balancing.png
Sleep 700ms

Right
Sleep 1s
Down
Enter
Sleep 1.5s
Screenshot docs/images/onboarding-dock.png
Sleep 700ms

Right
Sleep 1.5s
Screenshot docs/images/onboarding-done.png
Sleep 700ms

# The session starts. The menu key opens the menu.
Enter
Sleep 3s
Ctrl+\
Sleep 1.5s
Enter
Sleep 1.5s
Screenshot docs/images/switch.png
Sleep 700ms
Escape
Sleep 400ms
Escape
Sleep 500ms
Type "/exit"
Enter
Sleep 1s

# The usage of all accounts.
Type "clear"
Enter
Sleep 300ms
Type "agp status"
Enter
Sleep 2s
Screenshot docs/images/status.png
Sleep 700ms
Sleep 1s
