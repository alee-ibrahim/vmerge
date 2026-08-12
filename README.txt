===============================================================
 VIDEO MERGER  -  join video clips into one mp4
===============================================================

HOW TO USE
----------
 1. Double-click  MERGE-VIDEOS.bat

    The Video Merger screen opens, already listing any clips
    sitting in this folder.

 2. Drag your video files onto the window and press Enter.
    (dragging a whole folder in works too)

 3. Press S to start.

The result is written next to the clips as merged.mp4

Shortcut: you can also drag clips straight onto MERGE-VIDEOS.bat.
They open in the list in the order you dropped them.


THE SCREEN
----------
   CLIPS TO MERGE  (3)
   -----------------------------------------------------------------
     1  intro.mov              1280x720   25fps  h264  aac    00:00:12
     2  main take.mp4         1920x1080   30fps  h264  aac    00:04:31
     3  outro.mkv             1920x1080   30fps  h264  silent 00:00:08
   -----------------------------------------------------------------
   Total: 3 clips   00:04:51   512 MB
   Plan : re-encode to a common format

   Output : merged.mp4
   Quality: high      Encoder: auto

    [S] START MERGE     [R] Remove    [M] Move     [A] Sort by name
    [F] Add a folder    [C] Clear     [O] Output   [Q] Quality
    [T] Target size     [E] Encoder   [X] Exit

 - Type the letter, or the whole word ("start", "remove", "exit").
 - [R] Remove accepts  3  or  2,5  or  2-4
 - [M] Move asks which clip, then which position
 - [A] Sorts by filename, counting numbers properly, so clip2
       comes before clip10
 - Anything that looks like a path is treated as files to add, so
   you can keep dragging more in at any time.
 - The "Plan:" line tells you in advance what will happen, and how
   many clips have to be converted. Read it before pressing S - it
   is the difference between seconds and minutes.
 - [T] Target size changes what everything is converted to, which
   is the main control over how long a merge takes. See below.

After a merge the list stays put, so you can adjust it and merge
again. The output name auto-advances (merged_2.mp4) so nothing is
overwritten by accident.


WHAT IT ACCEPTS
---------------
 mp4  mov  mkv  avi  m4v  webm  wmv  flv  mpg  mpeg  ts  m2ts  mts  3gp

Mixed formats in one go are fine - phone videos, screen recordings
and camera files can all go in together.

Output is always mp4 (H.264 video + AAC audio), the format that
plays everywhere.


HOW LONG DOES IT TAKE?
----------------------
Only the clips that have to change are re-encoded. Everything else
is copied across untouched - bit for bit identical, no quality
loss, as fast as your disk.

So the cost of a merge is the cost of converting the odd ones out.
Join fifty clips from the same camera and nothing is re-encoded at
all. Join two hours of camera footage to one phone clip and only
the phone clip is converted; the two hours are copied.

The target format is picked by how much footage is in each format,
not by how many files. The longest material wins, so the bulk of
your video is the part that gets copied rather than re-encoded.

  Example: a 6-second portrait phone clip plus 2m13s of landscape
  camera footage. Converting everything to the phone's 1080x2400
  at 60 fps means five times the necessary encoding work and 75%
  of every frame black. Matching the camera footage instead means
  only the 6-second clip is touched: 2 seconds of work, and the
  camera footage stays exactly as it was.

Press T to override the target if you want something else - a
smaller frame or lower framerate is dramatically faster. The
"Plan:" line always shows what will happen before you commit.

Clips of a different shape are fitted inside the frame with black
bars rather than cropped, so nothing gets cut off. Clips with no
sound get a silent audio track so nothing drifts out of sync.


FIRST RUN
---------
The tool needs ffmpeg (free, open source video engine). If it is
not already on your PC, the first run downloads it (about 40 MB)
into a folder named "ffmpeg" right here. No questions asked.

 - No administrator rights needed.
 - Nothing is installed system-wide: no registry, no PATH change.
 - If this folder is read-only, it installs to your own AppData
   folder instead.
 - To uninstall everything, delete this folder.

If your PC has no internet access, do it manually instead:
 1. Go to  https://www.gyan.dev/ffmpeg/builds/
 2. Download "ffmpeg-release-essentials.zip"
 3. Unzip it here so this path exists:
        ffmpeg\bin\ffmpeg.exe


FILES IN THIS FOLDER
--------------------
 MERGE-VIDEOS.bat    <- double-click this
 merge-videos.ps1    <- the engine; must stay next to the .bat
 README.txt          <- this file
 ffmpeg\             <- created on first run
 merged.mp4          <- your result


FOR POWER USERS
---------------
The engine also runs without the screen, for scripting:

  powershell -ExecutionPolicy Bypass -File merge-videos.ps1 -Folder "D:\clips" -Output final.mp4

  -Tui                  Open the interactive screen (what the .bat does)
  -Folder <path>        Merge every clip in this folder, filename order
  -Files a.mp4,b.mov    Explicit list, in this exact order
  -FileList <path>      Text file with one clip path per line, in order
  -Output <name>        Output name or full path (default merged.mp4)
  -Quality <level>      visually-lossless | high (default) | medium | small
  -Encoder <choice>     auto (default) | cpu | nvenc | qsv | amf
  -ForceReencode        Re-encode even if the clips already match
  -SkipFfmpegDownload   Fail instead of downloading ffmpeg
  -NoPause              Don't wait for a keypress at the end

In -Folder mode, an  order.txt  file next to the clips (one
filename per line, # for comments) sets the order.

Exit code is 0 on success and 1 on failure. The closing pause is
skipped automatically when input is redirected, so it is safe to
call from another script.


TROUBLESHOOTING
---------------
The window opens and closes immediately
    It should now always stay open long enough to show a reason.
    If it still closes, open a Command Prompt in this folder and
    run  MERGE-VIDEOS.bat  from there so the message stays put.

"running scripts is disabled on this system"
    The .bat already works around this. It only happens if you
    double-click merge-videos.ps1 directly - use the .bat.

Output is longer/shorter than expected
    The tool warns you if the total doesn't add up. Usually one
    clip was unreadable - look for skipped clips in the output.

One clip was skipped
    That file is corrupt or is not really a video. Try playing it
    in VLC to confirm.

The merge is slower than you expected
    Press E to switch between the GPU and CPU encoder, or lower
    the quality with Q. A same-format join never re-encodes at
    all, so it is always near-instant.
