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

## First launch

On an empty shader cache the first launch plays the audio and shows the
credit titles, but the 3D scenes stay **black for about 10-12 minutes** while
about 2,900 pipelines are built. Measured once on the test machine below: a
picture after about 10 minutes, the queue empty after 12. In that time some
green static and a short hang with a loading wheel were also seen.

What to do: let the opening scene play through once without pressing anything
to skip it, then close the game and start it again. (`VICTORY_SKIP_BINK=1`
from the table below only skips the pre-rendered movies; the opening scene
meant here is drawn by the game and still plays.) On the second launch the opening scene has a picture
from the first moment (tested once: 2,847 cache hits, 2 misses).

The opening cutscene itself is slow (about 2 fps reported). Play gets faster
once you are in control (the bunk room at about 180 ms a frame).

## Test machine and status (measured 2026-11-25 and 2026-11-26)

Windows 11, NVIDIA RTX 3070 8 GB (driver 621.24), AMD Ryzen 5 3600 (6 cores /
12 threads), 32 GB RAM; the title's Series S mode at 1920x1080 output, with the
switches in the table below. One PC; your numbers will differ.

- Longest session: 5 hours 45 minutes on 2026-11-26 (free roaming at night,
  then a long taxi ride left running), closed by hand, without a crash and
  with 0 logged errors. The process's private memory grew slowly over it
  (about 20 GB after two hours, 22.6 GB near the end; one run). On
  2026-11-25 the first Chapter I mission was played through in a 104-minute
  session and saved by hand.
- 2026-11-26: 51 runs, 0 logged errors, no device loss, no crash.
- Frame times on the reference scene (afternoon, parking lot, recommended
  set), as medians of the log's 5-second lines by draws per frame: 205-215 ms
  at 7,000-9,000 draws, about 140 ms at 3,000-5,000 and about 115-120 ms at
  1,000-3,000 (measured with `VICTORY_AMBIENT=0.6`, which is no longer in
  the recommended set; see "What changed since the seventh snapshot"). The
  picture is low
  resolution, with weak edge anti-aliasing on thin geometry (power lines,
  leaves). On this machine the dense frame is mostly CPU time.

## New in the first snapshot (compared with the fork it is based on)

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

## What changed in the second snapshot (since the first)

