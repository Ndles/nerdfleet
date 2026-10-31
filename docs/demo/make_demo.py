#!/usr/bin/env python3
"""Record the nerdfleet demo, as a GIF and an MP4, with key captions.

The hosts are made up: docs/demo/ssh stands in for ssh and examples/demo_agent.rs
for the agent, so nothing about the machine recording it shows.

Run from the repository root:   python3 docs/demo/make_demo.py
See docs/demo/README.md for what it needs.
"""

import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1].parent
GIF, MP4 = REPO / "docs/demo/nerdfleet.gif", REPO / "docs/demo/nerdfleet.mp4"
WORK = Path("/tmp/nerdfleet-demo")
HOSTS = ["gpu-box", "llm-server", "workstation", "apu-mini", "data-node"]

WIDTH, HEIGHT, FONT_SIZE = 1280, 720, 15
TYPING_MS = 45
STRIP = 84
FONT_NAME = "JetBrainsMono Nerd Font Mono"
THEME = (
    '{ "background": "#15120F", "foreground": "#E6C58F", "cursor": "#E8A23A", '
    '"selection": "#3A322A", "black": "#2A241E", "red": "#D0705E", "green": "#A3B57A", '
    '"yellow": "#E3B062", "blue": "#7E9CB8", "magenta": "#B48CA8", "cyan": "#7FB4A8", '
    '"white": "#D9CCB4", "brightBlack": "#8A7D6C", "brightRed": "#E08A78", '
    '"brightGreen": "#B9C990", "brightYellow": "#EDC67F", "brightBlue": "#9AB4CC", '
    '"brightMagenta": "#C9A3BE", "brightCyan": "#9CCBBE", "brightWhite": "#F2E8D5" }'
)


def type_(text):
    return f"Type `{text}`"


# The storyboard: (keys shown as badges, what they do, vhs commands).
STEPS = [
    (["nerdfleet"], "every machine on one screen", [
        "Sleep 500ms", type_("nerdfleet"), "Sleep 300ms", "Enter", "Sleep 4.5s",
    ]),
    (["←↑↓→"], "what each one is training or serving", [
        "Right", "Sleep 1.1s", "Right", "Sleep 1.1s", "Down", "Sleep 1.1s", "Left", "Sleep 1.1s",
        "Up", "Sleep 300ms", "Left", "Sleep 900ms",
    ]),
    (["Enter"], "open a host: graphs, workloads, processes", ["Enter", "Sleep 4.5s"]),
    (["t"], "history: 15 minutes, 6 hours, 24 hours", [
        type_("t"), "Sleep 2s", type_("t"), "Sleep 2s", type_("t"), "Sleep 600ms",
    ]),
    (["s"], "a terminal on the host, beside its stats", [
        type_("s"), "Sleep 1.5s", type_("tail -n 6 train.log"), "Sleep 200ms", "Enter", "Sleep 3.5s",
    ]),
    (["Ctrl+]", "z"], "keys back to the dashboard, then zoom the terminal", [
        "Ctrl+]", "Sleep 700ms", type_("z"), "Sleep 2.5s",
    ]),
    (["Ctrl+]", "s"], "put it away; its session keeps running", [
        "Ctrl+]", "Sleep 700ms", type_("s"), "Sleep 1.5s",
    ]),
    (["Esc"], "back to the fleet: a training run has stalled", ["Escape", "Sleep 5s"]),
]

# Seconds until the workstation's run stalls: around when the last step starts.
STALL_S = 27

LOG = """\
epoch 41  step 18400  loss 0.412  lr 2.1e-4  3.9 it/s
epoch 41  step 18500  loss 0.407  lr 2.1e-4  4.0 it/s
epoch 41  step 18600  loss 0.409  lr 2.0e-4  3.9 it/s
epoch 42  step 18700  loss 0.398  lr 2.0e-4  3.8 it/s
epoch 42  step 18800  loss 0.401  lr 2.0e-4  4.0 it/s
val    epoch 41  top1 0.782  top5 0.941
"""


def prepare():
    shutil.rmtree(WORK, ignore_errors=True)
    bin_dir = WORK / "bin"
    bin_dir.mkdir(parents=True)
    shutil.copy(REPO / "docs/demo/ssh", bin_dir / "ssh")
    (bin_dir / "notify-send").write_text("#!/bin/sh\nexit 0\n")
    (bin_dir / "notify-send").chmod(0o755)
    config = WORK / "home/.config/nerdfleet"
    config.mkdir(parents=True)
    (config / "config.toml").write_text(
        "".join(f'[[host]]\nname = "{h}"\nssh = "{h}"\n\n' for h in HOSTS)
    )
    (WORK / "rc").write_text(
        "unalias -a\nexport PS1='\\[\\e[1;38;2;232;162;58m\\]∷\\[\\e[0m\\] '\nexport HISTFILE=/dev/null\n"
    )
    for h in HOSTS:
        home = WORK / "hosts" / h
        (home / "runs").mkdir(parents=True)
        (home / "runs/train.log").write_text(LOG)
        (home / "config.toml").write_text(
            f'shell = "/bin/bash"\nshell_args = ["--rcfile", "{WORK}/rc", "-i"]\n'
        )


