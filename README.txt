===============================================================
 VMERGE  -  join video clips into one mp4
===============================================================

HOW TO USE
----------
 1. Double-click MERGE-VIDEOS.exe

    The vmerge screen opens, already listing any clips sitting
    in this folder.

 2. Drag your video files onto the window and press Enter.
    Dragging a whole folder in works too.

 3. Press S to start.

The result is written next to the clips as merged.mp4.

Shortcut: you can also drag clips straight onto MERGE-VIDEOS.exe.
They open in the list in the order you dropped them.


WHAT IT ACCEPTS
---------------
 mp4  mov  mkv  avi  m4v  webm  wmv  flv  mpg  mpeg  ts  m2ts  mts  3gp

Mixed formats in one go are fine. Output is always mp4
(H.264 video + AAC audio), the format that plays everywhere.


FIRST RUN
---------
vmerge needs ffmpeg. If it is not already on your PC, the first
run downloads it into a folder named "ffmpeg" here.

 - No administrator rights needed.
 - Nothing is installed system-wide: no registry, no PATH change.
 - To uninstall everything, delete this folder.

If your PC has no internet access, install ffmpeg manually:
 1. Go to https://www.gyan.dev/ffmpeg/builds/
 2. Download "ffmpeg-release-essentials.zip"
 3. Unzip it here so this path exists:
        ffmpeg\bin\ffmpeg.exe


FILES IN THIS FOLDER
--------------------
 MERGE-VIDEOS.exe    <- double-click this
 README.txt          <- this file
 ffmpeg\             <- created on first run
 merged.mp4          <- your result


FOR POWER USERS
---------------
The engine also runs without the screen, for scripting:

  MERGE-VIDEOS.exe --no-tui --folder D:\clips --output final.mp4

  --folder <path>          Merge every clip in this folder, filename order
  --file-list <path>       Text file with one clip path per line, in order
  --output <name>          Output name or full path
  --quality <level>        visually-lossless | high | medium | small
  --encoder <choice>       auto | cpu | nvenc | qsv | amf
  --force-reencode         Re-encode even if the clips already match
  --skip-ffmpeg-download   Fail instead of downloading ffmpeg
  --no-pause               Do not wait for a keypress at the end

In folder mode, an order.txt file next to the clips (one filename
per line, # for comments) sets the order.

Exit code is 0 on success and 1 on failure.