All new behaviour is behind switches (see "Switches added since the first
snapshot" below), and each one that is on by default can be turned off.

- Six ports, credited in CREDITS.md: repeated output target bindings skipped
  (fork PR 1215); card copies brought up to date in three batched steps
  (fork PR 1227); merged write intervals compacted in place (fork PR 1232); a
  per-thread memory of buffer lookups (fork PR 1237); short fills of upload
  memory copied in place (fork main `de4146a0`); counters of where a
  landing's time goes (triippywhale `962e6af5`).
- Cheaper copies of title memory: source reads of one refresh split over the
  worker pool, asks that are already held skipped, a bounded alias lookup,
  and counters for all of it.
- Four cuts in the handling of write guards, each behind its own switch and
  off by default, with counters.
- Frame end: counters of what the present waits for, and an experimental
  present that waits only for the read-backs the title needs
  (`VICTORY_LATE_WAIT=3`).
- Input: stale latched and queued presses are dropped instead of replayed
  late, and an optional trace of the button edges the title is given.

Measured on the test machine (medians of the 5-second log lines, one or two
runs each, so weak evidence):

- Heavy scenes at 7,000-9,000 draws: 232-270 ms on this tree against
  241-245 ms on the first snapshot. No proven gain in steady frame time.
- The stalls while driving were measured as the texture refresh waiting on
  the title's memory remaps: 16-39 ms a frame on average, single waits up to
  0.7-0.8 s. `VICTORY_MAPPING_RANGES=1` removed that wait (0.1 ms average),
  but its run was not faster on average (313 against 270 ms in heavy frames,
  one pair of runs).
- `VICTORY_LATE_WAIT=3` is experimental and slower; not recommended. One
  run: heavy frames (7,000-9,000 draws) averaged 559 ms against 270 ms with
  `VICTORY_LATE_WAIT=2`; about six submission splits a frame, the present
  still waited 58-70 ms and only about 3 ms of that was recovered; it felt
  slower in play. The recommended value stays 2.
- 1280x720 output did not reduce card time and is not recommended.

## What changed since the second snapshot

- The title's physical memory is now taken from sections of up to 2 MiB
  instead of one section per request (the idea and patch from fork issue
  1089, ported). On by default; `VICTORY_SECTION_MB=0` restores the old way.
- Two ports as switches, off by default and not measured here: copy-backs
  into write-watched memory are no longer mistaken for title writes
  (`VICTORY_BACK_QUIET`, fork PR 1188; its known race: a title write to the
  same 4 KiB page between the query and the reset can be missed), and
  texture retries and fresh texture slots are picked in turns
  (`VICTORY_PICK_TURNS`, fork PR 1246).
- A read-back age probe (`VICTORY_BACK_LATCH`, off): counters only, for
  diagnosis; it changes no behaviour.
- `VICTORY_AMBIENT=0.6` and the three copy and binding switches that were
  optional before are now part of the recommended set below.

Measured on the test machine (5-second log lines; weak evidence):

- With 2 MiB sections, the texture refresh's wait on the title's memory
  remaps fell from 37 ms a frame on average (single waits up to 816 ms; 68 of
  228 samples over 20 ms) to about 1 ms (worst 28 and 62 ms; 1 sample over
  20 ms in each of two runs, of 314 and 331 samples).
- Heavy frames at 7,000-9,000 draws were 232 ms in the first of those two
  runs (the second ran while a screen recorder was recording and is not
  comparable).
- In play, the half-second freezes while driving were judged gone ("pretty
  decent feel").
- `VICTORY_AMBIENT=0.6`: many shaded areas are no longer pure black; judged "a
  lot better" in play. It does not lift everything: shadows cast on open
  ground and some building fronts stay close to black. No cost in frame time
  was seen in the runs with it; it has not been measured against a run
  without it.
- No gain in steady frame time over the earlier snapshots is proven.

## What changed since the third snapshot

Nothing here makes the tree faster. This snapshot adds one correctness fix,
a set of counters, and what a night of measurements on the test machine
showed (2026-11-26).

- A raw buffer view whose range runs past its buffer is now kept inside it
  or left null, instead of being handed to the driver as a description it
  refuses (fork PR 1266, ported; `VICTORY_RAW_VIEW_CLAMP`, on by default,
  `0` restores the old descriptors). Eight counters in the 5-second line say
  how often it happens. On the test machine they stayed at zero for a whole
  run, so the fix changes nothing there; it is included for other cards.
- Counters behind `VICTORY_FRAME_WHY2=1` (off; no behaviour change), as
  5-second lines beginning `why2`: queries by type; the culling compute
  dispatches; a timeline of the presenting thread, of the title's other
  threads and of the submit worker and the relay that adds up to the frame;
  what the worker does while the present drains; the slowest frame of the
  window; and descriptor work by cause.

What one run with those counters showed (four minutes in town, about 212 ms a
frame; weak evidence):

- The presenting thread spends 187 of 212 ms inside the present, waiting. It
  records nothing itself. That wait is not a cost of its own: it is the sum
  of what follows.
- About three of the title's other threads are inside this layer's recording
  code at any moment (637 thread-ms a frame, five callers at 119-132 ms each).
- The submit worker prepares for 126 ms a frame (240 at worst) and is idle
  for 71; the relay waits 85 ms for the card and is idle for 88.
- 235,590 descriptor records are checked a frame and 21,187 of them (9 %)
  have changed.
- The title issues about 34 occlusion queries a frame, resolved in one batch,
  and culls mainly with compute passes. This layer answers every query as
  "visible".

Tried on the test machine and not adopted (each one or two runs; frame times
by draw count against runs of the same night):

- The general ticket fix for 2-D compute groups on 32-lane cards (fork PR
  1261): with the fill light off, shaded areas were as black with it as
  without it.
- `VICTORY_HEAP_PASS_UNMARKED=1`: the 8,685 self-marked slot looks a frame go
  away and the picture is unharmed, but the frame is not shorter (155, 199
  and 214 ms at 3,000-5,000, 5,000-7,000 and 7,000-9,000 draws over 78
  samples).
- `VICTORY_LATE_WAIT=1` and `=0` are both slower than `=2` (about 294 ms and
  280-357 ms at 5,000-7,000 draws against about 200) and neither flashes. At
  `0` the submissions are not sent ahead at all. Stages 1 and 2 both let
  everything land before the present returns; stage 2 is faster earlier in
  the frame, where a refresh that meets pending shader writes merges only
  the bytes the title changed instead of waiting for every flight. The
  flashing belongs to `VICTORY_LATE_WAIT` unset with the relay on.
- Two experiments that are not in this snapshot: texture refreshes and buffer
  copies on a pool of four threads, alone and with the write-watch query
  moved out from under the journal's lock. The threads then queue on the
  guards' lock (193 thread-ms a frame) and the frame is no shorter (222 ms at
  7,000-9,000 draws, 272 ms above 9,000).

## What changed since the fourth snapshot

Everything new in this snapshot is behind a switch that is off unless set,
so without switches the tree behaves as the fourth snapshot did. The parts
were written and first run in one night (2026-11-26) on the test machine;
each was run once or twice, none long. Read the numbers as first readings.

- **Series S: a history blend after the stretch**
  (`VICTORY_UPSCALE_HISTORY=<blend>`). In Series S the title renders
  1280x720 with a different sub-pixel offset each frame and asks its
  upscaler to make 1920x1080 of it. This family answers that request with a
  plain stretch of the current frame, so each shown frame is one raw sample:
  leaf edges are grainy, thin wires are dotted, and a distant sign's border
  shows a different part each frame. With the blend, standing still, the
  player's words were "the background and power lines look wayyyy better".
  A plain blend trails badly in motion at 5 fps (0.85: "trails real bad";
  0.5 with the colour clamp, `VICTORY_UPSCALE_HISTORY_MV=1`: "much better";
  0.35: "still feels dreamy").
- **The same, following motion** (`VICTORY_UPSCALE_HISTORY_MV=2`). History
  is moved along the motion vectors and compared against the depth the
  title hands to the upscaler; it is dropped where a nearer object moved
  away, where the old position is off screen and where the colour no longer
  fits its surroundings. Still pixels accumulate for up to 16 frames, moving
  ones lean on the current frame. The title's own upscale-context flags are
  read when it creates the context (seen here: `0x29`, depth inverted,
  motion vectors without jitter). One short run: 7 % of pixels rejected,
  mean motion 13 pixels, no errors. Not yet judged by eye for long.
  `VICTORY_UPSCALE_MV_JITTERED`, `VICTORY_UPSCALE_DEPTH_INVERTED` and
  `VICTORY_UPSCALE_MV_SCALE=x,y` override the conventions.
- **The texture part of the heap pass on its own thread**
  (`VICTORY_HEAP_WORKER=1`, needs `VICTORY_LATE_WAIT`; fork PR 1007, hand
  ported, with a fix for slots the worker took but never made, and a stop
  path). One short run: 75 ms a frame of work moved off the recording
  threads, recording threads waited 0.02 ms for it. No timing comparison
  yet.
- **Refreshes of title memory proven unwritten are skipped**
  (`VICTORY_LIGHT_REFRESH=1`; self-check `VICTORY_LIGHT_REFRESH_CHECK=<n>`).
  Blocks of guarded view memory are published as clean with the stamp of
  the last tracked write; a refresh or a copy that asks about a published
  range is answered without the guards' lock. Writes that bypass tracking
  (kernel reads into title memory, protection changes, asynchronous I/O)
  open a window or a quarantine. Two runs with the check on (7,148 samples
  in the first), no mismatch, picture unharmed. About 945
  refreshes a frame skipped; preparing 87 ms a frame against 105-146 on
  plain runs, measured with the check on and other work on the machine, so
  not yet a clean comparison. Most asks (about 5,200 a frame) still fall
  back.
- **Preparing on a pool of threads** (`VICTORY_PREP_THREADS=4`) and **the
  write-watch query under per-region locks** (`VICTORY_WATCH_SPLIT=1`).
  Alone they did not shorten the frame here: the threads queue on the
  guards' lock. They are in the tree because the light refresh was measured
  with them.
- **Descriptor records compared only on pages that were written**
  (`VICTORY_DESC_DIRTY=1`; self-check `VICTORY_DESC_DIRTY_CHECK=<n>`). On
  this title it skips nothing in mode 1: the heaps sit in physical-page
  views, which the kernel's write watch does not cover (the 5-second line
  names the cause). Mode 2 uses the guards for that memory and has not been
  run. The whole compare is small in any case (about 240,000 records, a few
  milliseconds a frame).
