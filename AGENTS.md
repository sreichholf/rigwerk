# Agent guidelines

Defaults for coding agents (and humans) working on RigWerk.

## Commits

- One commit per logical change. Review fixes are not separate commits:
  fold them into the commit they fix (`git commit --fixup <sha>`, then
  `git rebase -i --autosquash`) before the PR is opened or merged.
- Rewriting history is fine on your own unmerged branch (use
  `git push --force-with-lease`). Never rewrite `master` or someone
  else's branch.
- Subject: imperative, sentence case, no trailing period, under ~70 chars.
- Body: a few terse bullets describing the basic change. No essays, no
  test logs, no review history.

```
Flush subnormal floats in the audio callbacks

- Add DenormalGuard (FTZ/DAZ on x86, FZ on aarch64), restored on drop
- Hold it in OutputMixer::render and the input callbacks
```

## Pull requests

- Title: same style as a commit subject.
- Body: short bullets only:
  - **What**: the basic change, a few bullets.
  - **Testing**: what was run (and what was not, e.g. no hardware).
  - **Open**: known limits, only if any.
- No restating the diff, no review transcript.

## Checks before pushing

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
```

Windows-only code: also `cargo check --target x86_64-pc-windows-gnu`.
