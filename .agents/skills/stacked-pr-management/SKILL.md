---
name: stacked-pr-management
description: This skill should be used when the user asks to "create stacked PR", "stack PRs", "dependent PR", "rebase stack", "merge stack", or manage PR dependencies. Uses GitHub's native stacked pull request workflow for dependent changes split across multiple pull requests.
metadata:
  version: "0.1.0"
---

# Stacked PR Management

Manage dependent pull requests as a linear stack. The bottom PR targets the
trunk branch. Each PR above it targets the branch directly below it, so every
review shows only that layer's diff.

GitHub stacked pull requests and the `gh stack` extension are in public preview.
Commands and behavior may change. Verify the installed command's help before a
destructive or remote operation.

## Trigger

Use this skill for:

- Creating or extending a PR stack.
- Linking existing branches or PRs into a stack.
- Rebasing, synchronizing, restructuring, or merging a stack.
- Inspecting stack order, dependencies, checks, or merge readiness.

Use `pr-slicer` first when the work has not been divided into coherent layers.
Use `pr-gates` before pushing or updating PRs in this repository.

## Invariants

- Every stack is linear and acyclic.
- The bottom PR targets the chosen trunk, usually the default branch.
- Every higher PR targets the head branch of the PR immediately below it.
- Dependencies live in the same layer or a lower layer.
- Each PR contains one focused, independently reviewable change.
- Stack order runs bottom to top. Merge order also runs bottom to top.
- All branches belong to the same repository. Cross-fork stacks are unsupported.

```text
main
└── #42 feature/base                 bottom
    └── #43 feature/middleware
        └── #44 feature/ui           top
```

For each layer, review `base...head`. Reviewing `main...head` on a higher layer
includes every dependency below it and does not represent that PR's diff.

## Authorization

- Do not commit, install an extension, push, force-push, create or edit PRs,
  change bases, mark PRs ready, enqueue, or merge without explicit authorization
  in the user's latest instruction.
- Treat authorization for one remote action as authorization for that action
  only. Creating a stack does not authorize merging it.
- Create new PRs as drafts first. Interactive `gh stack submit` defaults new PRs
  to ready for review, so set every new PR to draft before submitting.
- Never bypass branch protection, required checks, reviews, or merge queues.
- Preserve unrelated worktree changes. Do not stage or rewrite work outside the
  requested stack.

## Prerequisites

Native stack management requires:

- GitHub CLI `2.90.0` or later.
- Git `2.20` or later.
- Authentication through `gh auth status`.
- The `github/gh-stack` extension.
- Stacked pull requests enabled for the repository.

Inspect first:

```bash
gh --version
git --version
gh auth status
gh stack --help
```

If the extension is missing, ask before installing it:

```bash
gh extension install github/gh-stack
```

Exit code `9` means stacked pull requests are not enabled for the repository.
Use the manual fallback only after confirming the user wants that workflow.
GitHub Desktop does not support stacked pull requests.

## Preflight

Before changing a stack, inspect the working tree and live GitHub state:

```bash
git status --short --branch
git diff
git diff --cached
git log --oneline --decorate --graph --all -30
git remote -v
git branch -vv
gh stack view --json
gh pr list --state open --json number,title,headRefName,baseRefName,isDraft,url
```

Also verify:

- No unrelated rebase, merge, or cherry-pick is in progress.
- The worktree is clean before `gh stack modify`, `rebase`, or `sync`.
- No stack PR is queued for merge before restructuring it.
- Commit history is linear, with no merge commits or diverged layers.
- The local and remote stack compositions agree.

If local and remote stacks diverged, stop and present the choices. Do not choose
a source of truth for the user.

## Create A Stack

Prefer the native workflow:

```bash
gh stack init feature/base
# Make and commit the first layer.

gh stack add feature/middleware
# Make and commit the second layer.

gh stack add feature/ui
# Make and commit the third layer.

gh stack view
```

`gh stack init --base <branch>` selects a non-default trunk. Existing branches
can be adopted by passing them to `gh stack init` in bottom-to-top order.
`gh stack init` enables Git's `rerere` setting so conflict resolutions can be
reused across cascading rebases. Disclose that repository configuration change
before initializing the stack.

Run targeted tests for each layer before submission. Then run this repository's
required gates in order:

```bash
just preflight
just gate-pr
```

Submit only when the user authorized pushing and PR creation:

```bash
gh stack submit
```

In the interactive editor:

1. Confirm every included branch and its order.
2. Write a focused title and description for each layer.
3. Set every new PR to draft.
4. Confirm the base branch shown for each PR.
5. Submit, then verify the remote stack with `gh stack view` and `gh pr view`.

For non-interactive creation, `gh stack submit --auto` creates new PRs as drafts
unless `--open` is passed. Do not use `--open` unless the user explicitly asks
to mark the PRs ready for review.

## Extend Or Link A Stack

Add a new layer while checked out on the current top branch:

```bash
gh stack add feature/new-layer
```

After committing and running gates, `gh stack submit` pushes the branch, creates
its PR, and updates the remote stack.

Use `gh stack link` when branches or PRs already exist but are not tracked as a
native stack. Arguments are always bottom to top:

```bash
gh stack link feature/base feature/middleware feature/ui
gh stack link 42 43 44
```

`gh stack link` pushes branches, creates missing PRs, and corrects mismatched
bases. Treat it as a remote mutation that requires explicit authorization.
Without `--open`, newly created PRs remain drafts.

## Inspect And Navigate

```bash
gh stack view
gh stack view --json
gh stack checkout <stack-number-or-pr>
gh stack switch
gh stack bottom
gh stack down
gh stack up
gh stack top
gh stack trunk
```