- **Buffer-pass copies are released with their descriptor heap** (fork PR
  1276, hand ported; always on). Before, the cache of host copies was keyed
  by the heap's address and never emptied, so a destroyed heap leaked its
  copies and a new heap at the same address inherited a stale cache.
- **Host sampler heaps of 2,048 slots** (`VICTORY_SAMPLER_HEAP_FULL=1`,
  off). The translator clamps sampler indices to 2,047, but a host heap was
  only as large as the title's, so a clamped index could read past its end.
  Not yet run.
- A `why2 heap worker` line joins the `VICTORY_FRAME_WHY2` counters.

What the same night showed without a code change:

- The black shade is the title's ray-traced lighting walking an empty
  scene: the console's structure builder (about 52 compute programs, the
  roughly 2,700 "dropped without a host pipeline" draws a frame) is off in
  this family unless `VICTORY_BVH_BUILDER=1`, which costs seconds a frame.
  The eight compute programs this card refuses are unrelated to it.
- Fork PR 1274 (the 2 MiB sections of the third snapshot, proposed to the
  fork) was run by others on the fork's own code: remapping 42.8 to 5.0 ms
  a frame on an RTX 3080, no change in frame rate there.

## What changed since the fifth snapshot

Everything new here is behind a switch that is off unless stated, so without
switches the tree behaves as the fifth snapshot did, except for the changes
marked "always on" or "on". Sources are in CREDITS.md.

