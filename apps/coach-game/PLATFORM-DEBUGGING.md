# Platform debugging: getting at traces & logs per target

*Where the evidence lives on each platform, and the techniques that work
there.* This is the **debugging** companion to [`BUILD.md`](BUILD.md) (which
owns *build & packaging*). For the trace **format** and how to *replay* /
read a trace, see [`AGENTS.md`](AGENTS.md) ("Every run also records a UX
trace…") and [`CONTRIBUTING.md`](CONTRIBUTING.md) — not restated here.

Every run writes a trace bundle to a `traces/` directory. All files from one
run share a stamp `<YYYY-MM-DD-HHMMSS-mmm>`:

| file | what |
| --- | --- |
| `<stamp>-ux.jsonl.gz` | UX trace (gzip JSONL, one object/line, flushed each frame) |
| `<stamp>-engine-input.wav` | captured mic audio |
| `<stamp>-engine-input.features.jsonl` | per-hop pitch features |
| `<stamp>-engine-input.manifest.json` | run manifest |
| `<stamp>-log.jsonl` | telemetry log (INFO/WARN/… lines; flushed per write) |

The catch is **where `traces/` resolves** and **how you read it off the
device** — that differs per platform below.

## macOS

The easy case. `cargo run -p coach-game` runs the unbundled binary; `traces/`
is just a directory in the repo. Boot time, stderr-vs-stdout, the hard-kill
gzip-recovery one-liner, F10 marks, and `--replay` all live in
[`AGENTS.md`](AGENTS.md) — read that first; it is the macOS debugging surface.

Quick reminders that bite:
- **~3 s to boot, give a smoke test ≥6 s** before `kill`, or you kill it
  before the window/mic/first frame.
- **Bevy logs to stderr.** Capture with `2>&1`, not `>file` alone.
- Latest trace = lexicographically greatest file in `traces/`.
- **Test live audio in `--release` only** — see below.

### Live audio needs `--release` (debug silently drops ~half the samples)

Any run that captures audio — real mic *or* `--replay-audio <wav>` — must be
built `--release`. In **debug**, the engine (YIN pitch detection over a 2048
window every 512-sample block) is unoptimized and runs **slower than
realtime**, so the data-plane worker can't drain its ~85 ms ring fast enough.
The realtime side then drops whatever doesn't fit — measured at **~45 % of all
samples** for the sa-re-ga-ma clip. The recorded `-engine-input.wav` comes out
roughly **half the length** of the input, glitchy, and with a garbage pitch
track; live audio sounds the same way. The only on-screen tell is a single
summary line at shutdown: `data-plane: RT samples dropped (worker fell
behind)`.

This is the same "DSP is prohibitively slow in debug" rule from the workspace
`CLAUDE.md`, applied to live audio. Proof: the identical WAV through
`cargo run --release` records a **bit-perfect** copy of the input (normalized
cross-correlation 1.0, no drops); through `cargo run` (debug) it loses 45 %.

```
cargo run -p coach-game --release -- --replay-audio <wav>   # clean
cargo run -p coach-game            -- --replay-audio <wav>   # ~45% dropped
```

To diagnose a suspected drop, compare the input WAV against the recorded
`traces/<stamp>-engine-input.wav`: matching length + high cross-correlation =
healthy pipeline; a short, low-correlation output = the worker fell behind
(rebuild `--release`). Note the recorded WAV is **32-bit IEEE float**
(`WAVE_FORMAT_EXTENSIBLE`, subformat 3) while the test clip is 16-bit PCM, so a
naive reader that assumes PCM will fail to parse it — read the `fmt ` chunk's
subformat, don't trust the top-level tag.

### Previewing the iOS layout on Mac (no simulator)

To eyeball how the UI lays out at phone dimensions without booting the
simulator, set `GURUKUL_DEVICE_SIZE="w,h,scale"` — it forces a fixed logical
window size and scale-factor override. iOS is **landscape-locked** (see
[`BUILD.md`](BUILD.md)), so width > height:

```
GURUKUL_DEVICE_SIZE="852,393,3" cargo run -p coach-game   # iPhone 15
```

| device (landscape) | value |
| --- | --- |
| iPhone 15 / 14 Pro | `852,393,3` |
| iPhone SE | `667,375,2` |
| iPhone 15 Pro Max | `932,430,3` |
| iPad mini | `1133,744,2` |

This is a **layout proxy only** — it shows fit, spacing, overflow, and scale.
It does *not* reproduce touch input, safe-area insets (notch/home-bar), the
iOS audio-session/mic path, or the `BorderlessFullscreen` surface quirk. For
those, use the real simulator or device (below).

### Screenshotting the running window

Use [`scripts/shot.sh`](scripts/shot.sh) (`apps/coach-game/scripts/shot.sh
[out.png]`) to capture the live window. **Don't** reach for AppleScript window
bounds or a fixed `screencapture -R x,y,w,h` region:

- `cargo run` launches an **unbundled raw binary** (no `.app`), so the app is
  *not* AppleScript-scriptable — `System Events … front window` returns
  nothing, and a hardcoded `-R` region silently drifts out of alignment,
  producing a stretched/shifted image that misreads the layout (e.g. a
  centered dial looks off-center). Trust the UX **trace** over such a shot.
- The fix `shot.sh` uses: CoreGraphics' on-screen window list *does* see the
  window (owner `coach-game`), giving a stable window ID that
  `screencapture -l<id>` crops exactly. It's a built-in `swift` snippet — no
  install, and it needs only **Screen Recording** permission, which the
  terminal already holds (Automation/Accessibility is *not* needed).

If a capture comes back **all black**, that terminal lacks Screen Recording —
grant it in *System Settings → Privacy & Security → Screen Recording* (the
permission is per-terminal-app, so a new terminal needs its own grant). For
geometry questions where a shot is ambiguous, the UX trace's `geom` channel is
the authoritative source — it is the app's own measured pixels and cannot be
distorted by capture tooling.

## iOS

iOS SIGKILLs an app on suspend (no `Drop`/destructors run), and in Free
Practice there is no Quit and no way out, so artifacts are sealed by
**periodic flush** during the run — a hard kill still leaves a readable WAV,
log, and trace. That is why the techniques below find intact files even
though the app was force-terminated.

The two iOS targets store traces in the **app sandbox**
(`Documents/traces/`), but you reach that sandbox completely differently on
the simulator vs a physical device.

### Simulator — `simctl get_app_container`

The sim sandbox is a normal directory on your Mac. Resolve it while the sim
is booted (bundle id from [`ios/Info.plist`](ios/Info.plist)):

```sh
DATA=$(xcrun simctl get_app_container booted com.kaiwalya.gurukul.game data)
ls "$DATA/Documents/traces/"
gzcat "$DATA/Documents/traces/<stamp>-ux.jsonl.gz" | jq .
```

Full notes (container stability across shutdown, new UUID on reinstall) are
in [`BUILD.md`](BUILD.md) → "Retrieving trace bundles from the iOS
simulator". The sim mic uses the Mac's microphone, so audio-capture works
there — which makes the sim a poor proxy for device audio bugs (see the
cpal lesson below).

### Physical device — `devicectl device copy from`

`simctl get_app_container` does **not** work for a device. Pull the whole
`traces/` directory out of the app's data container with `devicectl`:

```sh
DEV=<your-device-id>                              # xcrun devicectl list devices
BID=com.kaiwalya.gurukul.game
DEST=$(mktemp -d)
xcrun devicectl device copy from --device "$DEV" \
  --domain-type appDataContainer --domain-identifier "$BID" \
  --source Documents/traces --destination "$DEST"
ls "$DEST/traces" 2>/dev/null || ls "$DEST"
```

- The `Failed to load provisioning paramter list … "No provider was found."
  Code=1002` lines are **harmless noise** — the copy still succeeds; grep for
  `File received from Device` to confirm.
- The destination layout mirrors the source; the files land under the
  `--destination` dir (sometimes directly, sometimes under `traces/`).
- The newest `-log.jsonl` is the fastest first read — our telemetry log is
  plain text and flushed per write, so even a crashed/force-quit run has it.

### Device system log — `idevicesyslog` (the heavy artillery)

Our own telemetry log is often **not enough** on a device, because errors
from the audio stack surface in *other* processes (`audiomxd`,
`mediaserverd`, `AudioToolbox`), and because **cpal collapses every
CoreAudio `OSStatus` to one opaque `DeviceNotAvailable`** (its own source
says `// TODO need stronger error identification`). When the telemetry log
just repeats the same catch-all, go to the **device unified log**.

`log stream --device` is **not** supported on current macOS, and
`devicectl device process monitor` is not a syslog stream. What works is
`idevicesyslog` from **libimobiledevice** (`brew install
libimobiledevice`):

```sh
UDID=$(idevice_id -l | head -1)                    # confirm the device is seen
OUT=/tmp/dev-syslog.txt
# Launch fresh, then capture EVERYTHING to a file for ~20 s while you repro:
xcrun devicectl device process launch --terminate-existing --device "$DEV" "$BID"
timeout 22 idevicesyslog -u "$UDID" > "$OUT"       # <- now reproduce the bug on the phone
echo "captured $(wc -l < "$OUT") lines"
```

Then **grep the file** (don't over-filter live — the failing lines come from
processes you won't predict):

```sh
grep -inE "remoteio|auremoteio|kAudioUnitErr|IsRecording|modes\"|StreamFormat|err [0-9-]" "$OUT" \
  | grep -ivE "found no value|empty base plist|CFPrefs|PERF: Received"
```

Lessons that made this productive:
- **Capture to a file, filter after.** A 20 s window is ~100 k lines; live
  `grep` with a process filter (`-p coach-game`) misses the audio-daemon
  errors entirely.
- **Filter on *behavior*, not your process name.** The decisive evidence for
  the device-mic bug was `audiomxd`'s `set_play_state` records showing
  `"IsRecording":false` and `modes:" Output"` — i.e. the app only ever built
  an **output** RemoteIO unit, never a recording one. `grep -c
  '"IsRecording":true'` returning **0** across the whole capture is what
  proved it.
- **Watch the audio session category in the log.** `Category =
  SoloAmbientSound` (playback default) vs `'reca'`/`Record` on a RemoteIO
  client tells you whether record mode was actually active when the unit was
  built.

## The general method (what this session taught)

When a platform-specific failure resists the obvious fixes:

1. **Read the telemetry `-log.jsonl` first** — cheapest signal, already on
   the device.
2. **If the error is a catch-all** (`DeviceNotAvailable` and friends), stop
   trusting it — the real `OSStatus` was discarded upstream. Go to the
   platform's own log (device syslog above).
3. **Capture wide, filter after.** Save the raw log; grep iteratively.
4. **Disprove theories with counts, not vibes.** `grep -c` for the thing
   that *must* be true if your theory holds (e.g. "a recording unit was
   built") converts a guess into a fact.
5. **The simulator is a weak proxy for device audio.** The sim's RemoteIO is
   lenient; the real one enforces input-bus enablement, buffer sizing, and
   route/rate rules the sim ignores. A clean sim run does **not** mean the
   device path works.

### Known open issue (device mic)

As of Phase 1.6.2, device mic capture does **not** work: cpal 0.17.1's iOS
input path never enables the RemoteIO **input** bus on real hardware — the
device syslog shows only output units (`IsRecording` never true). Sim mic and
Mac mic are unaffected. Tracked for a cpal version bump / patch / native
input adapter; see [`docs/PHASE_1_6_PLAN.md`](../../docs/PHASE_1_6_PLAN.md).
