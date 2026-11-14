# Run cards

A run card is a small JSON file a training process writes to say where it is: which metrics file it appends to,
how far it will go, and which process it is. nerdfleet's agent reads the cards on its host to show each run's
step, loss, eval and ETA on the host's card, raise alerts about runs, and open
[nerdscope](https://github.com/Ndles/nerdscope) on one. A queue card does the same for a sequence of jobs, such as an
experiment's runs and their scoring. Any trainer can write them: [training.md](training.md) shows how, with a
helper to copy ([examples/runcard.py](../examples/runcard.py)). This page is the format itself.

## Where

```
~/.local/state/runcards/<key>.json        ($RUNCARDS_DIR, if set, for the writer)
```

The agent reads `~/.local/state/runcards/` of the user it runs as. Name a card after what it describes, so a
restart or a resume rewrites the same file: nerdtrain uses the first 16 hex digits of
`sha256("run:" + absolute run directory)`, and `"queue:"` for a queue. Write it atomically (a temporary file in
the same directory, then a rename), and never let a failure to write it stop or slow the run.

## A run

```json
{
  "version": 1,
  "kind": "run",
  "name": "decay_tools_v2/b_v2_s1",
  "experiment": null,
  "run_dir": "/data/experiments/decay_tools_v2/runs/b_v2_s1",
  "metrics": "/data/experiments/decay_tools_v2/runs/b_v2_s1/train.jsonl",
  "format": "jsonl",
  "step_key": "step",
  "loss_key": "loss",
  "eval_key": "val_loss",
  "nonfinite_key": "nonfinite",
  "start_step": 2000,
  "max_steps": 2400,
  "pid": 81234,
  "pid_start": 37418822,
  "started_ms": 1793822312000,
  "status": "running",
  "ended_ms": null
}
```

| Field | Meaning |
|---|---|
| `version` | 1. A card of another version is ignored. |
| `name` | What to call the run. |
| `run_dir` | Its directory; nerdfleet opens nerdscope there. |
| `metrics` | An absolute path to a file of JSON lines the run appends to. |
| `step_key`, `loss_key`, `eval_key`, `nonfinite_key` | Which keys of a line hold the step, the training loss, the eval loss and a non-finite marker (1 or true). A line without a numeric `step_key` is not a step (nerdtrain's `{"event": ...}` lines), but an eval line has both. Optional, with these defaults. |
| `start_step` | The step it starts from: 0, or where a resume or a branch begins. The ETA counts from here. |
| `max_steps` | The step it ends at. |
| `pid`, `pid_start` | The process writing the metrics, and its start time in clock ticks after boot (field 22 of `/proc/<pid>/stat`), so a reused pid does not pass for the run. |
| `started_ms`, `ended_ms` | Milliseconds since the Unix epoch. |
| `status` | `running`, `finished` or `failed`. A run that crashes cannot say so: a `running` card whose process is gone has **died**. |

Bare `NaN`, `Infinity` and `-Infinity` in a metrics line (what Python's `json` writes for them) are read as
non-finite values, not as a broken line. The last line is skipped until it ends in a newline.

## A queue

```json
{
  "version": 1,
  "kind": "queue",
  "name": "decay_tools_v2",
  "pid": 81200,
  "pid_start": 37418100,
  "started_ms": 1793822300000,
  "updated_ms": 1793829000000,
  "status": "running",
  "current": "train b_v2_s2",
  "items": [{"name": "b_v2_s1", "state": "done"}, {"name": "b_v2_s2", "state": "running"}],
  "ended_ms": null
}
```

`state` is one of `pending`, `running`, `done`, `failed`, `skipped`; `current` says what it is doing now, or is
null. `status` and liveness read as for a run.

## What the agent does with them

It reads only cards that are regular files owned by its user, and metrics files that are too; it reads a
metrics file from where it stopped, a bounded amount each time, and starts over when the file gets shorter. It
forgets cards of runs that ended more than a day ago. Alerts: a non-finite loss, no new step for ten minutes
and ten times the usual step time while the process lives, a run that finished or failed, and one that died
before its last step. Only runs the agent saw running raise alerts, so a new agent does not report old news.