- **Gather segments** (`VICTORY_GATHER_SEGMENTS=1`, with
  `VICTORY_INDIRECT_GATHER=1`): once a list stops gathering its single
  indirect records, later records are clamped in segments, one pass each, so
  they no longer fall back to a pass per record. The clamp shader takes its
  record count from a counted table.
- **Indirect records the title can still change are clamped at the draw**
  (`VICTORY_GATHER_STRICT`, on): a gathered record whose source may be
  written before the draw falls back to a pass in front of that draw.
- **Textures the card had no room for are tried again after a second**
  (`VICTORY_TEXTURE_ROOM_RETRY`, on), at most `VICTORY_TEXTURE_ROOM_RETRY_PER_S`
  a second; before, such a texture was refused for the whole run.
- **A texture that a placed view aliases is held while a copy uses it**
  (always on): before, the copy could keep a bare pointer to a texture that
  was released under it.
- **BCPack (BC7) rotation only in modes 4 and 5** (always on): blocks of the
  other modes were decoded with swapped channel precisions.
- Texture rechecks: periodic (`VICTORY_HEAP_RECHECK_EVERY`) and, after a
  change, in the next frame (`VICTORY_RECHECK_SOON`).
- More `VICTORY_FRAME_WHY2` lines and a set of experiments for the CPU side
  of the frame (guard lock, write watch, heap pass, queues, thread placement,
  ambient light), all off; see the switch table.

## What changed since the sixth snapshot

- **A speed regression of the sixth snapshot is fixed.** The sixth snapshot
  (a4b6ebd6) was about 65-75 ms a frame slower in dense scenes with the
  recommended switches: its new safety rule (`VICTORY_GATHER_STRICT`) also
  demoted indirect-window draws (`VICTORY_INDIRECT_WINDOW=1`), sending them
  back to one clamp pass per record. This snapshot applies the rule by
  default only to records considered for the prelude gather and for
  segments; `VICTORY_GATHER_STRICT=2` also applies it to window hits, as
  before, and the gather line counts those as `demoted_window`.
- If you stay on the sixth snapshot: set `VICTORY_GATHER_STRICT=0`, and then
  do not use `VICTORY_INDIRECT_GATHER`.

Medians of the log's 5-second lines at 7,000-9,000 draws, same settings, same
day (one save, one PC, short runs):

| Build | ms a frame | Lines |
|---|---|---|
| before the safety rule | 208 | 5 |
| an older build, also without the rule | 210 | 14 |
| sixth snapshot, rule on window hits | 276 | 25 |
| sixth snapshot, rule on window hits (second run) | 285 | 7 |
| sixth snapshot with `VICTORY_GATHER_STRICT=0` | 203 | 11 |
| this snapshot | 215 | 5 |

The picture was the same in all of them.

## What changed since the seventh snapshot

