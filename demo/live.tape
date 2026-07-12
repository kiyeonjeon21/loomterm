Require tmux

Set Shell "zsh"
Set Width 1920
Set Height 1080
Set FontSize 15
Set FontFamily "Menlo"
Set LetterSpacing 0
Set Theme { "name": "Loomterm", "black": "#111418", "red": "#ff6b6b", "green": "#74c69d", "yellow": "#ffd166", "blue": "#64b5f6", "magenta": "#c792ea", "cyan": "#5cc8be", "white": "#d8dee9", "brightBlack": "#5f6b78", "brightRed": "#ff8787", "brightGreen": "#8bd3a8", "brightYellow": "#ffe08a", "brightBlue": "#8cc8ff", "brightMagenta": "#d7aefb", "brightCyan": "#7ee0d5", "brightWhite": "#f4f7fa", "background": "#111418", "foreground": "#d8dee9", "selection": "#2b333d", "cursor": "#d8dee9" }
Set Padding 12
Set Framerate 30
Set TypingSpeed 15ms
Set WaitTimeout 180s

Type "tmux attach -t loomterm-live-demo; sleep 1; printf '\\nLOOMTERM_DEMO_%s\\n' COMPLETE; sleep 2"
Enter
Wait /LOOMTERM_DEMO_COMPLETE/
