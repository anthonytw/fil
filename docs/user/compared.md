# fil compared with ssh, ET and mosh

fil does not replace ssh: it is run **over** ssh. It logs in with your `ssh`, so your keys, your `~/.ssh/config` and your jump hosts apply. What it adds is what a plain ssh connection loses when the network does.

| | ssh | Eternal Terminal | mosh | tmux or screen over ssh | **fil** |
|---|---|---|---|---|---|
| Reconnects by itself | no | yes | yes, over UDP | no (reattach by hand) | yes |
| Your terminal's own scrollback | yes | yes | no: it syncs the screen | no: it redraws the screen | yes |
| Reattach after the client crashed | no | no: the session is tied to the client | no | yes | yes |
| Take a session over from another machine | no | no | no | yes | yes |
| Port forwards that survive a reconnect | no | yes | no | no | yes |
| Needs a service on the host | sshd | a system-wide `etserver` on TCP 2022 | `mosh-server`, per user | tmux | nothing: one program you run as yourself |
| Needs root to set up | no | yes | no | no | no |
| Panes, windows, tabs | no | no | no | yes | no: your terminal already does that |
| Predictive local echo | no | no | yes | no | yes, from 0.2.0 (0.1.0 has none) |

## Against ssh

fil is for the connection that drops. Where ssh would hang and then die, fil shows a status line, reconnects, and resumes the stream with nothing lost. Sessions persist, so a closed laptop or a crashed terminal is not the end of your shell.

For anything that isn't an interactive shell, use ssh. fil is not for `scp`, `rsync` or `git` transport; plain ssh does that well, and fil's [agent relay](agent.md) is there for the `git push` you type in a session.

## Against Eternal Terminal

ET also reconnects and keeps native scrollback, and has forwarding. fil adds sessions that outlive the client (reattach after a crash, take over from another machine) and needs no system-wide service, no root and no new open port beyond an optional UDP range. ET's session is tied to the client process.

## Against mosh

mosh is excellent on lossy links and does predictive echo. It keeps the screen in sync rather than the byte stream, so you can't use your terminal's own scrollback, and it has no port forwarding. fil keeps native scrollback and forwards, and has the same UDP range (60000-61000) by default. fil does not speak mosh's protocol. From 0.2.0 fil has predictive echo too, modeled on mosh's and on by default for slow links, with the same underline for guesses on a long round trip; see [predictive echo](README.md#predictive-echo). Like mosh's, it does not hide the timing of your keystrokes on the network ([security and privacy](security-privacy.md)).

## Against tmux or screen

tmux and screen are multiplexers that run on the host and survive ssh drops. fil gives you the part of that you want when you don't want panes: sessions you can reattach, plus a reconnect that doesn't need you to type anything, and native scrollback instead of a redrawn screen. You can still run tmux inside fil if you like the panes.

## Using them together

fil's pipe can be any command, so `ProxyJump`, `ControlMaster` and `tailscale ssh` all work. mosh and ET can stay installed beside fil; they don't conflict.
