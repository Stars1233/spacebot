# Autonomy

Spacebot has all the primitives for autonomous operation but no loop that ties them together. Tasks can be claimed. Workers can be spawned. Memories can be saved. Goals can be stored. The cortex observes everything. But none of this fires without a user message.

This doc defines the autonomy system: how Spacebot wakes up, what it sees, what it can do, and how state is tracked — without a heartbeat.json or heartbeat.md.

---

## Philosophy

Most agent harnesses implement "heartbeat" as: run a prompt on a timer, let the LLM figure out what to do, persist state to a markdown or JSON file for next time.

This has a well-known failure mode: the agent writes malformed JSON, overwrites fields it shouldn't, or drifts from the schema over time. OpenClaw's `heartbeatTaskState` embeds this problem directly — task timestamps get lost on delivery failures, corrupted on concurrent writes, and diverge from reality after crashes.

The Spacebot approach inverts this: **state lives in structured storage, not in files the LLM writes.** The database is the source of truth. State is tracked through tasks and the working memory event log. The LLM reads from structured queries and writes through typed tools. There is no heartbeat.md and no heartbeat.json.

---

## The Autonomy Channel

The autonomy channel is the agent's process for self-directed work. It is not per-task. It is one channel that wakes on a configured interval, picks up one ready task, executes it, and exits. On the next interval it wakes again.

It is structurally similar to a cron channel — periodic, no user present, full agent context. The difference is that it is persistent across runs and has awareness of its own history.

The autonomy channel is the only process that:
- Picks up ready tasks without a user present
- Creates new tasks as part of its run
- Maintains a run history via `set_outcome`

It does **not** branch. Branches exist to keep memory tool calls out of user-facing channel context. The autonomy channel has no user context to protect — it uses tools directly.

---

## Context on Wake

The cortex assembles the autonomy channel's context before each wake. It gets:

- **Identity** — SOUL.md, IDENTITY.md, ROLE.md. The agent knows who it is.
- **Memory bulletin** — the cortex's current knowledge synthesis.
- **Working memory** — recent system events. What's been happening across all channels.
- **Task state** — all active tasks: ready, in-progress, backlog, pending_approval. Full detail on each.
- **Goals** — all active goals with descriptions and notes. Background context and direction, not a work queue.
- **Active workers** — what's currently running so it doesn't duplicate work.
- **Last few run summaries** — the `set_outcome` output from its previous runs, with timestamps. This is the primary continuity mechanism.

The last run summaries are surfaced up front: "Last run (2h ago): completed task X, created tasks Y and Z for backlog." The autonomy channel wakes with spatial awareness of where things stand and what it did recently — without a separate event log.

---

## What It Does

The autonomy channel picks one ready task and executes it. It reasons about which task to pick given goal context and current system state — it is not a FIFO queue.

During a run it can:
- **Use execution tools directly** — shell, file, browser. No forced delegation. An autonomy channel that needs to run a command runs it itself.
- **Spawn workers** for genuine parallelism or long-running subtasks — when multiple things can run simultaneously, or when a subtask needs its own timeout budget.
- **Create new tasks** — as it works, it identifies follow-on work and adds it to the backlog. All autonomy-created tasks land in `pending_approval`. The agent proposes; the user decides.
- **Update task metadata** — progress notes, checkpoints, blockers. This is the state that the next run reads if the task spans multiple wakes.

What it **cannot** do:
- Reply to users (no `reply` tool)
- Create cron jobs
- Spawn other autonomy channels

---

## Continuity Between Runs

State across runs is tracked two ways:

**Tasks** — the task itself carries state. If the autonomy channel makes partial progress, it writes a `progress_note` to task metadata. The next run reads the task, sees where things left off, and continues. Nothing is lost between wakes.

**Run summaries** — on exit, the autonomy channel calls `set_outcome` with a summary of what it did: what task it worked on, what it completed, what it created, where things stand. The next wake receives the last few of these summaries as part of its context. This gives the channel a narrative thread across runs without bloating working memory.

Working memory provides broader system context — what users have been asking, what workers have been doing, what memories have been saved. The run summaries provide the autonomy-specific thread.

---

## Lifecycle

