# Contributing

Thank you for helping. Please read this page before opening an issue or a
pull request.

## Scope

This tree has one target for now: the title's **Series S mode** on **NVIDIA
cards with 8 GB of memory** and **older CPUs** (for example Ryzen 3000/5000,
Intel 9th to 12th generation). Changes for other hardware are welcome only if
they do not slow down or break this target; put them behind a switch where
they could.

## Hard rules

- **No game files of any kind** in issues, pull requests or commits: no
  executables, archives, dumps, memory captures, shader containers or shader
  caches, textures, other people's saves, keys, or links to any of these.
- **No built programs of this project** attached either.
- Do not ask for these files, and do not offer them.
- Screenshots and short clips of your own run are fine.

## Licence

This tree is GPL-3.0-or-later only. Do not copy code from GPL-2.0-only projects
(for example XCL) or from sources without a clear, compatible licence. Keep
the original authors of anything you bring in (`git cherry-pick -x`, or a
credit line in the commit message), and add yourself and them to
[CREDITS.md](CREDITS.md).

## Evidence

Say what you measured and how:

- hardware, driver version, console mode (Series S or X), output resolution;
- the scene, and the draws per frame;
- the median of the 5-second frame lines in the log, over at least a minute;
- the switches you set;
- one change at a time, against a reference run without it;
- how many runs you made.

One run proves little: the same build can differ widely between runs. Label
guesses as guesses.

## Before a pull request

```powershell
cargo xtask install-hooks     # once: the asset-policy check on every commit
cargo xtask policy            # the asset-policy check by hand (docs/asset-policy.md)
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

The tests and lints are the ones the CI runs (`.github/workflows/ci.yml`); the
policy check is described in `docs/asset-policy.md`. Say so in the description
if your change adds network code, starts processes, deletes files, adds a
build script or adds a dependency.

## AI assistance

Welcome and expected. Say which model wrote or reviewed the change. For code
that touches the GPU layer or the shader translator, use current frontier
models only (the newest top-tier models of the major labs at the time of the
change), not small, older or local models. A human reads the diff and runs the
game before the change is called tested.

## Reporting a problem

Open an issue with:

1. **Hardware**: CPU, GPU and its memory, driver version, RAM, Windows version.
2. **Switches**: every `VICTORY_*` variable you set, and the console mode and
   resolution.
3. **What you saw**: where in the game, what happened, what you expected; a
   screenshot or short clip of your own run if it helps.
4. **The 5-second lines**: the log lines around the problem, as text.
