# vmerge

Joins video clips into one `.mp4` with a real terminal UI: arrow-key selection,
in-place reordering, and live progress bars. It will also
[fetch a video from a link](#downloading-a-video) if you have not got the clip
yet.

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
| `u` | **download a video** from a link |
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
| `esc` | clear marks, close a dialog, or cancel a running merge or download |
| `x` | exit |

### Mouse

Clicking works as well as typing: click a clip to select it, click any button in
the bottom bar to run it, click a menu item to choose it, click an answer on a
dialog, click the drop zone to add clips, click a prompt's `ADD THESE` to accept
what you dropped in. Whatever is under the pointer lights up first, so you can
see what a click will hit before making it. A picker or the key sheet also closes
by clicking off it, and the scroll wheel moves through the list and through menus.

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

### Downloading a video

`u` takes a link instead of a file. It asks how much of the video you want, then
fetches that one video into the current folder and stops — it is a job of its
own, not a way of adding clips, so nothing is merged and the clip list is left
exactly as it was. `--download <URL>` is the same job without the screen.

```
MERGE-VIDEOS.exe --download "https://www.youtube.com/watch?v=..." --download-quality 720p
```

| Choice | Gets |
| --- | --- |
| `best` | whatever the site has, at the highest resolution |
| `1080p` `720p` `480p` | a hard cap, not a preference — 720p means 720p |
| `audio` | an `.m4a`, no picture |

Anything but `audio` arrives as an `.mp4`, preferring H.264 and AAC within the
resolution you asked for — which is exactly what the merge side then joins
without re-encoding anything. Resolution is still the first thing sorted on, or
a 360p H.264 stream would beat a 1080p VP9 one and the cap would mean nothing.

The work is done by [yt-dlp](https://github.com/yt-dlp/yt-dlp), which covers
YouTube and around 1800 other sites and uses the ffmpeg this program already
installed. Two things about it are worth knowing:

- **It is fetched only when you first use a link.** Almost every run of this
  program joins clips that are already on disk, so an 18 MB download during
  first-time setup would be charged to people who will never need it. When it
  does happen it draws the same bar ffmpeg setup does, and the executable is
  checked against the `SHA2-256SUMS` published beside it — matched by name, so
  the ARM build's digest can never be checked against the x86 build's bytes.
- **It goes stale, so it keeps itself current.** An ffmpeg from two years ago
  still joins clips; a yt-dlp from two years ago fails, because the sites it
  reads change underneath it and upstream ships a fix most weeks. A copy this
  program installed updates itself before use if it has not been checked in a
  fortnight. A copy already on your `PATH` is normally left alone — it is yours —
  but yt-dlp names its versions after their release date, so one more than three
  months old is passed over in favour of an installed copy, and it says why:

```
  The yt-dlp on your PATH was released 250 days ago, which is old enough
  to be refused by most video sites. Leaving it alone and installing
  a copy this program can keep up to date instead.
```

  Without that, an eight-month-old package manager install produces
  `HTTP Error 403: Forbidden` and an error about streaming protocols that nobody
  could reasonably connect back to it.

**A broadcast that is still on air is taken from its beginning.** Every link is
asked what it is before a byte of it is fetched, because a live sitting has no
last byte to wait for and finding that out afterwards is finding out too late.
A live one is then fetched with `--live-from-start`, and that one flag settles
three separate things.

It is what makes a sitting already an hour old arrive as an hour of video rather
than the tail end of one — and that hour is not recoverable later, because it is
gone the moment the broadcast ends. It puts the download back on yt-dlp's own
fragment downloader, which reports progress properly: the silence that made this
look like a hang was never a live stream being unreportable, only ffmpeg being
handed the job and saying nothing about it. And because the work is counted in
fragments, and the site says how many there are so far, a live download has a
real position to show:

```
  RECORDING  LIVE                                    17% of the broadcast so far
  ────────────────────────────────────────────────────────────────────────────
  captured 62.7 MB   at 4.1 MB/s   elapsed 1:18   ends when you stop it

  esc  finish the recording
```

That percentage is measured, not estimated, but it is a percentage of something
that keeps growing — so it creeps rather than fills, and it is honest about never
quite arriving while the sitting is still going. It says what it is a fraction
of, because `17%` on its own would read as "barely started" when it means
"seventeen minutes of the hundred broadcast so far". A live broadcast declares no
size, so there is no byte total to draw from and no estimate worth printing;
until the first counts arrive there is nothing honest to draw at all, and a
travelling marker says "working" where a bar would have to invent a position.

`esc` means *finish* here rather than *cancel*. yt-dlp writes each stream to its
own part-file and only joins them at the end, so stopping half way leaves the
picture in one file, the sound in another, and no video at all — both complete as
far as they go. They are joined without re-encoding a frame, and what comes out
is every minute that had arrived when the key was pressed. Measured on a sitting
already two and a half hours old: 42 minutes of it on disk after 78 seconds.

A broadcast that has not started yet is refused with an explanation. One that has
already finished is an ordinary video and downloads the ordinary way.

**YouTube needs a JavaScript engine.** It hands out video links behind a script
puzzle, and yt-dlp has to run an engine to answer it. yt-dlp enables
[Deno](https://deno.com) and nothing else by default, so a machine with Node.js
installed — which is most of them — counts as having no engine at all. This looks
for `deno`, `node` and `bun` on `PATH` and names whatever it finds, which costs
one flag and is the difference between a download working and this:

```
  WARNING: [youtube] No supported JavaScript runtime could be found...
  ERROR: unable to download video data: HTTP Error 403: Forbidden
```

If you have neither engine, install [Node.js](https://nodejs.org) or Deno and
this will find it on its own.

**`403 Forbidden` also happens at random.** It is the most misleading thing
yt-dlp says: it reads like a blocked network or a private video, and it is
usually neither. Measured on one link, three identical runs gave two immediate
successes and one refusal — same video, same quality, same formats. yt-dlp's own
`--retries` re-requests the *same* URL, which is no use when that URL is the
thing being refused, so the whole extraction is run again to get fresh ones, up
to three times. Both causes get a sentence added to the error saying which one
it was, so a genuinely missing engine is never mistaken for bad luck.

Where the finished file went is read from a file, not from yt-dlp's console
output. yt-dlp re-encodes what it prints into something the console can render,
and on Windows that quietly drops what will not fit: a video titled
`Chill Mix 🎧 Vol 85` is printed back without the emoji, so the path names a file
that does not exist and a download that worked perfectly is reported as lost.
`--print-to-file` writes UTF-8 and changes nothing. If the name it records leads
nowhere anyway, the newest file in the folder is used rather than giving up.

Partial files go to a hidden working folder and only the finished video is moved
out, so stopping a download leaves nothing half-written behind — the same reason
merges work in `_merge_temp` rather than in place. Best video and best audio
usually arrive as two separate files, so the bar runs to 100% twice; the screen
says which stream is arriving rather than letting that read as lost progress.

**Stopping stops the whole tree.** yt-dlp runs an ffmpeg of its own to join the
video and audio streams, and Windows' `TerminateProcess` ends exactly one
process. Stopping a download mid-join therefore used to leave that ffmpeg
running, still writing and still holding the working folder open, so clearing
that folder failed and a download the user had stopped carried on to the end.
Children are put in a job object instead, which is how Windows expresses "these
belong together", and the group ends in one call. It is set to die with this
program too, so a crash does not orphan anything either.

From the command line, `ctrl-c` asks the job to stop rather than killing the
program where it stands, so a recording started with `--download` still gets its
file written out. Press it twice and it is taken as meant: the default handler
runs, and the job object takes ffmpeg down rather than leaving it behind.

**The folder is not read on the way in.** Files dropped onto the program start
the list and nothing else does. Preloading whatever videos were already sitting
in the folder sounds helpful — "the clips are in this folder" is the common case
— but what it actually produces is a list nobody asked for, of unrelated videos
or yesterday's output, whose first use is to be cleared. It also made the opening
move a scan of every video in the folder, which is a wait before the program is
usable, for a list that was wrong anyway.

Playlists are not supported: a link that carries one alongside a video gets the
video, and a bare playlist link gets its first entry.

### Updating itself

Every start checks whether a newer release exists, and installs it if one does.
The people this is for double-click an exe someone sent them; they do not watch a
repository, so a fix has to come to them. When there is nothing to install — the
usual case — the check costs about 280 ms and says nothing at all.

When there is:

```
  Version 0.1.2 is out — this is 0.1.1. Updating.
  ████████████████████████  100%   4.1 MB / 4.1 MB   9.8 MB/s
  Updated to 0.1.2. Starting it now.
```

The new version then runs with the arguments you gave, and its exit code is
passed straight through, so a script cannot tell the difference.

What makes that safe enough to do without asking:

- **Only a release of this repository, over TLS.** The download URL the API hands
  back has to belong to it, which is what keeps a look-alike host out.
- **Forwards only.** The tag has to parse *and* be strictly greater than what is
  running. A tag this build cannot read is refused rather than guessed at, so a
  malformed or renamed release can never install an older binary over a newer one.
- **Checked before it is swapped in**: the length GitHub declares for the asset,
  a Windows executable header, and the SHA-256 when the release publishes one as
  `MERGE-VIDEOS.exe.sha256`. Publishing that file is worth the half-second — it
  is the only check here that would survive someone with write access to a
  release but not to the tag. If it is published and does not match, or cannot be
  read, the update stops.
- **Failing changes nothing.** No network, a rate-limited API, a stalled host, a
  folder that cannot be written: all of them leave the running exe exactly as it
  was, and the merge you actually came to do goes ahead. An update that does not
  happen is never the reason something else did not happen.

Turn it off with `--no-update`, or by setting `VMERGE_NO_UPDATE` to anything.
Debug builds never update — replacing the build you are testing with a released
one would be the opposite of helpful.

A running exe cannot be overwritten on Windows, but it *can* be renamed, which
frees its name for the new file. So the old one is moved to
`MERGE-VIDEOS.exe.previous`, hidden, and deleted at the next start — nothing can
delete it while it is still the process doing the deleting. That cleanup happens
even with `--no-update`.

Where the bytes come from is not a detail. `github.com/…/releases/download/…`
redirects to `objects.githubusercontent.com`, which on the connection this was
measured on refuses every attempt in about 300 ms — five for five with curl, and
the same from inside the program. It is the same unreliability that made this
project mirror ffmpeg in its own tree rather than attach it to a release. The
GitHub *API* asset route serves the identical bytes through
`release-assets.githubusercontent.com` and worked first time, every time, so that
is tried first and the browser URL is the fallback.

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
| `--download <url>` | download that one video and stop; nothing is merged |
| `--download-quality` | `best` \| `1080p` \| `720p` \| `480p` \| `audio` |
| `--skip-ffmpeg-download` | fail instead of downloading ffmpeg |
| `--skip-ytdlp-download` | fail instead of downloading yt-dlp |
| `--no-update` | do not look for a newer version on start |
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

**One primary action, and a button that looks like one.** Everything clickable
is a chip — `▐ q quality ▌` — padded, capped at both ends with half-blocks, lit
a shade brighter under the pointer, and clickable across all of it, caps
included. Exactly one chip per screen is filled: `S START MERGE`, or
`A ADD CLIPS` in the drop zone while the list is empty. Things you can only
press (`↑↓`, `⇧↑↓`) stay plain text, because a button nobody can click is worse
than no button. Things with nothing to act on — `START MERGE` with an empty
list — keep their words and their place but lose the face, so the answer arrives
before the click rather than after it. The bar sits three rows deep with a blank
row above it: buttons need the room their padding takes, and a toolbar pressed
against the text above reads as one more line of that text. The full key list
lives behind `?`.

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

65 tests. The planning rules (duration weighting, tie-breaks, NTSC rationals,
what blocks the fast path) are covered directly, and every screen and overlay is
rendered into a `TestBackend` and read back — including at terminal sizes down
to 1x1, which is what catches a layout that would panic on a resize.

The self-updater is tested where the decisions are, not where the network is:
which releases are acceptable, which download URLs belong to this repository,
that `0.1.10` is newer than `0.1.9`, and that the sweep removes a previous image
and a part-finished download while leaving everything else in the folder alone.
The parts that cannot be unit-tested — swapping a running exe and handing over to
it — were exercised for real by building a binary labelled `0.1.0`, letting it
update itself from the live release, and checking that the arguments, the exit
code and `--version` all came out right on the other side.

The mouse tests are worth keeping honest: they find a label on the rendered
screen and click the middle of it, so if the hit-region arithmetic ever drifts
away from what the spans actually occupy, a button stops matching its label and
a test fails. One goes further and clicks a button's outermost cells, because a
single cell of disagreement between a chip's drawn width and its hit region
shifts every button after it along the row — drift the middle of a label is wide
enough to hide.