```
Cortex tick
  → elapsed since last autonomy run >= interval
  → no autonomy channel currently running
  → autonomy.enabled = true
  ↓
Cortex assembles context (identity + bulletin + working memory + tasks + goals + run summaries)
  ↓
Autonomy channel wakes with full context
  ↓
Picks one ready task, executes
  ↓
May create pending_approval tasks for backlog
  ↓
Calls set_outcome → summary recorded
  ↓
Channel exits
  ↓
Cortex records last_run_at, cleans up channel
```

If there are no ready tasks, the autonomy channel still wakes — it may create pending_approval tasks from goals and backlog items, or simply exit with "nothing to do."

If the channel times out or crashes, the task it was working on returns to `ready` state. If a task fails 3 consecutive times, it moves to `failed` and emits a working memory `Error` event.

---

## Goals

Goals are user-defined objectives. They live in the `goals` table with title, description, status, priority, optional due date, and notes.

Goals do not drive a separate review process. They are context. The autonomy channel reads them on each wake, uses them to inform which task to pick and what new tasks to create, and updates their notes when relevant progress has been made.

A user creates a goal via conversation or the API. The autonomy channel notices the goal has no tasks, creates pending_approval tasks to make progress toward it, and records this in the goal's notes. That is the full extent of "goal review" — it happens inside the autonomy channel's run, not in a separate process.

```sql
CREATE TABLE goals (
    id TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(16)))),
    title TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'active',
    priority TEXT NOT NULL DEFAULT 'medium',
    due_date TEXT,
    notes TEXT,
    metadata TEXT DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    completed_at TEXT
);
```

---

## Configuration

```toml
[autonomy]
enabled = false                    # Off by default
interval_secs = 1800               # 30 min default
active_hours = [8, 22]             # UTC hour range (optional)
max_turns = 20                     # Turn budget per run
timeout_secs = 300                 # Hard timeout per run
run_history_count = 5              # How many past run summaries to surface on wake
```

`active_hours` suppresses wakes outside the configured window. `enabled = false` (default) means the system never wakes autonomously — the user opts in explicitly.

---

## The Context Problem with Direct Worker Pickup

The previous implementation sent ready tasks straight to workers:

```
cortex → claim task → Worker::new(task_title + description) → done
```

Workers got: filesystem paths, sandbox config, tool descriptions. No soul. No identity. No memory. No sense of who the agent is or how it prefers to work.

This fails for autonomous task pickup. A task created three hours ago gets handed to a worker cold. The worker doesn't know the agent's voice, doesn't know what decisions have been made since the task was written, doesn't know what's been happening.

The forced delegation model also compounds this. A task that needs to research something, write some code, and save a summary to memory requires three process boundaries when one sufficiently capable process could handle it start to finish.

The autonomy channel solves both problems. It brings full agent context to every run. It uses tools directly. Workers are available for genuine parallelism — not as a mandatory hop.

---

## Implementation

**Phase 1 — Autonomy Channel**
- `AutonomyChannelContext` builder: identity + bulletin + working memory + tasks + goals + run summaries
- `autonomy_channel.md.j2` system prompt (direct tools, no forced delegation, `set_outcome` on exit)
- Goals table migration + `goal_create`, `goal_update`, `goal_list` tools
- Cortex: interval trigger, `last_run_at` tracking, context assembly, channel lifecycle
- `set_outcome` storage + retrieval for run history
- Task retry/failure handling (3 strikes → `failed`, working memory error event)
- Remove `pickup_one_ready_task` → direct Worker path
- `autonomy.enabled`, `interval_secs`, `active_hours`, `run_history_count` config

**Phase 2 — Polish**
- Autonomy UI surface: run history, last wake time, current task
- "Quiet while active" flag: suppress autonomy wakes when user channels have been active recently
- Autonomy outcomes surfaced to relevant user channels via working memory synthesis

---

## Non-Goals

- **Autonomy channel does not talk to users.** No `reply` tool. Output goes to task metadata, working memory, and `set_outcome`.
- **No recursive autonomy.** The autonomy channel cannot spawn other autonomy channels.
- **No identity or config modification.** Factory tools remain cortex-chat / admin only.
- **Goals are not auto-completed.** The agent proposes completion; the user confirms.
- **Per-goal active hours** are out of scope. `active_hours` applies to the whole system.