Use GitHub and Git as the source of truth. Do not invent helpers or maintain a
second stack-state file unless the repository already has one.

## Rebase A Stack

Use the extension's cascading rebase instead of rebasing every branch by hand:

```bash
gh stack rebase
```

It fetches the remote, updates the trunk, and rebases layers from bottom to top.
When a lower PR has merged, it uses `--onto` semantics so the next layer keeps
only its own commits.

Useful scopes:

```bash
gh stack rebase --downstack
gh stack rebase --upstack
gh stack rebase --no-trunk
```

On conflict:

1. Inspect the conflicted files and confirm the resolution belongs to the
   current layer.
2. Stage only the resolved files.
3. Continue with `gh stack rebase --continue`.
4. Abort the whole cascading rebase with `gh stack rebase --abort` if the
   intended result is unclear.
5. Reinspect every `base...head` diff after completion.

Exit code `3` indicates a rebase conflict. Exit code `7` indicates another
rebase is already in progress. Do not start a second rewrite.

## Synchronize A Stack

`gh stack sync` combines fetch, trunk fast-forward, cascading rebase, push, PR
state synchronization, and remote stack synchronization:

```bash
gh stack sync
```

This command can rewrite and push every active branch before post-rebase gates
run. Do not use it for routine updates in this repository. Keep the phases
separate so rewritten commits are verified before they reach the remote:

```bash
gh stack rebase
# Run targeted tests.
just preflight
just gate-pr
gh stack push
```

Run `gh stack push` only with explicit push authorization. It uses an explicit
force-with-lease for each active branch, but the multi-branch push is not atomic.
Some branches may update while another branch is rejected.

Use `gh stack sync` only when the user explicitly requests that command and
accepts its combined rebase-and-push behavior. Inspect the command's output and
run the gates again afterward to validate the resulting local stack.

Use `--prune` only with explicit permission to delete merged local branches.
When sync reports a divergence, do not automatically replace local state or
delete the remote stack.

## Restructure A Stack

Use `gh stack modify` to reorder, insert, rename, fold, or drop layers. It
requires a clean worktree, linear history, no active rebase, and no queued PRs.

```bash
gh stack modify
gh stack modify --continue
gh stack modify --abort
```

Preview the full proposed order before applying it. Folding or dropping a layer
changes review boundaries and removes that branch from local stack tracking,
while preserving its local branch and PR. After a successful modification, run
tests and gates, inspect every layer diff, then use `gh stack submit` only when
authorized to update the remote stack.

## Dissolve A Stack

`gh stack unstack` removes native stack tracking without closing the underlying
pull requests or deleting their branches:

```bash
gh stack unstack
gh stack unstack <stack-number>
```

This changes remote stack state and requires explicit authorization. Use
`--local` only when the user wants to remove local tracking while preserving the
remote stack. Merged, merging, and queued PRs remain attached to the remote
stack. Inspect the resulting PR bases and dependency descriptions afterward.

## Merge A Stack

GitHub can merge one layer, a bottom portion, or the full stack. Selecting a PR
merges that PR and every PR below it. Selecting the top PR merges the full
stack. The operation is all-or-nothing unless a merge queue processes the stack
in separate groups.

Before merging:

- Identify the exact PR range the user authorized.
- Confirm every included PR is open and no longer a draft.
- Inspect unresolved review threads and approvals for every included PR.
- Run `gh pr checks <number>` for every included PR.
- Confirm the requested merge method and repository policy.
- Re-read the stack immediately before executing the merge.

Merge interactively unless the user explicitly requests an unattended merge:

```bash
gh stack merge <pr-number>
```

Use `--yes` only after the exact range and merge method are confirmed:

```bash
gh stack merge <pr-number> --yes --squash
```

Repository rules, CODEOWNER approvals, checks, and merge requirements apply to
every layer. If the trunk uses a merge queue, the stack is queued and the queue
chooses the merge method.

After a partial merge, GitHub rebases the remaining branches and retargets the
new bottom PR to the trunk. Refresh the local stack, inspect each remaining
diff, and update the handoff. Do not manually repeat work GitHub completed or
run a command that pushes without fresh gate evidence.

## Manual Fallback

Use manual Git and `gh pr` commands only when native stacked pull requests are
unavailable and the user approves the fallback.

1. Create the bottom branch from trunk.
2. Create each later branch from its immediate parent.
3. Push each branch after gates pass.
4. Create each PR as a draft with its immediate parent branch as the base.
5. Add the full stack order and current-layer marker to every PR description.
6. Verify each `base...head` diff and live PR base.

For a manual cascading rebase, record every old parent commit before rewriting
the first branch. Rebase bottom to top with:

```bash
git rebase --onto <new-parent> <old-parent> <child>
```

Push rewritten branches with `--force-with-lease`, never `--force`. Manual
multi-branch updates are not atomic. Stop and reassess if any lease fails.

## Failure Rules

- Stop when observed topology differs from the expected stack.
- Stop when checks fail, mergeability changes, or review state is unresolved.
- Stop after the same gate command fails twice. Diagnose the root cause instead
  of rerunning it.
- Exit code `8` means another process holds the stack lock. Do not remove the
  lock without proving it is stale.
- Exit code `10` means an interrupted modify operation requires recovery. Use
  the command's reported recovery path rather than editing tracking state.
- Never delete or recreate remote stack state to resolve divergence without the
  user's explicit choice.

## Handoff

Report:

- The stack as a bottom-to-top ASCII tree with PR URLs.
- Every PR's head, base, draft state, and check status.
- Commands that changed local branches or remote state.
- Exact targeted tests and gate commands with outcomes.
- Any force-with-lease updates, conflicts, partial pushes, or merge-queue state.
- Remaining review dependencies and the next safe action.
