# Records docs/demo.gif. Run from the repository root:
#   cargo build --example demo -p peephole -p peephole-cli && vhs docs/demo.tape
Output docs/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1100
Set Height 760
Set Padding 16
Set TypingSpeed 45ms
Set Theme "Catppuccin Mocha"

Hide
Type "set +m; export XDG_STATE_HOME=$(mktemp -d) PATH=$PWD/target/debug:$PATH PS1='$ '" Enter
Type "PEEPHOLE_WAIT=1 target/debug/examples/demo > $XDG_STATE_HOME/out 2>&1 &" Enter
Type "clear" Enter
Show

Type "peephole" Sleep 400ms Enter
Sleep 2s
Type ".ls order" Sleep 400ms Enter
Sleep 2.5s
Type "order.items[0].qty = 10" Sleep 400ms Enter
Sleep 1s
Type "order.total()" Sleep 400ms Enter
Sleep 1.5s
Type "order.status = #{Shipped: #{tracking: " Sleep 200ms Type `"NEW1"}}` Sleep 400ms Enter
Sleep 1s
Type "history.filter(|e| e.status == " Sleep 200ms Type `"Pending")` Sleep 400ms Enter
Sleep 2s
Type ".c" Sleep 400ms Enter
Sleep 1s
Ctrl+C
Sleep 500ms
Type "cat $XDG_STATE_HOME/out" Sleep 400ms Enter
Sleep 3s
