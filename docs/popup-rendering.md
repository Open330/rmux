# Popup refreshes and flicker

A popup child can emit a small clock or spinner update, but the host used to
repaint its entire popup surface for every PTY read. Removing the preliminary
blank fill prevents one source of flicker; it does not eliminate those full
repaints or the intermediate frames from split child writes.

Popup child-output refreshes now allow row deltas at the attach output boundary.
The comparison uses the last full persistent frame, after generation checks,
rather than a frame the producer merely queued. The cache always retains the
full new frame. Queue replacement, client refresh, resizing, and restoring the
popup after other screen output therefore do not depend on a chain of deltas.

Only self-contained cursor-save/reset/absolute-position/restore rows at one
column and strictly increasing rows qualify. A changed row includes its padding,
so shortened or emptied text erases old cells. Moved/resized row coordinates,
overlapping menus, transient message composition, and other layouts fall back
to the existing full-frame path. Explicit client refreshes are never diffed.

On Unix, the popup reader also uses a fixed 8 ms coalescing window and a 256 KiB
batch limit. It waits for the first byte before starting that window, so idle
popups do not wake periodically. Continuous output cannot keep extending the
window; bytes beyond the limit remain for the next batch. This reduces partial
frames when one TUI update arrives in multiple PTY reads. Windows retains its
existing read scheduling and receives the row-delta improvement.

This does not require forcing synchronized-output support onto terminals that
do not advertise it. Existing terminal capability handling remains in effect.
Batching is not an application-level frame protocol: updates that span the
window can still be observed in parts. Initial paints and full restoration still
need complete frames.

## Reproducing the comparison

Build the candidate with `cargo build --release --bins`, then run:

```sh
python3 scripts/bench/popup_unix.py \
  --baseline /path/to/baseline/rmux \
  --candidate target/release/rmux \
  --muxa /path/to/muxa \
  --output /tmp/popup-comparison.json
```

Each binary needs its matching sibling `rmux-daemon`. The Linux harness records
one real `muxa watch` stream, then replays the same bytes with the same timing.
It creates disposable servers with a 140×40 client and a borderless 99%-height
popup. The source TUI has the matching 140×39 viewport. No user tmux config is
sourced. Status-on samples use a 2-second interval and muxa's real
`status-line --needs-attention` and `status-line --pane` commands. This is not the
user's complete theme/plugin configuration. Captured watch content is temporary
and is removed; only numeric samples are saved.

A Linux run on 2026-09-09 compared the existing `58a679b` build with these changes.
After 2 seconds of warmup, each sample measured 6 seconds. There were two samples
per condition, with binary order reversed for the second repetition:

| Status | Build | Terminal bytes | Row draw blocks | Daemon CPU |
| --- | --- | ---: | ---: | ---: |
| Off | Before | 137,717 | 663 | 0.333% |
| Off | After | 752 | 4 | 0.417% |
| On | Before | 138,350 | 666 | 2.083% |
| On | After | 1,385 | 7 | 2.000% |

Bytes and row draw block counts were identical across both repetitions.
Row draw blocks count the emitted cursor-save sequences, including status rows.
The attach client's measured CPU was zero ticks in all samples. CPU uses one
core = 100%, counts only the daemon/client processes, and excludes the replay
child, muxa daemon, and external status commands. Other host workloads were not
isolated, though this run did not overlap builds or tests.

The observed result is a **99.0–99.5% reduction in terminal output** for this
recording. The CPU differences are around one accounting tick per sample and do
not establish a CPU improvement. This is not a general rmux-versus-tmux benchmark
or a guarantee that every terminal's visible flicker is gone.

The running daemon must use the new build to benefit. Replacing the CLI file
alone does not update an already running server's renderer.


## Checking whether the fix is running

On Linux, compare the SHA-256 of `/proc/<daemon-pid>/exe` with the installed
`rmux-daemon` file. A process can keep executing an older binary after the file
has been replaced; the CLI version string alone does not establish which
renderer a live server uses. Check the daemon serving the affected socket.

A second isolated comparison on 2026-09-11 used a copy of the affected live
server's executable as the baseline and the `dbbbbcc` build as the candidate.
For the same recorded watch stream, each six-second sample produced:

| Status | Running older build: bytes / row blocks | Candidate: bytes / row blocks |
| --- | ---: | ---: |
| Off | 89,221 / 429 | 564 / 3 |
| On | 89,854 / 432 | 1,197 / 6 |

These single samples confirm substantially less popup repaint output. They do
not measure subjective flicker in every terminal or establish a CPU gain;
compilation was running concurrently. They also do not diagnose flicker outside
popups. Applying the renderer to an existing server requires a planned server
restart; replacing the CLI or reloading the configuration is insufficient.