No code change: the recommended settings and this README only.

- **`VICTORY_AMBIENT=0.6` is no longer in the recommended set.** It was
  added in the third snapshot to make shaded areas readable by day. At night
  the same constant fill lifts every dark surface to one grey: headlights
  and street lamps no longer stand out, and the scene looks flat. On the
  same save at night, the same build without it (`VICTORY_AMBIENT` unset or
  `off`) was judged far better in play; a session of 5 hours 45 minutes was
  then played and left running that way. The frame-time medians did not show
  a cost or a gain from the change (different scenes; not a like-for-like
  measurement).
- Daytime without the fill was then compared on the reference scene
  (afternoon, parking lot) and judged fine; 216 ms at 7,000-9,000 draws
  (3 lines) against 215 with the fill (5 lines). Surfaces facing away from
  the light can be close to black (seen at night: a backlit character's
  back, the side of a character away from headlights). If shade is too dark
  for you, set `VICTORY_AMBIENT=0.6` again, or try the experimental
  `VICTORY_AMBIENT_SKY=1` with it, which lets the fill follow the sky's
  brightness.
- **`VICTORY_UPSCALE_HISTORY=0.85` with `VICTORY_UPSCALE_HISTORY_MV=2` is now
  in the recommended set** (Series S; both exist since the fifth snapshot).
  Edges, wires and foliage are much steadier; in play it was judged a far
  better picture, and the most wanted improvement so far. Compared on the
  reference scene, one short run each, medians at 7,000-9,000 draws:

  | History | ms a frame | Lines | Judged |
  |---|---|---|---|
  | `0.85`, `_MV=2` (follows motion) | 215 | 4 | best picture, moves well |
  | off | 226 | 7 | much worse picture |
  | `0.85`, `_MV=1` (colour clamp only) | 215 | 13 | motion unacceptable |

  No measurable cost: it is one pass on the card, and preparing time was the
  same with it on and off (98 and 99 ms). On a night drive the frame felt
  slower with it; the comparison above does not show that, so it was more
  likely the scene.
- In the title's own settings menu: motion blur off and camera bobbing off
  were judged to feel much better at these frame rates.
- In that long session the card was at its texture budget for the first two
  hours (textures waited for room); none were logged waiting in the
  remaining hours.

## Known problems

- Slow (see the frame times above).
- Occasionally a single object or face shows a black texture that pops in
  and out.
- No bounced light: the title's ray-traced bounce light does not run on
  32-lane cards. Without a fill, shade is close to black; with
  `VICTORY_AMBIENT` the lighting is flat, worst at night (no longer
  recommended; see "What changed since the seventh snapshot").
- Once, on the very first launch, one NPC's face was missing; it was there
  after the restart.
- The opening cutscene runs at about 2 fps (see "First launch").
- The picture's exposure flashes when submissions are in flight across the
  present (`VICTORY_LATE_WAIT` unset with the relay on). The recommended
  `VICTORY_LATE_WAIT=2` lets everything land before the present returns; the
  presenting thread then waits there for most of the frame (see the counters
  above), which is the frame's other work and not a separate cost.
- Hitches while driving. The texture refresh's wait for the title's memory
  remaps is gone with the default 2 MiB sections; other hitches remain, and
  their cause is not yet found.
- With `VICTORY_BAKE_TEXTURES_MS=0`, characters can be baked dark at first.
- Shaded areas are black without `VICTORY_AMBIENT`: the title's bounced light
  does not run. `VICTORY_AMBIENT=0.6` covers part of it with a constant fill,
  the same everywhere: many shaded areas become readable, but shadows cast on
  open ground and some building fronts stay close to black, and night scenes
  turn flat and grey. Real bounced light still does not run.
- Leaf edges, hair and thin wires look speckled or dotted, on every build of
  this tree so far.
- Foliage looks dotted in Series S.
- Water can be drawn white for a minute or two after arriving at it, then
  fills in.
- At 5 fps a quick press of some buttons (crouch) is not always taken by the
  title although the press is delivered; holding the button for a second
  works.
- Eight compute programs are refused by the translator.
- No autosave was seen during a mission.
- The first launch on an empty shader cache, and the first after a change to
  the shader translator, builds every pipeline again (see "First launch").
- Many changes here are specific to this title, and some are marked
  `shortcut:` in the code where they approximate what the console does.
