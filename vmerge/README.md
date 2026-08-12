# Video Merger (Rust + Ratatui)

Joins video clips into one `.mp4`. A port of `merge-videos.ps1` with a real
terminal UI: arrow-key selection, in-place reordering, and live progress bars.

Build:

```
cargo build --release
```

The binary is `target/release/vmerge.exe`; a copy sits in the project root as
`MERGE-VIDEOS.exe`, which is the drag-and-drop target.

ffmpeg is found in `PATH`, in an `ffmpeg\bin` folder beside the executable, or
in `%LOCALAPPDATA%\video-merge`. If none of those has it, it is downloaded and
unpacked on first run — no admin rights, nothing installed system-wide.

## Using it

Double-click `MERGE-VIDEOS.exe` and it lists whatever clips are already in its
folder. Or drop clips straight onto the executable to load exactly those, in the
order you dropped them.

### Keys

| Key | Does |
| --- | --- |
| `↑` `↓` / `j` `k` | move the selection |
| `shift+↑` `shift+↓` / `J` `K` | **move the selected clip** up or down |
| `space` | mark a clip |
| `del` | remove marked clips, or the selected one |
| `a` | add clips (type or paste a path) |
| `c` | clear the list |
| `n` | sort by filename (1, 2, 10 — not 1, 10, 2) |
| `m` | release/recapture the mouse |
| `s` | **start the merge** |
| `o` | output file name |
| `q` | quality |
| `t` | target size and framerate |
| `e` | encoder (auto ⇄ cpu) |
| `r` | force a re-encode of every clip |
| `esc` | clear marks, close a dialog, or cancel a running merge |
| `x` | exit |

### Mouse

Clicking works as well as typing: click a clip to select it, click any button in
the bottom bar to run it, click a menu item to choose it, click the yes/no
buttons on a dialog. The scroll wheel moves through the list and through menus.

Capturing the mouse takes the terminal's own text selection away, so `m`
releases it when you want to copy something off the screen — the bottom-bar
button says which way it will go. Cancelling a running merge is deliberately
keyboard-only (`esc`), so a stray click cannot throw away work in progress.

### Drag and drop

Dragging files onto the window works. In raw mode a dropped path arrives as a
burst of keystrokes rather than a line of text, so two things catch it:
terminals with bracketed paste (Windows Terminal) deliver the whole path as one
event, and on older consoles a run of characters arriving faster than anyone can
type is treated as pasted text. Either way it lands in the add prompt, where you
can check it before it is added.

### Command line

Everything works without the screen too. `--no-tui` merges straight away and
prints plain text; that also happens automatically when output is redirected, so
`MERGE-VIDEOS.exe > log.txt` produces a clean log instead of a spinning UI.

```
MERGE-VIDEOS.exe --no-tui --folder D:\clips --output holiday.mp4
MERGE-VIDEOS.exe --no-tui a.mp4 b.mp4 --quality small --encoder cpu
```

| Flag | |
| --- | --- |
| `--folder <dir>` | where to look for clips |
| `--output <name>` | output name or full path |
| `--file-list <file>` | one path per line, in order |
| `--quality` | `visually-lossless` \| `high` \| `medium` \| `small` |
| `--encoder` | `auto` \| `cpu` \| `nvenc` \| `qsv` \| `amf` |
| `--force-reencode` | re-encode even when clips already match |
| `--skip-ffmpeg-download` | fail instead of downloading ffmpeg |
| `--no-tui`, `--no-pause` | plain output; do not wait for a keypress |

An `order.txt` in the folder (one filename per line, `#` for comments) sets the
order in one-shot mode.

## How the merge works

Unchanged from the PowerShell version, because this is where the hard-won
knowledge lives:

- **Duration-weighted target.** The common format is whichever size and
  framerate most of the actual footage is already in — not the largest clip. A
  six-second phone clip must not decide the format for an hour of camera
  footage; it would upscale and frame-double everything else for nothing.
- **Everything goes through MPEG-TS.** Even a pure copy. TS carries a fixed
  90 kHz clock; joining mp4s directly with a stream copy looks fine and then
  silently yields a file whose video ends long before its audio, because each
  mp4 keeps its own timebase.
- **NTSC rationals are preserved.** `30000/1001` and `29.97` are not the same
  format to ffmpeg, and treating them as such costs a needless re-encode.
- **Rotation is tracked.** A portrait phone clip reports 1920x1080 plus
  `rotate:90`, and must not be treated as identical to an unrotated clip.
- **Silent clips get a generated audio track**, or the join desyncs.

### Three strategies

`Invoke-MergeAll` in the PowerShell version called `Invoke-CopyMerge`,
`Invoke-PartialConvertMerge` and `Invoke-ReencodeMerge`. None of the three was
ever defined, so every multi-clip merge fell into the `catch` block and reported
"Something went wrong". Only the single-clip path worked.

They collapse into two functions here, which is what the half-finished
`Invoke-SegmentedMerge` refactor was reaching for:

- `copy_merge` — every clip already shares one format, so each is remuxed
  untouched and the set is joined, whatever the codec.
- `segmented` — one target format; each clip is copied if it already matches and
  converted only if it does not. With `force_all` this is the full re-encode,
  used as the last-resort fallback.

Failures fall back rather than stopping: a failed fast join retries as a
segmented merge, and a failed segmented merge retries converting everything.
Each fallback says so on screen as it happens.

## Deliberate differences from the PowerShell version

- **`Format-Fps` rounded 29.97 to "30"** (0.05 tolerance). That made the clip
  table contradict the plan line: two clips looked identical while the tool said
  one needed converting. The tolerance is now tight enough to keep NTSC rates
  distinct on screen.
- **Reordering is `shift+↑↓`**, replacing `M` → "move which number?" → "to which
  position?".
- **Live per-clip progress** from `ffmpeg -progress pipe:1`, instead of ffmpeg's
  raw `-stats` output.
- **A merge can be cancelled** with `esc`; ffmpeg is stopped and the
  part-finished file is removed.
- **Progress is terminal-aware.** Redirected output gets one line per clip
  rather than carriage-return litter.
- **The mouse works** for selecting clips and pressing buttons.
- **Refuses to write its output over one of its inputs.** The PowerShell would
  hand ffmpeg a file it was busy replacing, destroying the source and the result
  together.
- **The working folder carries the process id**, so two merges in one folder no
  longer overwrite each other's segments.

## Tests

```
cargo test
cargo test dump_screens -- --ignored --nocapture   # print the screens
```

38 tests. The planning rules (duration weighting, tie-breaks, NTSC rationals,
what blocks the fast path) are covered directly, and every screen and overlay is
rendered into a `TestBackend` and read back — including at terminal sizes down
to 1x1, which is what catches a layout that would panic on a resize.

The mouse tests are worth keeping honest: they find a label on the rendered
screen and click the middle of it, so if the hit-region arithmetic ever drifts
away from what the spans actually occupy, a button stops matching its label and
a test fails.
