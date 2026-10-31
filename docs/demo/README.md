# Recording the demo

`make_demo.py` records `nerdfleet.gif` for the README, and `nerdfleet.mp4` for posting elsewhere (not committed): the
keys are typed into a headless terminal by [vhs](https://github.com/charmbracelet/vhs), and a strip below the terminal
names them. Rerun it whenever the look changes.

```sh
sudo pacman -S vhs                 # vhs pulls in ttyd and ffmpeg
cargo install --path ../nerdmux    # the terminals run the installed `nerdmux`
python3 docs/demo/make_demo.py     # from the repository root; about a minute
```

The font is `JetBrainsMono Nerd Font Mono` (change `FONT_NAME` for another).

## The hosts are simulated

The viewer is the real nerdfleet; everything it connects to is made up, so the recording shows a whole fleet and
nothing about the machine that recorded it.

- `ssh` here comes first on the viewer's PATH. It answers the viewer's probe, starts the pretend agent, and for a
  terminal starts nerdmux in a throwaway home.
- `examples/demo_agent.rs` is the pretend agent: it speaks the real protocol, with made-up hardware, workloads, 24 hours
  of history and one stalled training run.

Everything happens in `/tmp/nerdfleet-demo`, with its own `$HOME`, config and sockets. Desktop notifications go to a
stand-in `notify-send`, so the stalled run does not notify the desktop recording it.