- A few style lints are switched off in the workspace `Cargo.toml` until the
  newer code is tidied.

## Switches used on an 8 GB card

Environment variables, set before starting `victory.exe`. Each is described
in a comment where the code reads it.

| Switch | What it does |
|---|---|
| `VICTORY_SHADER_LOOP_LIMIT=256` | translated shader loops stop after 256 iterations per lane instead of 2^20; every number in this README was measured with it (it has its own shader cache entries) |
| `VICTORY_DISPLAY_MODE=1920x1080` | the display size the console reports; all runs here used it |
| `VICTORY_SKIP_BINK=1` | skips the title's pre-rendered movies that play once (logos and the intro movie); `=all` also skips looping ones |
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
| `VICTORY_COPY_PLAIN=1` | copies of title memory into host buffers are made in place by this process, with the kernel's copy behind them for a page that is not there |
| `VICTORY_FILLS_PLAIN=1` | a list's fills of under 256 bytes are copied in place instead of through the kernel's copy (needs `VICTORY_COPY_PLAIN=1`) |
| `VICTORY_REUSE_TARGET_BINDS=1` | repeated bindings of the same output targets are skipped |
| `VICTORY_CARD_REHASH_RUNS=1` | the hash passes that check the card's copies of title buffers run once per run of changed pieces instead of once per range (used in the best run above; its own share of that run's frame times is not separated yet) |
| `VICTORY_UPSCALE_HISTORY=0.85` | Series S: each stretched frame is blended with the previous ones before the interface is drawn; 0.85 is the most weight history may have |
| `VICTORY_UPSCALE_HISTORY_MV=2` | the history follows the title's motion vectors, with depth and colour rejection and a weight per pixel (without it the blend trails badly in motion) |

If materials look wrong, `VICTORY_TABLE_CONSTANTS=0` turns off reading table
constants in place: every table constant is then copied for the draw into
upload memory, as before (more descriptor slots, slower).

`VICTORY_FILLS_PLAIN=1`, `VICTORY_COPY_PLAIN=1` and
`VICTORY_REUSE_TARGET_BINDS=1` have been set in the recent runs without a
crash; no frame-time gain from them is proven.

### Switches added since the first snapshot

Defaults as the code reads them. "On" switches are turned off with `=0`;
"off" switches are turned on with `=1` unless stated otherwise.

