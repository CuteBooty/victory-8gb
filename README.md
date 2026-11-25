# VIctory fork (work in progress)

**This tree has one focus for now: the title's Series S mode on NVIDIA cards
with 8 GB of memory and older CPUs.** Other hardware may work, but it is not
tested or tuned here.

A source snapshot of a fork of [VIctory](https://github.com/Marvisak/VIctory)
by Marvisak and its contributors, a compatibility layer that runs an Xbox
Series title natively on Windows from files the user supplies. It is based on
[fxceless/victory-fork](https://github.com/fxceless/victory-fork) at its commit
`c96b4f2b` and adds changes from that fork's pull requests, from other forks,
and of its own. This snapshot has no commit history; CREDITS.md names who
wrote what and where it came from.

**Source code only.** No game files, dumps, shader caches or built programs are
distributed here, and none will be. You need your own legally obtained copy of
the game. This project is not affiliated with or endorsed by the title's
publisher or the console maker. Licensed GPL-3.0-or-later; built with AI
assistance. See [CREDITS.md](CREDITS.md), [CONTRIBUTING.md](CONTRIBUTING.md)
and [AGENTS.md](AGENTS.md).

## Test machine and status (measured 2026-11-25)

Windows 11, NVIDIA RTX 3070 8 GB (driver 621.24), AMD Ryzen 5 3600 (6 cores /
12 threads), 32 GB RAM; the title's Series S mode at 1920x1080 output, with the
switches in the table below.

- The first story mission was played from start to end in one 103-minute run
  without a crash; a manual save was written and loaded again (on an earlier
  build of this tree).
- Frame times, as medians of the 5-second log lines: open water about
  141-168 ms (6-7 fps); medium scenes with 4,400-7,000 draws 180-250 ms;
  heavy scenes (town, dense foliage, 7,000-10,000 draws) 240-340 ms (3-4 fps).
- The newest build in this snapshot has had two short runs only.

## New in this snapshot (compared with the fork it is based on)

- Table constants read in place (ported from upstream `683ad7a9`): descriptor
  tables holding a constant buffer no longer need fresh slots for every draw.
  Measured here: waits for slots 285-400 ms a frame (summed over threads) ->
  0; slots asked 163-193 thousand -> 62-96 thousand a frame.
- Texture copy-back for the boat (from vx-layer), with hardening.
- Read-back tracking and four correctness fixes: stale data between lists of
  one submission, the extent of a boxed read-back, table reuse across a title
  write, and the present's wait policy.
- Six shader translator fixes.
- Descriptor heap lock split (fork PR #1146).
- Series S profile (fork PR #188).

## Known problems

- Slow (see the frame times above).
- The picture's exposure flashes unless `VICTORY_LATE_WAIT=2` is set; that
  wait costs 70-90 ms a frame in heavy scenes.
- Stalls while the texture refresh waits for title memory to stop moving.
- With `VICTORY_BAKE_TEXTURES_MS=0`, characters can be baked dark at first.
- Shaded areas can be black: bounced light does not run.
- Foliage looks dotted in Series S.
- Eight compute programs are refused by the translator.
- No autosave was seen during a mission.
- The first launch after a change to the shader translator translates every
  shader again (about 20 minutes on the test machine).
- Many changes here are specific to this title, and some are marked
  `shortcut:` in the code where they approximate what the console does.
- A few style lints are switched off in the workspace `Cargo.toml` until the
  newer code is tidied.

## Switches used on an 8 GB card

Environment variables, set before starting `victory.exe`. Each is described
in a comment where the code reads it.

| Switch | What it does |
|---|---|
| `VICTORY_SKIP_BINK=1` | skips the title's movies that play once (the intro); `=all` also skips looping ones |
| `VICTORY_TEXTURE_DROP_MIPS=1` | host copies of 2D textures the title fills (at least 512 texels on the shorter side) leave out their largest mip level, about three quarters of their bytes; shaders see them at half size |
| `VICTORY_SMALL_TEXTURE_KB=0` | turns off letting small textures (up to 1024 KB by default, within a 384 MB reserve) onto a card that is out of room; with `0` they are refused while the card is full, as before |
| `VICTORY_QUEUE2=1` | the relay's fetches of card copies run on a dedicated D3D12 copy queue (same as `VICTORY_RELAY_COPY_QUEUE=1`); pieces fetched beside a later flight are fetched again by it. Ignored with `VICTORY_CARD_FETCH_INFLIGHT=1` |
| `VICTORY_INDIRECT_WINDOW=1` | indirect draws that read record after record of one argument buffer are clamped a window of records at a time (16 up to 64) instead of one compute pass per record |
| `VICTORY_ASYNC_SUBMIT=1` | the title's submissions are queued and prepared and sent by a thread of the layer's own (needs `VICTORY_LATE_WAIT`) |
| `VICTORY_ASYNC_SUBMIT_SAFE=0` | with `VICTORY_ASYNC_SUBMIT=1`, the title's thread no longer waits until its submission has finished reading title memory; those reads overlap the title |
| `VICTORY_CARD_HASH_WIDE=1` | the hash pass over copies in the card's memory gives each 64 KiB piece a group of 256 threads instead of one thread |
| `VICTORY_BAKE_TEXTURES_MS=0` | no extra texture-upload budget (1,500 ms by default, which stalled for seconds) before the title's decoration bake draws; the texture flush before each bake draw (`VICTORY_BAKE_FLUSH`, on by default) still runs |
| `VICTORY_LATE_WAIT=2` | submissions go to the card without a wait for each and are settled together where the title can observe them; at stage 2, bytes the title changed under a flight's writes are merged in instead of settling the flights |
| `VICTORY_BACK_SETTLE=0` | a present sent ahead waits only for flights that copy shader-written textures back, not for every flight that copies small results back to title memory; it applies only when the present is sent ahead (`VICTORY_LATE_WAIT` 3 or the relay), since at stage 2 the present waits for everything; set explicitly, it also keeps the present off `VICTORY_FRAME_FEEDBACK=2`'s policy |
| `VICTORY_LAND_ASIDE=1` | the command lists of landed flights are reset on a thread of their own instead of at the end of each submission (off by default) |

If materials look wrong, `VICTORY_TABLE_CONSTANTS=0` turns off reading table
constants in place: every table constant is then copied for the draw into
upload memory, as before (more descriptor slots, slower).

## Building

Requirements and further commands are in the upstream README below. The
commands this repository documents:

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute --console series-s
```

A build with debugging aids:

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

## What the fork this tree is based on changed (summary)

As described by fxceless/victory-fork; measurements are from that fork's own
test machine (Windows 11, i7-14700KF, RTX 4070 12 GB), not from the machine
above.

Rendering:
- Meshlet culling read its scene buffers as the wrong kind of view, so the world drew almost nothing; fixed.
- Per-draw descriptor tables could run out, after which draws used slots never made for them and the card hung; fixed.
- Light packed by the game's compute shaders was read back through empty copies (sky, probes, fog); fixed for every mip level and layer.
- The fog reconstruction, shadow-mask, hair, blend-shape skinning, fire and several material programs did not translate; fixed.
- Gathers at a named mip level (with and without an offset) read level 0; fixed.
- Implicit-level samples outside pixel shaders produced invalid DXIL in 730 compute programs; fixed.
- Rectangle lists, depth and stencil twins for depth textures written by compute, texture read-backs.
- Ray tracing, in progress and off by default: the game's own structure-builder programs translate, title memory can be read by raw address from shaders, the hardware ray/box and ray/triangle test is emulated in DXIL (checked against a reference on the GPU), and 59 of the 64 trace programs translate. Nothing is lit by it yet. See [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md).

Speed and memory:
- Waits for memory values, write-watch queries and page queries no longer walk page tables per call.
- Large title buffers get a copy in card memory, with shader writes found by hashing on the card.
- Submissions can be sent ahead of the wait for the card (`VICTORY_LATE_WAIT`), with a relay thread that lands results as the card finishes (`VICTORY_RELAY`).
- Title memory can be opened as a graphics heap so that buffers are used in place instead of copied (`VICTORY_INPLACE`, first stage only).
- The bindless texture queue no longer uploads stale views; the view table no longer grows without bound; the pipeline library is mapped instead of read into memory.
- An intermittent device loss was traced to the pipeline library file being replaced while mapped; fixed.
- Translated shaders get clean-up passes over the finished module (dead code, constant folding, reuse of values, branch facts); the pack's host instruction count falls by about 43 % (`VICTORY_SHADER_OPT=0` turns them off).

`VICTORY_TEXTURE_DROP_MIPS` (see the table above) came from a pull request
to that fork; its author measured about 210 ms a frame in the first room
instead of about 430 on an RTX 4060 Ti 8 GB.

Everything below this line is upstream VIctory's README as this fork took it
over (its status section, which described upstream at that time, is left out).

---

# VIctory

Runs the title (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager → Performance → Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

`VICTORY_SKIP_BINK=1` skips the title's movies that play once (logos and
the intro); `VICTORY_SKIP_BINK=all` skips the looping ones too, so menu
backgrounds stay dark.

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. With none connected the keyboard is a pad while the
title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_PIPELINE_WAIT_MS=24` | reduce the per-frame wait for background pipeline creation from the default of 500 ms. A draw can be omitted until its pipeline is ready |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