def step_seconds(commands):
    total = 0.0
    for c in commands:
        if c.startswith("Sleep "):
            v = c.split()[1]
            total += float(v[:-2]) / 1000 if v.endswith("ms") else float(v[:-1])
        elif c.startswith("Type "):
            total += len(c[6:-1]) * TYPING_MS / 1000
        else:
            total += TYPING_MS / 1000
    return total


def record():
    clip = WORK / "clip.mp4"
    lines = [
        f'Output "{clip}"', 'Set Shell "bash"', f'Set FontFamily "{FONT_NAME}"',
        f"Set FontSize {FONT_SIZE}", f"Set Width {WIDTH}", f"Set Height {HEIGHT}", "Set Padding 12",
        f"Set TypingSpeed {TYPING_MS}ms", "Set Framerate 30", f"Set Theme {THEME}",
        'Env COLORTERM "truecolor"',
        f'Env PATH "{WORK}/bin:{REPO}/target/release:{Path.home()}/.cargo/bin:/usr/bin:/bin"',
        f'Env HOME "{WORK}/home"', f'Env XDG_CONFIG_HOME "{WORK}/home/.config"',
        f'Env XDG_STATE_HOME "{WORK}/home/.state"', f'Env XDG_RUNTIME_DIR "{WORK}/run"',
        f'Env DEMO_WORK "{WORK}"', f'Env DEMO_AGENT "{REPO}/target/release/examples/demo_agent"',
        f'Env DEMO_STALL_S "{STALL_S}"', "Hide",
        type_(f"unset NERDMUX NERDMUX_PANE NERDMUX_SOCKET; mkdir -m 700 -p {WORK}/run; exec bash --rcfile {WORK}/rc -i"),
        "Enter", "Sleep 400ms", type_("clear"), "Enter", "Sleep 300ms", "Show",
    ]
    for _, _, commands in STEPS:
        lines += commands
    lines += ["Hide", type_("q"), "Sleep 300ms"]
    tape = WORK / "demo.tape"
    tape.write_text("\n".join(lines) + "\n")
    subprocess.run(["vhs", "-q", str(tape)], check=True, cwd=WORK)
    return clip


def caption(clip):
    font = subprocess.run(["fc-match", "-f", "%{file}", FONT_NAME + ":bold"],
                          capture_output=True, text=True, check=True).stdout
    frame, fill, bg, text_c = "0xA3B57A", "0x1C1E15", "0x15120F", "0xE6C58F"
    size, margin = 22, 12
    advance = size * 0.6
    py, ph = HEIGHT + margin, STRIP - 2 * margin
    parts = [
        f"pad={WIDTH}:{HEIGHT + STRIP}:0:0:color={bg}",
        f"drawbox=x={margin}:y={py}:w={WIDTH - 2 * margin}:h={ph}:color={fill}:t=fill",
        f"drawbox=x={margin}:y={py}:w={WIDTH - 2 * margin}:h={ph}:color={frame}:t=2",
    ]
    pad, height = 9, size + 14
    top = py + (ph - height) // 2
    common = f"fontfile={font}:fontsize={size}:y={top + 2}:y_align=font"
    start = 0.0
    for n, (keys, what, commands) in enumerate(STEPS):
        end = start + step_seconds(commands)
        enable = f"gte(t,{start:.3f})" if n == len(STEPS) - 1 else f"between(t,{start:.3f},{end:.3f})"
        x = margin + 18.0
        for k, key in enumerate(keys):
            f = WORK / f"cap-{n}-{k}.txt"
            f.write_text(key)
            w = len(key) * advance + 2 * pad
            parts.append(f"drawbox=x={x:.0f}:y={top}:w={w:.0f}:h={height}:color={frame}:t=fill:enable='{enable}'")
            parts.append(f"drawtext={common}:textfile={f}:fontcolor={bg}:x={x + pad:.0f}:enable='{enable}'")
            x += w + 10
        f = WORK / f"cap-{n}-text.txt"
        f.write_text(what)
        parts.append(f"drawtext={common}:textfile={f}:fontcolor={text_c}:x={x + 8:.0f}:enable='{enable}'")
        start = end
    script = WORK / "caption.filter"
    script.write_text(",".join(parts))
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(clip), "-/vf", str(script), "-an",
                    "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "20", "-movflags", "+faststart",
                    str(MP4)], check=True)
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-i", str(MP4), "-vf",
                    "fps=12,split[a][b];[a]palettegen=stats_mode=diff[p];"
                    "[b][p]paletteuse=dither=none:diff_mode=rectangle",
                    "-loop", "0", str(GIF)], check=True)


def main():
    for tool in ("vhs", "ffmpeg", "nerdmux", "cargo"):
        if not shutil.which(tool):
            sys.exit(f"make_demo: {tool} is not installed")
    subprocess.run(["cargo", "build", "--release", "--example", "demo_agent", "--bin", "nerdfleet"], check=True, cwd=REPO)
    prepare()
    caption(record())
    print(f"wrote {GIF.relative_to(REPO)} and {MP4.relative_to(REPO)}")


if __name__ == "__main__":
    main()