| Switch | Default | What it does |
|---|---|---|
| `VICTORY_CARD_SYNC_BATCH` | on | the list that brings copies in the card's memory up to date records every changed range, then every state change in one call, then every hash pass, instead of copy by copy |
| `VICTORY_LOOKUP_CACHE` | on | a thread remembers what its asks for a buffer's host copy found, until the tables change |
| `VICTORY_BOUND_ALIAS_LOOKUP` | on | the guard looks up other views only for pages with a bounded number of them (the shared placeholder page can have thousands) |
| `VICTORY_SKIP_HELD_ASK` | on | the submitting thread skips building ranges for a kernel ask that is already held and would be discarded |
| `VICTORY_LATCH_DROP_STALE` | on | queued taps expire from the time of the physical press; only live levels after two seconds without reads; pending presses cleared on focus loss and pad disconnect (`=0`: replayed as before) |
| `VICTORY_FILLS_PLAIN` | off | a list's fills of under 256 bytes are copied in place instead of through the kernel's copy; needs `VICTORY_COPY_PLAIN=1`; `=alt` switches it every 20 seconds for comparisons |
| `VICTORY_REUSE_TARGET_BINDS` | off | repeated bindings of the same output targets are skipped |
| `VICTORY_SPLIT_SOURCE` | off | disjoint runs of one refresh read from title memory as jobs on the worker pool |
| `VICTORY_GUARD_DIRECT_OPEN` | off | a write fault on one guarded block opens that block directly |
| `VICTORY_GUARD_REUSE_ARM` | off | a guarded block with other views that is still read-only is noted clean and only its other views are guarded again, without a new call for the block itself |
| `VICTORY_GUARD_BATCH_ALIAS` | off | the other views of a guarded run are armed in one call, retried block by block if Windows refuses it |
| `VICTORY_GUARD_HOT` | off | views that fault again and again are held open across shadow refreshes |
| `VICTORY_LATE_WAIT=3` | (2 recommended) | experimental and slower: the submission is split after the last producer of a read-back the title needs, and a present sent ahead waits only for those (falls back to stage 2 without the relay) |
| `VICTORY_LATE_SET` | `exposure+water` | with `VICTORY_LATE_WAIT=3`, which read-backs the present waits for: `exposure`, `exposure+water` or `all` (unknown values mean `all`) |
| `VICTORY_FRAME_END_WHY` | off | with `VICTORY_LATE_WAIT` 2 or 3, a 5-second line of what the present waits for (diagnostic) |
| `VICTORY_SOURCE_DIFF` | off | with guarded copies (the default copy mode), a census of how many source bytes really changed (diagnostic) |
| `VICTORY_INPUT_WHY` | off | logs each button edge the title is given, with its source (diagnostic) |
| `VICTORY_SECTION_MB` | 2 | the title's physical pages come from sections of up to this many MiB; `0`: one section per request, as before |
| `VICTORY_BACK_QUIET` | off | copy-backs into write-watched memory are not read as title writes (known race: a title write to the same 4 KiB page between the query and the reset can be missed) |
| `VICTORY_PICK_TURNS` | off | texture retries and fresh texture slots are picked in turns |
| `VICTORY_BACK_LATCH` | off | 5-second counters of how old read-backs are when the present returns (diagnostic; changes no behaviour) |
| `VICTORY_AMBIENT` | off | a constant least amount of indirect light in place of the title's bounced light; `=<level>` sets it (`0.6` was in the recommended set until the seventh snapshot: readable shade by day, flat and grey at night; `0` is none, an unreadable value means 0.6); where the title's ray-traced light probes would supply bounced light (they do not run here), it is the same everywhere and not real bounced light |
| `VICTORY_RAW_VIEW_CLAMP` | on | a raw buffer view stays inside its buffer: a range past the end is shortened to the whole dwords that fit, an unaligned or empty one becomes a null view; `0`: the old descriptors, and no counting |
| `VICTORY_FRAME_WHY2` | off | 5-second lines beginning `why2`: queries, culling dispatches, the title's and the worker's timelines, descriptor work by cause; since the fifth snapshot also settle causes, heap-slot counts, submit worker idle time and guard-lock hold durations (diagnostic; changes no behaviour; the timeline is approximate relay observations, not exact frame attribution) |
| `VICTORY_UPSCALE_HISTORY` | off | Series S: blends each stretched frame with the previous ones before the interface is drawn; `=<blend>` from 0 to 1 is the most weight history may have (0.9 with `_MV=2`, 0.5 with `_MV=1`) |
| `VICTORY_UPSCALE_HISTORY_MV` | off | `1`: history clamped to the current 3x3 colour range; `2`: history follows the title's motion vectors, with depth and colour rejection and a weight per pixel |
| `VICTORY_HEAP_WORKER` | off | the texture part of the descriptor heap pass runs on a thread of its own (needs `VICTORY_LATE_WAIT`); textures can show a frame late |
| `VICTORY_LIGHT_REFRESH` | off | refreshes and copies of guarded title memory proven unwritten are answered from published clean bits; `VICTORY_LIGHT_REFRESH_CHECK=<n>` compares every n-th skipped block and answer anyway and counts mismatches |
| `VICTORY_PREP_THREADS` | off | `=<n>`: a submission's texture refreshes and buffer copies are prepared on n threads |
| `VICTORY_WATCH_SPLIT` | off | the kernel's write-watch query runs under per-region locks instead of the journal's lock |
| `VICTORY_DESC_DIRTY` | off | `1`: descriptor records on pages not written since the last pass are not compared (write-watched memory only); `2`: also guarded view memory (not yet run); `VICTORY_DESC_DIRTY_CHECK=<n>` samples skipped ranges |
| `VICTORY_SAMPLER_HEAP_FULL` | off | host sampler heaps are made with 2,048 slots, the last a null sampler (not yet run) |
| `VICTORY_INDIRECT_GATHER` | off | experimental, `=1`: single indirect records that miss their window are clamped together in their list's prelude (needs `VICTORY_INDIRECT_WINDOW=1`). Measured before the safety rule below existed: 237 ms plain -> 218 with gather -> 210 with gather and segments, at 7,000-9,000 draws. With the safety rule about half the records fall back; gather with the rule has not been re-measured since the fix in the seventh snapshot |
| `VICTORY_GATHER_STRICT` | `1` | a record whose source can still change under the card (in-place arguments, or a copy not on the card while submissions are in flight) is clamped at its draw instead. Unset or `=1`: only records considered for the gather and for segments; `=2`: also indirect-window hits (the sixth snapshot's behaviour, slower); `=0`: off (unsafe with gather) |
| `VICTORY_GATHER_SEGMENTS` | off | experimental, `=1` (`=alt` in turns), needs gather: after a list stops gathering, records are clamped in segments, one pass each; removes the "list had stopped gathering" fallbacks (see `VICTORY_INDIRECT_GATHER` for the figure measured before the safety rule) |
| `VICTORY_TEXTURE_ROOM_RETRY` | on | a texture the card had no room for is tried again after a second instead of being refused for the run, at most `VICTORY_TEXTURE_ROOM_RETRY_PER_S` (default 8) a second; `=0` restores the old way. Not yet exercised in our runs |
| `VICTORY_MERGE_CAP` | 512 | `=<runs>`: the most changed runs merged beside flights' writes in a submission before it settles instead |
| `VICTORY_RECHECK_SOON` | off | a texture picked by index that changed after its upload is looked at again in the next frame, not four frames later |
| `VICTORY_HEAP_RECHECK_EVERY` | off | `=<frames>`: uploaded textures picked by index are looked at again every that many frames |
| `VICTORY_STANDING_SKIP` | off | experimental: refreshes the submission's standing write-watch asking vouches for are skipped; its self-check had one unexplained mismatch |
| `VICTORY_PORT_GUARD` | off | experimental: the guards' lock is shared, with per-block claims (not with `VICTORY_GUARD_HOT`) |
| `VICTORY_GUARD_CHUNK` | off | experimental, `=<blocks>` (64 suggested): a guarding lets go of the guards' lock between chunks |
| `VICTORY_VIEW_KNOWN` | off | experimental: view memory the guards once found is left out of the kernel's write-watch askings |
| `VICTORY_WAKE_FIRST` | off | experimental: the queues are woken before the landed lists are reset |
| `VICTORY_PRESENT_FLIGHT` | off | experimental: at `VICTORY_LATE_WAIT=2` the present's copy goes to the card as a flight of its own |
| `VICTORY_HEAP_SAME_SKIP` | off | experimental: the heap pass passes by due slots that would find what was kept (`_CHECK=<n>` samples them) |
| `VICTORY_CORE_PLAN` | off | experimental, `=ours-a`, `ours-b`, `split`, `title-a` or `together`: threads placed on one of two comparable last-level cache groups (CPU sets); `VICTORY_CORE_HARD=1` uses a hard affinity mask instead |
| `VICTORY_AMBIENT_SKY` | off | experimental, `=1` (or `gain,least[,power]`): the fill light follows the sky's brightness |
| `VICTORY_AMBIENT_REFLECT` | off | experimental, `=<part>`: reflections held to that part of the light around |
| `VICTORY_SAME_RECORDS` | off | experimental: descriptor records copied onto the same bytes are not written |
| `VICTORY_DECLARE_ONCE` | off | experimental: a shader-write declaration repeated within a list is recorded once until the list next writes title memory |
| `VICTORY_RUNS_AVX2` | unset | experimental, `=1`: the changed runs under flights' writes found 32 bytes at a time with AVX2 (same runs); `=0` the old compare, timed |

## Send us numbers

Everything above was measured on one PC. Testers with 8 GB cards or older
CPUs are wanted. Please send:

- GPU, CPU, RAM, and the console profile (Series S or X);
- the exact switch set you ran with;
- from one five-minute run with `VICTORY_FRAME_WHY2=1`, the output of
  `perl tools/bands.pl <log>` (frame, preparing and GPU time by draws per
  frame) and `perl tools/timeline.pl <log>` (the frame's timeline from the
  present call, by draws per frame).

Contributions are welcome, especially on the CPU side of the frame.

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
target\release\victory.exe run <game dir>\<the title's exe> --execute --console series-s
```

`--console series-s` selects the title's Series S mode, the only mode this tree
is tested in; without it the title runs in Series X mode. The game folder can be
anywhere outside this repository: the path on this command line is the one
used. Set the switches from "Switches used on an 8 GB card" in the environment
first.

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
