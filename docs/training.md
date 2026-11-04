# Watching training runs

You start a training run on a GPU box and walk away. This is how to see how it is doing from your own machine,
and be told when something goes wrong, with three tools that know about each other:

| Tool | Runs on | What it does for training |
|---|---|---|
| **nerdfleet** | your machine, with a small agent on each host | the glance: every host's card shows its runs (step, loss, eval loss, time left) and queues, and alerts come as desktop notifications |
| **[nerdtui](https://github.com/Ndles/nerdtui)** | the host the run is on | the close look: full loss curves, every metric the run logs, a health check, two runs side by side |
| **[nerdmux](https://github.com/Ndles/nerdmux)** | the host | keeps nerdtui (and your shells) running on the host when you disconnect |

They meet in two files the trainer writes: its **metrics**, as JSON lines, and a **run card** saying where the
metrics are ([runcards.md](runcards.md)). Nothing else is shared: no server, no port, no account.

```
  GPU box                                                     your machine
  ─────────────────────────────────────────────               ─────────────────────────
  your trainer ──writes──> runs/exp1/train.jsonl
       └───────writes──> ~/.local/state/runcards/<id>.json
                                   │
  nerdfleet agent ── reads both ───┘ ── over SSH ──────────>  nerdfleet: RUN line, alerts
                                                                   │ n
  nerdmux window "exp1": nerdtui runs/exp1  <── over SSH ──────────┘
```

## Set up a host

1. **nerdfleet** on your machine, the host in its config (`nerdfleet add` picks it from `~/.ssh/config`). For
   alerts while nerdfleet is closed, make the host's agent resident: `nerdfleet resident enable <host>`.
2. **nerdtui** on the host: one file, no Python needed. Copy the release binary there and run
   `./nerdtui --install`, which puts it in `~/.local/bin`.
3. **nerdmux** on the host (optional but better): `n` then opens nerdtui in a window of the host's nerdmux that
   outlives the connection, and opening it again goes back to the same window. Without nerdmux, nerdtui runs
   in nerdfleet's terminal and ends when you close it.

## Make your trainer write the two files

Copy [`examples/runcard.py`](../examples/runcard.py) next to your training script. It is the standard library
only and writes both files:

```python
from runcard import Run

with Run("runs/exp1", name="exp1", max_steps=10_000) as run:
    for step in range(1, 10_001):
        loss = train_step()
        run.log(step, loss=loss, lr=scheduler.get_last_lr()[0])
        if step % 500 == 0:
            run.log(step, val_loss=evaluate())
```

The names matter in three places:

- **`loss`** is the training loss and **`val_loss`** the eval loss: those are the two numbers on the card, and
  nerdtui draws them in one pane. Anything else you log (`lr`, `grad_norm`, `tokens_per_s`) shows up in nerdtui
  on its own; `tokens_per_s` also gives the card a pace before the agent has measured one.
- **`step`** is what progress and the time left are counted in, against `max_steps`.
- The metrics file is called **`train.jsonl`** and sits in the run's directory: that is how nerdtui knows a
  directory is a run.

In a multi-process job, log from one process (rank 0). Resuming? Pass `start_step` so the time left counts from
where it resumed, and keep the same run directory: the card is named after it, so the resumed run takes the old
card's place.

**From a framework:** call the same two methods from its hooks.

```python
# PyTorch Lightning
class RunCard(pl.Callback):
    def on_fit_start(self, trainer, module):
        if trainer.is_global_zero:
            self.run = Run(trainer.default_root_dir, max_steps=trainer.max_steps)
    def on_train_batch_end(self, trainer, module, outputs, batch, i):
        if trainer.is_global_zero:
            self.run.log(trainer.global_step, loss=float(outputs["loss"]))
    def on_validation_end(self, trainer, module):
        if trainer.is_global_zero and "val_loss" in trainer.callback_metrics:
            self.run.log(trainer.global_step, val_loss=float(trainer.callback_metrics["val_loss"]))
    def on_fit_end(self, trainer, module):
        if trainer.is_global_zero:
            self.run.end()

# Hugging Face Trainer
class RunCard(transformers.TrainerCallback):
    def on_train_begin(self, args, state, control, **kw):
        if state.is_world_process_zero:
            self.run = Run(args.output_dir, max_steps=state.max_steps)
    def on_log(self, args, state, control, logs=None, **kw):
        if state.is_world_process_zero and logs:
            values = {"loss": logs["loss"]} if "loss" in logs else {}
            if "eval_loss" in logs:
                values["val_loss"] = logs["eval_loss"]
            if values:
                self.run.log(state.global_step, **values)
    def on_train_end(self, args, state, control, **kw):
        if state.is_world_process_zero:
            self.run.end()
```

**Not Python, or your own logger?** Write the card yourself: it is a dozen fields
([runcards.md](runcards.md)), written once at the start (atomically: a temporary file, then a rename) and once
at the end. Point its `metrics` at whatever JSON-lines file you already write; `step_key`, `loss_key` and
`eval_key` name your keys if they are not `step`, `loss` and `val_loss`. nerdtui then needs the file to be
called `train.jsonl`, or your own adapter for it.

**A queue of runs** (a sweep, an experiment with several seeds) can write a queue card too: its jobs and their
states, and what it is doing now. The card then has a `QUEUE` line with how many are done. See
[runcards.md](runcards.md#a-queue).

## What you see

- **The host's card:** a `RUN` line per run that is training or ended in the last six hours, a `QUEUE` line,
  then the usual workloads. Yellow: no step for five minutes, or a non-finite loss. Red: failed, or died.
- **The host view** (`Enter`): every run with its loss curve so far, its pace, its eval step and its directory.
  `▶` marks the run `n` opens: the selected workload's, else the one training.
- **`n`:** nerdtui on that run, on the host, in the host's terminal (`Ctrl+]` back to the dashboard, `s` puts the
  terminal away, and nerdtui keeps running in its nerdmux window).
- **Alerts:** a non-finite loss; no new step for ten minutes and ten times the usual step time while the
  process lives; a run that finishes or fails; a run whose process is gone before its last step ("died",
  which is what a crash looks like, since a crashed run cannot say so). A resident agent raises them whether or
  not nerdfleet is open; nerdfleet shows them on the card and as desktop notifications.

## When something does not show

| You see | Because |
|---|---|
| no `RUN` line | no card in `~/.local/state/runcards/` of the user nerdfleet connects as, or not `"version": 1`, or the card or metrics file is not a regular file owned by that user |
| `RUN` line, no time left | the agent has not seen a minute of steps yet, and the run logs no `tokens_per_s` |
| the run shows **died** while it trains | `pid` in the card is not the process that is running (a launcher's pid, say), or `pid_start` is wrong; write the card from the training process itself |
| `n` says there is no nerdtui | install it on the host (`./nerdtui --install`); nerdfleet looks on `PATH` and in `~/.local/bin` |
| `n` does nothing | the host has no run cards, or the run's directory has characters other than letters, digits and `/._-`, which nerdfleet does not pass to a shell |
| "older resident agent: u updates it" | the host's resident agent is from an older nerdfleet and does not read cards; `u` updates it |
