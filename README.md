# vmerge

Joins video clips into one `.mp4` with a real terminal UI: arrow-key selection,
in-place reordering, and live progress bars.

Build:

```
cargo build --release
```

Developer checks:

```
cargo test
cargo lint
cargo +nightly miri-test
```

`cargo lint` is a repo alias for Clippy with warnings denied.

Miri needs the nightly toolchain
(`rustup toolchain install nightly --component miri`):

```
cargo +nightly miri-test
```

The Miri alias disables isolation because the ffmpeg and merge tests write real
files. Miri is not part of CI, and is unlikely to earn its place: the crate has exactly
one `unsafe` block — the `SetFileAttributesW` call in `src/proc.rs` — and it is
`#[cfg(windows)]`, so a Linux runner would have nothing to check. Miri cannot
evaluate FFI calls either way.

The binary is `target/release/vmerge.exe`. Copy it to the project root as
`MERGE-VIDEOS.exe` to get a friendly drag-and-drop target:

```
copy target\release\vmerge.exe MERGE-VIDEOS.exe
```

The built exe is deliberately not committed — it changes on every build, so
tracking it would grow the history with each commit. The vendored ffmpeg archive
is committed because it changes once or twice a year.

ffmpeg is found in `PATH`, in an `ffmpeg\bin` folder beside the executable, or
in `%LOCALAPPDATA%\video-merge`. If none of those has it, it is downloaded and
unpacked on first run — no admin rights, nothing installed system-wide. Both
steps report themselves, using the same eighth-block bar as the merge screen:

```
  Downloading ffmpeg from this project's mirror - this happens once
  ████████████████▊───────   58%   19.0 MB / 32.8 MB   14.3 MB/s   0:01 left
  Unpacking...
  ████████████████████████  100%   184.2 MB / 184.2 MB
```

The size and the estimate come from the server rather than a number baked into
the source: the figure inherited from the PowerShell claimed "about 40 MB" and
the zip is 106. A transfer that stops short is caught there rather than
surfacing later as a corrupt archive, and with output redirected each step
prints a line every 25% instead of a bar it cannot draw.

### Which build, and why

ffmpeg.org publishes no Windows binaries of its own; its download page points at
two third-party builders, [gyan.dev](https://www.gyan.dev/ffmpeg/builds/) and
[BtbN](https://github.com/BtbN/FFmpeg-Builds/releases). All four sources below
were measured from the same ~250 KB/s connection:

| Source | Size | Throughput | Wall clock |
| --- | --- | --- | --- |
| **this repo's mirror** | **32.8 MB** | **14.3 MB/s** | **2.3 s** |
| gyan.dev `.7z` | 32.8 MB | 211 KB/s | ~2.5 min |
| gyan.dev `.zip` | 106.1 MB | 268 KB/s | ~6.8 min |
| BtbN on GitHub | 170.3 MB | *repeatedly 0* | did not finish |

The mirror is not faster because of anything clever: it is the same 32.8 MB
archive served from GitHub's code hosts, which reach 9–14 MB/s from the
connection this was measured on, while gyan.dev manages 250 KB/s.

The archive lives in the repository tree and is fetched over
`raw.githubusercontent.com` rather than being attached to a release, because
GitHub's *release-asset* host proved unreliable on that connection: three
consecutive attempts returned nothing at all, and a later attempt succeeded
completely. Intermittent is worse than slow for a first-run bootstrap — setup
that fails half the time is setup that cannot be trusted — whereas the code
hosts were fast on every attempt. It is likely the same reason the 170 MB BtbN
download never completed.

The mirror is pinned by SHA-256, so a corrupted or swapped file is rejected
before anything is unpacked and setup falls through to upstream. Upstream is not
hash-checked, because its contents change with every ffmpeg release. `.7z` is
preferred over `.zip` because it is the same build in a third of the bytes.

Sources are tried in order, and one only counts as good once its archive has
*unpacked* — so a 7z that will not decode falls back to the zip rather than
failing setup. Upstream stays in the list so the tool keeps working if this
repository is ever renamed, made private, or unreachable.

Only `ffmpeg.exe` and `ffprobe.exe` are written. `ffplay.exe` is another 104 MB
and nothing here ever invokes it, and the rest of the archive is documentation
and presets — so 196 MB lands on disk out of 307 MB unpacked. A solid 7z block
still has to be *decompressed* to reach later entries, but it does not have to be
*written*, and skipping those writes took a clean first run from 29 s to 19 s.

End to end, a first run is now about **19 seconds**: 2.3 s to fetch 32.8 MB, the
rest to unpack it.

`FFmpeg/FFmpeg` on GitHub is source only: no releases, no binary assets. Using
it would mean compiling ffmpeg and libx264 on the user's machine.

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
| `?` | the full key reference |
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

## The design

Four rules, taken from how the TUIs that read well actually work:

**Hierarchy survives monochrome.** Strip every colour and the screen still
reads. The cursor is a solid bar `▌`, a mark is a bullet `●` — two different
shapes, not two different colours — and the primary action is the only filled
chip on screen. `hierarchy_survives_monochrome` renders with the palette
stripped and asserts all of that, rather than trusting it.

**Layers, not boxes.** Panels are a slightly lighter background between hairline
rules. The first version was boxes inside boxes: every nested border spent two
rows and two columns to say nothing, and the rounded corners were the loudest
thing on screen.

**Every cell counts.** Sizes, framerates and lengths are right-aligned in
columns sized to their content, so they can be compared down the screen.
Codec and audio share one column (`h264·aac`, `h264·—` when silent). The whole
page is capped at 112 columns: on a 200-column terminal a full-width table
pushes the name and the numbers so far apart they stop reading as one row.

**One primary action, and hints in tiers.** `S START MERGE` is filled; the
things you can click are a shade back; the things you can only press (`↑↓`,
`⇧↑↓`) are a shade further back again. The full list lives behind `?` so the bar
does not have to be a wall of twelve identical chips.

Colour lives in `theme.rs` as semantic slots — `surface`, `accent`, `muted`,
`good` — never as hex values at the call site. There are two palettes: 24-bit,
and a sixteen-colour fallback that follows the terminal's own theme. Windows
consoles have done 24-bit since Windows 10 1703, so that is the default;
`VMERGE_COLORS=16` forces the plain one.

Progress bars use eighth-width blocks (`█████▊───`), so a twenty-cell bar
resolves 160 steps. Without the partial cell a slow clip looks stalled for
seconds at a time.

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
- **Redesigned throughout** — see the design section above.
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

49 tests. The planning rules (duration weighting, tie-breaks, NTSC rationals,
what blocks the fast path) are covered directly, and every screen and overlay is
rendered into a `TestBackend` and read back — including at terminal sizes down
to 1x1, which is what catches a layout that would panic on a resize.

The mouse tests are worth keeping honest: they find a label on the rendered
screen and click the middle of it, so if the hit-region arithmetic ever drifts
away from what the spans actually occupy, a button stops matching its label and
a test fails.
