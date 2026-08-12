<#
    merge-videos.ps1  --  the engine behind the two .bat launchers

    Joins video clips into a single .mp4 file.

    Input  : mp4, mov, mkv, avi, m4v, webm, wmv, flv, mpg, mpeg, ts, m2ts, mts, 3gp
    Output : one .mp4 (H.264 + AAC, faststart)

    Double-clicking MERGE-VIDEOS.bat opens the interactive screen (-Tui):
    drag clips onto the window, reorder them, press S to start. The other
    parameters below drive the same engine without the screen.

    Clips that already share identical codecs/size/framerate are joined
    without re-encoding (seconds, zero quality loss). Anything mixed is
    normalised to a common format first, then joined.
#>

[CmdletBinding()]
param(
    # Interactive terminal UI: drag files in, reorder, start when ready.
    [switch]   $Tui,

    # Folder containing the clips. Defaults to the folder this script lives in.
    [string]   $Folder,

    # Explicit list of files, in the exact order you want them joined.
    [string[]] $Files,

    # Text file holding one path per line, in order. This is how the .bat hands
    # over drag-and-dropped files: a list file survives filenames containing
    # quotes, apostrophes, ampersands and commas, which a command line does not.
    [string]   $FileList,

    # Output file name (or full path). Default: merged.mp4 next to the clips.
    [string]   $Output,

    # visually-lossless | high | medium | small
    [ValidateSet('visually-lossless', 'high', 'medium', 'small')]
    [string]   $Quality = 'high',

    # auto = use the GPU if one works, cpu = always libx264.
    [ValidateSet('auto', 'cpu', 'nvenc', 'qsv', 'amf')]
    [string]   $Encoder = 'auto',

    # Force a full re-encode even when the clips look identical.
    [switch]   $ForceReencode,

    # Never download ffmpeg; fail instead if it is missing.
    [switch]   $SkipFfmpegDownload,

    # Don't wait for a keypress at the end (for scripting).
    [switch]   $NoPause
)

$ErrorActionPreference = 'Stop'
$script:ExitCode = 0

$VIDEO_EXTENSIONS = @(
    '.mp4', '.mov', '.mkv', '.avi', '.m4v', '.webm', '.wmv', '.flv',
    '.mpg', '.mpeg', '.ts', '.m2ts', '.mts', '.3gp', '.3g2', '.ogv', '.asf'
)

$TEMP_DIR_NAME  = '_merge_temp'
$QUALITY_LEVELS = @('visually-lossless', 'high', 'medium', 'small')

# Every intermediate segment is written with this, and it is the reason joining
# works at all.
#
# Each mp4 keeps its own timebase, and a stream copy between two different
# timebases does not rescale: joining a 1/15360 clip to a 1/90000 one yields a
# file whose video races to the end while the audio plays on - 10 seconds of
# audio against 58 seconds of video, from two clips that looked identical.
# Forcing one timescale on every segment removes the mismatch.
#
# 90000 is the standard choice: it divides exactly by 24, 25, 30, 50 and 60, and
# by the awkward rates too (29.97 fps lands on exactly 3003 ticks per frame).
#
# MPEG-TS would also normalise the clock, but the TS muxer starts each segment
# 1.4 s into its own timeline, and joining those leaves two frames sharing one
# timestamp at every boundary - measured, on every combination of muxdelay,
# genpts and igndts flags tried.
$SEGMENT_ARGS = @('-video_track_timescale', '90000')

# ---------------------------------------------------------------------------
# Console helpers
# ---------------------------------------------------------------------------

function Write-Head($text) { Write-Host ''; Write-Host "  $text" -ForegroundColor Cyan }
function Write-Info($text) { Write-Host "  $text" -ForegroundColor Gray }
function Write-Good($text) { Write-Host "  $text" -ForegroundColor Green }
function Write-Warn($text) { Write-Host "  $text" -ForegroundColor Yellow }
function Write-Bad($text)  { Write-Host "  $text" -ForegroundColor Red }
function Write-Rule        { Write-Host ('  ' + ('-' * 84)) -ForegroundColor DarkGray }

function Write-Banner {
    Write-Host ''
    Write-Host '  ===========================================' -ForegroundColor DarkCyan
    Write-Host '            V I D E O   M E R G E R'          -ForegroundColor White
    Write-Host '  ===========================================' -ForegroundColor DarkCyan
}

function Stop-Here($message) {
    Write-Host ''
    Write-Bad "STOPPED: $message"
    $script:ExitCode = 1
    Complete-Run
}

function Test-Interactive {
    # ReadKey reads the console input buffer, not stdin, so with stdin
    # redirected it would wait for a keypress that is never coming. Anything
    # driving this tool from a script must not hang on the closing pause.
    try { return (-not [Console]::IsInputRedirected) } catch { return $false }
}

function Complete-Run {
    Write-Host ''
    if (-not $NoPause -and (Test-Interactive)) {
        Write-Host '  Press any key to close this window...' -ForegroundColor DarkGray
        try { [void]$Host.UI.RawUI.ReadKey('NoEcho,IncludeKeyDown') } catch { Start-Sleep -Seconds 5 }
    }
    exit $script:ExitCode
}

function Format-Size([double]$bytes) {
    if ($bytes -ge 1GB) { return ('{0:N2} GB' -f ($bytes / 1GB)) }
    if ($bytes -ge 1MB) { return ('{0:N1} MB' -f ($bytes / 1MB)) }
    return ('{0:N0} KB' -f ($bytes / 1KB))
}

function Format-Duration([double]$seconds) {
    if ($seconds -le 0) { return 'unknown' }
    $ts = [TimeSpan]::FromSeconds($seconds)
    return ('{0:00}:{1:00}:{2:00}' -f [int]$ts.TotalHours, $ts.Minutes, $ts.Seconds)
}

function Format-Fps([double]$fps) {
    if ([math]::Abs($fps - [math]::Round($fps)) -lt 0.05) { return [string][int][math]::Round($fps) }
    return ('{0:N2}' -f $fps)
}

function Limit-Text([string]$text, [int]$width) {
    if ($text.Length -le $width) { return $text.PadRight($width) }
    return ($text.Substring(0, $width - 3) + '...')
}

# Runs ffmpeg/ffprobe and returns their stdout.
#
# Why this wrapper exists: PowerShell 5.1 turns a native program's stderr into
# error records whenever the console output is redirected (someone runs the .bat
# as `MERGE-VIDEOS.bat > log.txt 2>&1`, or a wrapper captures it). ffmpeg writes
# its progress to stderr, so with $ErrorActionPreference = 'Stop' a completely
# successful merge would throw and get thrown away. Exit codes are the truth.
function Invoke-Native($exe, [string[]]$argList) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try   { & $exe @argList }
    finally { $ErrorActionPreference = $prev }
}

# Sorts clip2 before clip10 (plain alphabetical would not).
function Get-NaturalKey([string]$text) {
    $sb = New-Object System.Text.StringBuilder
    foreach ($m in [regex]::Matches($text, '\d+|\D+')) {
        if ($m.Value -match '^\d+$') { [void]$sb.Append($m.Value.PadLeft(20, '0')) }
        else                        { [void]$sb.Append($m.Value.ToLowerInvariant()) }
    }
    return $sb.ToString()
}

# ---------------------------------------------------------------------------
# ffmpeg discovery / installation  (per-user, never needs admin)
# ---------------------------------------------------------------------------

# Test-Path throws on illegal characters instead of just saying "no", which is
# never what a caller here wants.
function Test-FolderPath($path) {
    if (-not $path) { return $false }
    try { return (Test-Path -LiteralPath $path -PathType Container) } catch { return $false }
}

function Test-FilePath($path) {
    if (-not $path) { return $false }
    try { return (Test-Path -LiteralPath $path -PathType Leaf) } catch { return $false }
}

function Test-AnyPath($path) {
    if (-not $path) { return $false }
    try { return (Test-Path -LiteralPath $path) } catch { return $false }
}

function Test-WritableDir($path) {
    try {
        if (-not (Test-Path -LiteralPath $path -PathType Container)) { return $false }
        $probe = Join-Path $path ('.write-test-' + [guid]::NewGuid().ToString('N'))
        [IO.File]::WriteAllText($probe, 'x')
        Remove-Item -LiteralPath $probe -Force
        return $true
    }
    catch { return $false }
}

function Find-LocalFfmpeg($root) {
    foreach ($r in @($root, (Join-Path $env:LOCALAPPDATA 'video-merge'))) {
        if (-not $r) { continue }
        $candidates = @(
            (Join-Path $r 'ffmpeg\bin\ffmpeg.exe'),
            (Join-Path $r 'ffmpeg\ffmpeg.exe'),
            (Join-Path $r 'ffmpeg.exe')
        )
        foreach ($c in $candidates) { if (Test-Path -LiteralPath $c) { return $c } }

        $nested = Join-Path $r 'ffmpeg'
        if (Test-Path -LiteralPath $nested) {
            $hit = Get-ChildItem -LiteralPath $nested -Filter 'ffmpeg.exe' -Recurse -File -ErrorAction SilentlyContinue |
                   Select-Object -First 1
            if ($hit) { return $hit.FullName }
        }
    }
    return $null
}

function Install-Ffmpeg($root) {
    $sources = @(
        @{ Name = 'gyan.dev';    Url = 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip' },
        @{ Name = 'BtbN/GitHub'; Url = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip' }
    )

    $zip    = Join-Path $env:TEMP ('ffmpeg-'   + [guid]::NewGuid().ToString('N') + '.zip')
    $stage  = Join-Path $env:TEMP ('ffmpeg-x-' + [guid]::NewGuid().ToString('N'))
    $target = Join-Path $root 'ffmpeg'

    # Everything here runs as the current user. If the script itself sits
    # somewhere unwritable (read-only share, Program Files), fall back to the
    # per-user AppData folder instead of asking for elevation.
    if (-not (Test-WritableDir $root)) {
        $target = Join-Path $env:LOCALAPPDATA 'video-merge\ffmpeg'
        Write-Info ("Script folder is read-only, installing to {0} instead." -f $target)
    }

    try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch { }

    $downloaded = $false
    foreach ($src in $sources) {
        try {
            Write-Info ("Downloading ffmpeg from {0} (about 40 MB, one time only)..." -f $src.Name)
            # WebClient streams straight to disk. Invoke-WebRequest on
            # PowerShell 5.1 buffers the whole response in memory and is
            # several times slower for a file this size.
            $wc = New-Object System.Net.WebClient
            $wc.Headers.Add('User-Agent', 'video-merge-setup')
            try { $wc.DownloadFile($src.Url, $zip) } finally { $wc.Dispose() }
            $downloaded = $true
            break
        }
        catch {
            Write-Warn ("Could not download from {0}: {1}" -f $src.Name, $_.Exception.Message)
        }
    }
    if (-not $downloaded) { return $null }

    try {
        Write-Info 'Extracting...'
        if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        [System.IO.Compression.ZipFile]::ExtractToDirectory($zip, $stage)

        $exe = Get-ChildItem -LiteralPath $stage -Filter 'ffmpeg.exe' -Recurse -File | Select-Object -First 1
        if (-not $exe) { Write-Warn 'The downloaded archive did not contain ffmpeg.exe.'; return $null }

        $binDir    = $exe.Directory.FullName
        $binTarget = Join-Path $target 'bin'
        New-Item -ItemType Directory -Path $binTarget -Force | Out-Null

        foreach ($name in @('ffmpeg.exe', 'ffprobe.exe', 'ffplay.exe')) {
            $srcFile = Join-Path $binDir $name
            if (Test-Path -LiteralPath $srcFile) {
                $dest = Join-Path $binTarget $name
                Copy-Item -LiteralPath $srcFile -Destination $dest -Force
                # Clear the "downloaded from the internet" flag so Windows does
                # not raise a SmartScreen prompt on first run.
                try { Unblock-File -LiteralPath $dest -ErrorAction SilentlyContinue } catch { }
            }
        }

        $installed = Join-Path $binTarget 'ffmpeg.exe'
        if (Test-Path -LiteralPath $installed) {
            Write-Good ("ffmpeg ready: {0}" -f $binTarget)
            return $installed
        }
        return $null
    }
    catch {
        Write-Warn ("Extraction failed: {0}" -f $_.Exception.Message)
        return $null
    }
    finally {
        if (Test-Path -LiteralPath $zip)   { Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue }
        if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }
    }
}

function Resolve-Tools($root) {
    $ffmpeg = $null

    $inPath = Get-Command 'ffmpeg.exe' -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($inPath) { $ffmpeg = $inPath.Source }
    if (-not $ffmpeg) { $ffmpeg = Find-LocalFfmpeg $root }

    if (-not $ffmpeg) {
        Write-Head 'First-time setup'
        Write-Info 'ffmpeg (the free video engine this tool needs) is not installed yet.'
        Write-Info 'Setting it up automatically - no admin rights, nothing installed'
        Write-Info 'system-wide. It just lands in an "ffmpeg" folder next to this script.'
        Write-Host ''

        if ($SkipFfmpegDownload) { Stop-Here 'ffmpeg is missing and -SkipFfmpegDownload was set.' }

        $ffmpeg = Install-Ffmpeg $root
        if (-not $ffmpeg) {
            Write-Host ''
            Write-Info 'Automatic setup did not work. Manual option:'
            Write-Info '  1. Open https://www.gyan.dev/ffmpeg/builds/'
            Write-Info '  2. Download "ffmpeg-release-essentials.zip"'
            Write-Info ("  3. Unzip it so that {0}\ffmpeg\bin\ffmpeg.exe exists" -f $root)
            Stop-Here 'Could not set up ffmpeg automatically.'
        }
    }

    $ffprobe = Join-Path (Split-Path -Parent $ffmpeg) 'ffprobe.exe'
    if (-not (Test-Path -LiteralPath $ffprobe)) {
        $probeInPath = Get-Command 'ffprobe.exe' -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($probeInPath) { $ffprobe = $probeInPath.Source }
        else { Stop-Here "Found ffmpeg but not ffprobe next to it ($ffmpeg)." }
    }

    return @{ FFmpeg = $ffmpeg; FFprobe = $ffprobe }
}

# ---------------------------------------------------------------------------
# Probing
# ---------------------------------------------------------------------------

function Get-ClipInfo($ffprobe, $path) {
    $json = Invoke-Native $ffprobe @('-v', 'quiet', '-print_format', 'json',
                                     '-show_format', '-show_streams', '--', "$path")
    if ($LASTEXITCODE -ne 0 -or -not $json) { return $null }

    try { $data = ($json -join '') | ConvertFrom-Json } catch { return $null }
    if (-not $data.streams) { return $null }

    $video = $data.streams | Where-Object { $_.codec_type -eq 'video' } | Select-Object -First 1
    $audio = $data.streams | Where-Object { $_.codec_type -eq 'audio' } | Select-Object -First 1
    if (-not $video) { return $null }

    $fps = 0.0
    if ($video.r_frame_rate -and $video.r_frame_rate -match '^(\d+)/(\d+)$') {
        $den = [double]$Matches[2]
        if ($den -ne 0) { $fps = [double]$Matches[1] / $den }
    }

    # r_frame_rate is the highest rate the stream could carry, not the rate it
    # runs at. A previously merged file often reports something silly like 375.
    # Trusting it would pick an absurd target framerate and multiply the work,
    # so the average rate is what gets used for decisions and display; the raw
    # value is kept only for the strict "are these identical" check.
    $avgFps = 0.0
    if ($video.avg_frame_rate -and $video.avg_frame_rate -match '^(\d+)/(\d+)$') {
        $den = [double]$Matches[2]
        if ($den -ne 0) { $avgFps = [double]$Matches[1] / $den }
    }
    $effFps = if ($avgFps -gt 0 -and $avgFps -le 240) { $avgFps }
              elseif ($fps -gt 0 -and $fps -le 240)   { $fps }
              elseif ($avgFps -gt 0)                  { 60.0 }
              else                                    { 30.0 }

    # Phone footage stores orientation as metadata: a portrait clip can report
    # itself as 1920x1080 plus rotate:90. Track it so we never treat a rotated
    # clip as format-identical to an unrotated one.
    $rotation = 0
    if ($video.side_data_list) {
        foreach ($sd in $video.side_data_list) {
            if ($null -ne $sd.rotation) { $rotation = [int][math]::Abs([double]$sd.rotation) }
        }
    }
    if ($rotation -eq 0 -and $video.tags -and $video.tags.rotate) {
        try { $rotation = [int][math]::Abs([double]$video.tags.rotate) } catch { }
    }

    $duration = 0.0
    if ($data.format -and $data.format.duration) {
        try { $duration = [double]$data.format.duration } catch { }
    }

    $width  = 0; if ($video.width)  { $width  = [int]$video.width }
    $height = 0; if ($video.height) { $height = [int]$video.height }
    if ($rotation -eq 90 -or $rotation -eq 270) { $t = $width; $width = $height; $height = $t }

    return [pscustomobject]@{
        Path         = $path
        Name         = [IO.Path]::GetFileName($path)
        VideoCodec   = [string]$video.codec_name
        Width        = $width
        Height       = $height
        PixFmt       = [string]$video.pix_fmt
        Fps          = [math]::Round($effFps, 3)   # the rate to use for decisions
        RawFps       = [math]::Round($fps, 3)      # r_frame_rate, can be nonsense
        AvgFps       = [math]::Round($avgFps, 3)
        FrameRateRaw = [string]$video.r_frame_rate
        Rotation     = $rotation
        HasAudio     = [bool]$audio
        AudioCodec   = if ($audio) { [string]$audio.codec_name } else { 'none' }
        SampleRate   = if ($audio -and $audio.sample_rate) { [int]$audio.sample_rate } else { 0 }
        Channels     = if ($audio -and $audio.channels) { [int]$audio.channels } else { 0 }
        Duration     = $duration
        SizeBytes    = (Get-Item -LiteralPath $path).Length
    }
}

# Identical enough to stitch together without touching the pixels?
function Test-CanStreamCopy($clips) {
    if ($clips.Count -lt 2) { return $true }

    $first = $clips[0]
    if ($first.VideoCodec -notin @('h264', 'hevc')) { return $false }
    if ($first.HasAudio -and $first.AudioCodec -ne 'aac') { return $false }

    foreach ($c in $clips) {
        if ($c.VideoCodec   -ne $first.VideoCodec)   { return $false }
        if ($c.Width        -ne $first.Width)        { return $false }
        if ($c.Height       -ne $first.Height)       { return $false }
        if ($c.PixFmt       -ne $first.PixFmt)       { return $false }
        if ($c.FrameRateRaw -ne $first.FrameRateRaw) { return $false }
        if ($c.Rotation     -ne $first.Rotation)     { return $false }
        if ($c.HasAudio     -ne $first.HasAudio)     { return $false }
        if ($c.AudioCodec   -ne $first.AudioCodec)   { return $false }
        if ($c.SampleRate   -ne $first.SampleRate)   { return $false }
        if ($c.Channels     -ne $first.Channels)     { return $false }
    }
    return $true
}

# ---------------------------------------------------------------------------
# Encoder selection
# ---------------------------------------------------------------------------

function Test-Encoder($ffmpeg, $name) {
    try {
        Invoke-Native $ffmpeg @('-hide_banner', '-loglevel', 'quiet',
                                '-f', 'lavfi', '-i', 'color=c=black:s=320x240:r=25:d=0.2',
                                '-c:v', $name, '-f', 'null', '-') | Out-Null
        return ($LASTEXITCODE -eq 0)
    }
    catch { return $false }
}

function Select-VideoEncoder($ffmpeg, $preference) {
    if ($preference -eq 'cpu') { return @{ Name = 'libx264'; Label = 'CPU (libx264)' } }

    if ($preference -ne 'auto') {
        $name = "h264_$preference"
        if (Test-Encoder $ffmpeg $name) { return @{ Name = $name; Label = "GPU ($name)" } }
        Write-Warn "$name is not usable here; falling back to the CPU encoder."
        return @{ Name = 'libx264'; Label = 'CPU (libx264)' }
    }

    $encoderList = ''
    try { $encoderList = (Invoke-Native $ffmpeg @('-hide_banner', '-loglevel', 'quiet', '-encoders')) -join "`n" } catch { }
    foreach ($candidate in @('h264_nvenc', 'h264_qsv', 'h264_amf')) {
        if ($encoderList -match [regex]::Escape($candidate)) {
            if (Test-Encoder $ffmpeg $candidate) {
                return @{ Name = $candidate; Label = "GPU ($candidate)" }
            }
        }
    }
    return @{ Name = 'libx264'; Label = 'CPU (libx264)' }
}

function Get-QualityArgs($encoderName, $quality) {
    $crf = switch ($quality) {
        'visually-lossless' { 16 }
        'high'              { 20 }
        'medium'            { 23 }
        'small'             { 27 }
        default             { 20 }
    }

    switch ($encoderName) {
        'libx264'    { return @('-preset', 'veryfast', '-crf', "$crf") }
        'h264_nvenc' { return @('-preset', 'p5', '-rc', 'vbr', '-cq', "$crf", '-b:v', '0') }
        'h264_qsv'   { return @('-preset', 'veryfast', '-global_quality', "$crf", '-look_ahead', '0') }
        'h264_amf'   { return @('-quality', 'speed', '-rc', 'cqp', '-qp_i', "$crf", '-qp_p', "$crf") }
        default      { return @('-crf', "$crf") }
    }
}

# ---------------------------------------------------------------------------
# Merge strategies
# ---------------------------------------------------------------------------

function New-ConcatList($paths, $listPath) {
    $lines = foreach ($p in $paths) {
        $escaped = (Resolve-Path -LiteralPath $p).Path.Replace('\', '/').Replace("'", "'\''")
        "file '$escaped'"
    }
    # UTF-8 with no BOM: ffmpeg's concat demuxer chokes on a BOM.
    [IO.File]::WriteAllLines($listPath, [string[]]$lines, (New-Object Text.UTF8Encoding($false)))
}

# Joins the prepared segments into the final mp4. Nothing is re-encoded here;
# every segment already carries the same format and the same 90 kHz clock.
function Join-Segments($tools, $paths, $output, $tempDir) {
    $listPath = Join-Path $tempDir 'concat.txt'
    New-ConcatList $paths $listPath

    Invoke-Native $tools.FFmpeg @(
        '-hide_banner', '-loglevel', 'warning', '-stats', '-y',
        '-f', 'concat', '-safe', '0', '-i', "$listPath",
        '-c', 'copy', '-movflags', '+faststart',
        '--', "$output")

    return ($LASTEXITCODE -eq 0)
}

# Works out the one shape and framerate everything gets converted to.
#
# Weighted by DURATION, not by file count: whichever format most of the actual
# footage is already in wins. Picking "biggest frame, highest framerate" instead
# lets a six-second phone clip decide the format for an hour of camera footage -
# every other clip then gets upscaled and frame-doubled, which multiplies the
# encoding time and leaves most of each frame black.
function Get-TargetFormat($clips, $override) {
    if ($override -and $override.Width -gt 0 -and $override.Height -gt 0) {
        $tw = [int]$override.Width
        $th = [int]$override.Height
        $tf = [double]$override.Fps
    }
    else {
        $sizeWeight = @{}
        $fpsWeight  = @{}
        foreach ($c in $clips) {
            $seconds = [math]::Max([double]$c.Duration, 0.1)
            $sizeKey = "$($c.Width)x$($c.Height)"
            $fpsKey  = [string][math]::Round([double]$c.Fps, 3)
            if (-not $sizeWeight.ContainsKey($sizeKey)) { $sizeWeight[$sizeKey] = 0.0 }
            if (-not $fpsWeight.ContainsKey($fpsKey))   { $fpsWeight[$fpsKey]   = 0.0 }
            $sizeWeight[$sizeKey] += $seconds
            $fpsWeight[$fpsKey]   += $seconds
        }

        # Most footage wins; a tie goes to the larger frame / higher framerate.
        $bestSize = ($sizeWeight.GetEnumerator() | Sort-Object `
            @{ Expression = { $_.Value }; Descending = $true }, `
            @{ Expression = { $d = $_.Key -split 'x'; [int]$d[0] * [int]$d[1] }; Descending = $true } |
            Select-Object -First 1).Key
        $bestFps = ($fpsWeight.GetEnumerator() | Sort-Object `
            @{ Expression = { $_.Value }; Descending = $true }, `
            @{ Expression = { [double]$_.Key }; Descending = $true } |
            Select-Object -First 1).Key

        $dims = $bestSize -split 'x'
        $tw = [int]$dims[0]
        $th = [int]$dims[1]
        $tf = [double]$bestFps

        if ($override -and $override.Fps -gt 0) { $tf = [double]$override.Fps }
    }

    if ($tw -le 0 -or $th -le 0) { $tw = 1920; $th = 1080 }
    if ($tw % 2) { $tw++ }
    if ($th % 2) { $th++ }
    if ($tf -le 0)   { $tf = 30 }
    if ($tf -gt 120) { $tf = 120 }

    # Audio target, same duration-weighted idea. Matching the majority's sample
    # rate is what lets those clips be copied instead of re-encoded further down.
    $rateWeight = @{}
    $chanWeight = @{}
    foreach ($c in $clips) {
        if (-not $c.HasAudio -or $c.SampleRate -le 0) { continue }
        $seconds = [math]::Max([double]$c.Duration, 0.1)
        $rk = [string]$c.SampleRate
        $ck = [string]$c.Channels
        if (-not $rateWeight.ContainsKey($rk)) { $rateWeight[$rk] = 0.0 }
        if (-not $chanWeight.ContainsKey($ck)) { $chanWeight[$ck] = 0.0 }
        $rateWeight[$rk] += $seconds
        $chanWeight[$ck] += $seconds
    }
    $tRate = 48000
    $tChan = 2
    if ($rateWeight.Count -gt 0) {
        $tRate = [int]($rateWeight.GetEnumerator() | Sort-Object @{ Expression = { $_.Value }; Descending = $true } |
                       Select-Object -First 1).Key
    }
    if ($chanWeight.Count -gt 0) {
        $tChan = [int]($chanWeight.GetEnumerator() | Sort-Object @{ Expression = { $_.Value }; Descending = $true } |
                       Select-Object -First 1).Key
    }
    if ($tChan -lt 1 -or $tChan -gt 2) { $tChan = 2 }

    # Everything is normalised to H.264 + AAC, the pair that plays everywhere.
    # The exception is a set of clips that are already identical to each other
    # (see Get-PassThroughTarget) - those are passed through as they are.
    return [pscustomobject]@{
        Width      = $tw
        Height     = $th
        Fps        = [math]::Round($tf, 3)
        FpsExpr    = Get-FpsExpr $tf
        VideoCodec = 'h264'
        PixFmt     = 'yuv420p'
        SampleRate = $tRate
        Channels   = $tChan
        Label      = ('{0}x{1} @ {2} fps' -f $tw, $th, (Format-Fps $tf))
    }
}

# When every clip is already the same format as every other, that format is the
# target and no pixels get touched at all - whatever the codec happens to be.
function Get-PassThroughTarget($clip) {
    return [pscustomobject]@{
        Width      = $clip.Width
        Height     = $clip.Height
        Fps        = $clip.Fps
        FpsExpr    = Get-FpsExpr $clip.Fps
        VideoCodec = $clip.VideoCodec
        PixFmt     = $clip.PixFmt
        SampleRate = $clip.SampleRate
        Channels   = $clip.Channels
        Label      = ('{0}x{1} @ {2} fps' -f $clip.Width, $clip.Height, (Format-Fps $clip.Fps))
    }
}

# ffmpeg wants a rate, and the exact rational matters: a clip written as
# 30000/1001 and one written as 29.97 are not seen as the same format later,
# which would cost a needless re-encode.
function Get-FpsExpr([double]$fps) {
    $ntsc = @{
        '24000/1001'  = 23.976
        '30000/1001'  = 29.97
        '48000/1001'  = 47.952
        '60000/1001'  = 59.94
        '120000/1001' = 119.88
    }
    foreach ($pair in $ntsc.GetEnumerator()) {
        if ([math]::Abs($fps - $pair.Value) -lt 0.02) { return $pair.Key }
    }
    if ([math]::Abs($fps - [math]::Round($fps)) -lt 0.01) { return [string][int][math]::Round($fps) }
    return ('{0:0.###}' -f $fps)
}

# Is this clip already exactly what the target asks for, video and audio? If so
# it can be copied into the join untouched - no quality loss, no encoding time.
function Test-ClipMatchesTarget($clip, $target) {
    return ($clip.VideoCodec -eq $target.VideoCodec -and
            $clip.Width      -eq $target.Width -and
            $clip.Height     -eq $target.Height -and
            $clip.PixFmt     -eq $target.PixFmt -and
            $clip.Rotation   -eq 0 -and
            [math]::Abs([double]$clip.Fps - $target.Fps) -lt 0.01 -and
            $clip.HasAudio -and
            $clip.AudioCodec -eq 'aac' -and
            $clip.SampleRate -eq $target.SampleRate -and
            $clip.Channels   -eq $target.Channels)
}

# How many clips actually have to be re-encoded to reach the target?
function Get-ConvertCount($clips, $target) {
    $n = 0
    foreach ($c in $clips) { if (-not (Test-ClipMatchesTarget $c $target)) { $n++ } }
    return $n
}

# Builds the filter chain that lands a clip exactly on the target format.
function Get-TargetFilter($target) {
    # Fit inside the target box and pad the rest black, so nothing is cropped
    # and clips of a different shape still line up frame-for-frame.
    return ("scale=w=$($target.Width):h=$($target.Height):force_original_aspect_ratio=decrease," +
            "pad=$($target.Width):$($target.Height):(ow-iw)/2:(oh-ih)/2:color=black," +
            "setsar=1,fps=$($target.FpsExpr),format=yuv420p")
}

function Get-ConvertArgs($clip, $target, $encChoice, $quality, $outFile, $container) {
    $ffArgs = @('-hide_banner', '-loglevel', 'warning', '-stats', '-y')

    if (-not $clip.HasAudio) {
        # Silent clips still need an audio track, or the join desyncs.
        $ffArgs += @('-f', 'lavfi', '-i', "anullsrc=channel_layout=$(if ($target.Channels -eq 1) { 'mono' } else { 'stereo' }):sample_rate=$($target.SampleRate)")
        $ffArgs += @('-i', $clip.Path)
        $ffArgs += @('-map', '1:v:0', '-map', '0:a:0', '-shortest')
    }
    else {
        $ffArgs += @('-i', $clip.Path)
        $ffArgs += @('-map', '0:v:0', '-map', '0:a:0')
    }

    $ffArgs += @('-vf', (Get-TargetFilter $target))
    $ffArgs += @('-c:v', $encChoice.Name)
    $ffArgs += (Get-QualityArgs $encChoice.Name $quality)
    $ffArgs += @('-c:a', 'aac', '-b:a', '192k', '-ar', "$($target.SampleRate)", '-ac', "$($target.Channels)")
    $ffArgs += $SEGMENT_ARGS
    $ffArgs += @('-f', $container, '--', $outFile)
    return $ffArgs
}

# Turns one clip into one segment, ready to be joined.
#
# A clip that already matches the target is remuxed rather than re-encoded: read
# and rewritten onto the shared timescale, no encoding, no quality loss, limited
# only by disk speed. Everything else is converted.
function New-Segment($tools, $clip, $target, $encChoice, $quality, $tempDir, $index) {
    $part = Join-Path $tempDir ('part_{0:d4}.mp4' -f $index)

    if (Test-ClipMatchesTarget $clip $target) {
        Invoke-Native $tools.FFmpeg (@('-hide_banner', '-loglevel', 'warning', '-stats', '-y',
              '-i', $clip.Path,
              '-map', '0:v:0', '-map', '0:a:0',
              '-c', 'copy') + $SEGMENT_ARGS + @('-f', 'mp4', '--', $part))

        if ($LASTEXITCODE -eq 0 -and (Test-FilePath $part)) {
            return @{ Path = $part; Copied = $true }
        }
        # Remuxing should not fail, but if it does, encoding is still an option.
        Write-Warn 'Copying that clip did not work; converting it instead.'
    }

    Invoke-Native $tools.FFmpeg (Get-ConvertArgs $clip $target $encChoice $quality $part 'mp4')

    if ($LASTEXITCODE -eq 0 -and (Test-FilePath $part)) {
        return @{ Path = $part; Copied = $false }
    }
    return $null
}

# Prepares every clip as a segment, then joins them. Clips that already match
# the target cost almost nothing; only the odd ones out get encoded.
function Invoke-SegmentedMerge($tools, $clips, $output, $tempDir, $encChoice, $quality, $target) {
    $toConvert = Get-ConvertCount $clips $target

    if ($toConvert -eq 0) {
        Write-Info ("Every clip is already {0} - joining without re-encoding." -f $target.Label)
    }
    else {
        Write-Info ("Common format: {0}, H.264 + AAC {1} {2} Hz" -f `
            $target.Label, $(if ($target.Channels -eq 1) { 'mono' } else { 'stereo' }), $target.SampleRate)
        if ($toConvert -lt $clips.Count) {
            Write-Info ("{0} of {1} clip(s) already match and are copied as they are;" -f `
                ($clips.Count - $toConvert), $clips.Count)
            Write-Info ("the other {0} get converted with {1}, quality {2}." -f `
                $toConvert, $encChoice.Label, $quality)
        }
        else {
            Write-Info ("Encoder: {0}, quality: {1}" -f $encChoice.Label, $quality)
        }
    }
    Write-Host ''

    $parts   = New-Object System.Collections.Generic.List[string]
    $skipped = 0
    $index   = 0

    foreach ($clip in $clips) {
        $index++
        $what = if (Test-ClipMatchesTarget $clip $target) { 'copy   ' } else { 'convert' }
        Write-Host ("  [{0}/{1}] {2}  {3}" -f $index, $clips.Count, $what, $clip.Name) -ForegroundColor White

        $seg = New-Segment $tools $clip $target $encChoice $quality $tempDir $index
        if (-not $seg) {
            Write-Warn ("Could not prepare {0} -- it is being left out." -f $clip.Name)
            $skipped++
            continue
        }
        $parts.Add($seg.Path)
    }

    if ($parts.Count -eq 0) { Write-Bad 'None of the clips could be prepared.'; return $false }
    if ($skipped -gt 0) {
        Write-Warn ("{0} of {1} clips were left out because of errors." -f $skipped, $clips.Count)
    }

    Write-Host ''
    Write-Info 'Joining...'
    Write-Host ''

    return (Join-Segments $tools $parts.ToArray() $output $tempDir)
}

# Picks a strategy, runs it, cleans up. Returns $true on success.
function Invoke-MergeAll($tools, $clips, $outPath, $quality, $encoderPref, $forceReencode, $targetOverride) {
    $workRoot = Split-Path -Parent $outPath
    if (-not $workRoot) { $workRoot = (Get-Location).Path }
    $tempDir = Join-Path $workRoot $TEMP_DIR_NAME

    if (Test-Path -LiteralPath $tempDir) { Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    try { (Get-Item -LiteralPath $tempDir).Attributes += 'Hidden' } catch { }

    $success = $false
    try {
        if ($clips.Count -eq 1) {
            Write-Head 'Only one clip - just rewrapping it as mp4.'
            Write-Host ''
            Invoke-Native $tools.FFmpeg @(
                '-hide_banner', '-loglevel', 'warning', '-stats', '-y',
                '-i', $clips[0].Path, '-c', 'copy', '-movflags', '+faststart',
                '--', "$outPath")
            $success = ($LASTEXITCODE -eq 0)
            if (-not $success) {
                Write-Warn 'Direct copy failed; converting instead.'
                $enc = Select-VideoEncoder $tools.FFmpeg $encoderPref
                $success = Invoke-SegmentedMerge $tools $clips $outPath $tempDir $enc $quality `
                                                (Get-TargetFormat $clips $targetOverride)
            }
        }
        else {
            # One pipeline for every case. When the clips already agree on a
            # format that format is the target, so nothing is re-encoded; when
            # they disagree, only the clips that differ are converted.
            $allSame = (-not $forceReencode) -and (Test-CanStreamCopy $clips)
            $target  = if ($allSame) { Get-PassThroughTarget $clips[0] }
                       else          { Get-TargetFormat $clips $targetOverride }

            if ($allSame) { Write-Head 'All clips share the same format' }
            else          { Write-Head 'Merging (the clips are not all the same format)' }

            $enc = if ($allSame) { @{ Name = 'libx264'; Label = 'CPU (libx264)' } }
                   else          { Select-VideoEncoder $tools.FFmpeg $encoderPref }

            $success = Invoke-SegmentedMerge $tools $clips $outPath $tempDir $enc $quality $target
        }
    }
    catch {
        Write-Host ''
        Write-Bad ("Something went wrong: {0}" -f $_.Exception.Message)
        $success = $false
    }
    finally {
        if (Test-Path -LiteralPath $tempDir) {
            Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    return $success
}

function Show-Result($tools, $success, $outPath, $expectedDuration, $elapsed) {
    Write-Host ''
    if ($success -and (Test-AnyPath $outPath)) {
        $final   = Get-Item -LiteralPath $outPath
        $outInfo = Get-ClipInfo $tools.FFprobe $outPath

        Write-Host '  -------------------------------------------' -ForegroundColor DarkCyan
        Write-Good 'DONE'
        Write-Host '  -------------------------------------------' -ForegroundColor DarkCyan
        Write-Info ("File     : {0}" -f $final.FullName)
        Write-Info ("Size     : {0}" -f (Format-Size $final.Length))
        if ($outInfo) {
            Write-Info ("Length   : {0}" -f (Format-Duration $outInfo.Duration))
            Write-Info ("Video    : {0}x{1} @ {2} fps" -f $outInfo.Width, $outInfo.Height, (Format-Fps $outInfo.Fps))
            $expected = [math]::Round($expectedDuration, 0)
            $actual   = [math]::Round($outInfo.Duration, 0)
            if ($expected -gt 0 -and [math]::Abs($expected - $actual) -gt [math]::Max(2, $expected * 0.02)) {
                Write-Warn ("Heads up: inputs add up to {0} but the output is {1}." -f `
                    (Format-Duration $expected), (Format-Duration $actual))
            }
        }
        Write-Info ("Took     : {0}" -f (Format-Duration $elapsed))
        return $true
    }

    Write-Bad 'The merge did not finish. Nothing usable was written.'
    Write-Info 'Check the ffmpeg messages above for the reason.'
    if (Test-AnyPath $outPath) {
        Remove-Item -LiteralPath $outPath -Force -ErrorAction SilentlyContinue
    }
    return $false
}

# ---------------------------------------------------------------------------
# Input collection
# ---------------------------------------------------------------------------

function Get-VideoFilesInFolder($folder, $excludeLeaf) {
    Get-ChildItem -LiteralPath $folder -File |
        Where-Object { $VIDEO_EXTENSIONS -contains $_.Extension.ToLowerInvariant() } |
        Where-Object { $_.Name -notmatch '^merged(_\d+)?\.mp4$' } |
        Where-Object { -not $excludeLeaf -or $_.Name -ne $excludeLeaf } |
        Sort-Object { Get-NaturalKey $_.Name }
}

function Get-DefaultOutputPath($folder) {
    $p = Join-Path $folder 'merged.mp4'
    $n = 2
    while (Test-AnyPath $p) {
        $p = Join-Path $folder ("merged_{0}.mp4" -f $n)
        $n++
    }
    return $p
}

# Turns one typed/pasted/dropped line into a list of path candidates.
#
# Dropping several files at once gives one line of quoted paths:
#     "C:\a\first clip.mp4" "C:\a\second clip.mp4"
# while dropping a single file often gives no quotes at all, and that lone
# path may itself contain spaces. Quoted segments are therefore pulled out
# first, and only a line with no quotes in it is treated as one whole path.
function Split-PathLine([string]$line) {
    $line = $line.Trim()
    if ($line -eq '') { return @() }

    $quoted = @()
    foreach ($m in [regex]::Matches($line, '"([^"]+)"|''([^'']+)''')) {
        $v = if ($m.Groups[1].Success) { $m.Groups[1].Value } else { $m.Groups[2].Value }
        $v = $v.Trim()
        if ($v -ne '') { $quoted += $v }
    }
    if ($quoted.Count -gt 0) { return $quoted }

    # No quotes anywhere: the whole line is a single path, spaces and all.
    if (Test-AnyPath $line) { return @($line) }

    # Nothing matched a real path, so fall back to whitespace-separated
    # tokens - a list of short names, or a typo to report back.
    return @([regex]::Matches($line, '\S+') | ForEach-Object { $_.Value })
}

# ---------------------------------------------------------------------------
# Interactive terminal UI
# ---------------------------------------------------------------------------

# Reads one line of input. Returns $null when there is no more input at all.
#
# Read-Host keeps handing back an empty string forever once redirected input
# runs out, which would spin the menu loop for ever. Console.In.ReadLine gives
# a real null at end of input, so the TUI can be driven from a file or pipe.
function Read-Answer {
    try {
        if ([Console]::IsInputRedirected) { return [Console]::In.ReadLine() }
        return [string](Read-Host)
    }
    catch { return $null }
}

function Show-TuiScreen($list, $state) {
    try { Clear-Host } catch { }
    Write-Banner
    Write-Host ''

    if ($list.Count -eq 0) {
        Write-Host '  The list is empty.' -ForegroundColor DarkGray
        Write-Host ''
        Write-Host '  DRAG YOUR VIDEO FILES ONTO THIS WINDOW, then press Enter.' -ForegroundColor Yellow
        Write-Host '  (a whole folder works too, and so does a pasted path)' -ForegroundColor DarkGray
    }
    else {
        Write-Host ("  CLIPS TO MERGE  ({0})" -f $list.Count) -ForegroundColor White
        Write-Rule
        for ($i = 0; $i -lt $list.Count; $i++) {
            $c     = $list[$i]
            $audio = if ($c.HasAudio) { $c.AudioCodec } else { 'silent' }
            Write-Host ("  {0,3}  " -f ($i + 1)) -NoNewline -ForegroundColor DarkCyan
            Write-Host (Limit-Text $c.Name 34) -NoNewline -ForegroundColor White
            Write-Host ("  {0,9}  {1,5}fps  {2,-5} {3,-6} {4}" -f `
                ("$($c.Width)x$($c.Height)"), (Format-Fps $c.Fps), $c.VideoCodec, $audio,
                (Format-Duration $c.Duration)) -ForegroundColor DarkGray
        }
        Write-Rule
        $dur = ($list | Measure-Object -Property Duration  -Sum).Sum
        $sz  = ($list | Measure-Object -Property SizeBytes -Sum).Sum
        Write-Host ("  Total: {0} clips   {1}   {2}" -f $list.Count, (Format-Duration $dur), (Format-Size $sz)) -ForegroundColor Gray

        if (Test-CanStreamCopy $list) {
            Write-Host '  Plan : fast join, no re-encode - takes seconds' -ForegroundColor DarkGray
        }
        else {
            $target = Get-TargetFormat $list $state.Target
            $n      = Get-ConvertCount $list $target
            $note   = if ($state.Target) { ' (your choice)' } else { ' (auto)' }
            Write-Host ("  Plan : convert to {0}{1}, then join" -f $target.Label, $note) -ForegroundColor DarkGray
            Write-Host ("         {0} of {1} clip(s) need converting - press T to change the target" -f `
                $n, $list.Count) -ForegroundColor DarkGray
        }
    }

    Write-Host ''
    Write-Host ("  Output : {0}" -f $state.OutputName) -ForegroundColor Gray
    Write-Host ("  Quality: {0}      Encoder: {1}" -f $state.Quality, $state.Encoder) -ForegroundColor DarkGray
    Write-Host ''
    Write-Host '   [S] START MERGE     [R] Remove    [M] Move     [A] Sort by name' -ForegroundColor White
    Write-Host '   [F] Add a folder    [C] Clear     [O] Output   [Q] Quality' -ForegroundColor White
    Write-Host '   [T] Target size     [E] Encoder   [X] Exit' -ForegroundColor White
    Write-Host ''

    if ($state.Message) {
        $colour = switch ($state.MessageKind) {
            'good' { 'Green' }
            'warn' { 'Yellow' }
            'bad'  { 'Red' }
            default { 'Gray' }
        }
        Write-Host ("  > " + $state.Message) -ForegroundColor $colour
        Write-Host ''
    }
}

function Add-ClipsToList($tools, $list, $candidates, $state) {
    $added = 0
    $bad   = 0

    foreach ($candidate in $candidates) {
        $expanded = @()

        if (Test-FolderPath $candidate) {
            $expanded = (Get-VideoFilesInFolder $candidate $null | ForEach-Object { $_.FullName })
            if ($expanded.Count -eq 0) {
                $state.Message = "No videos found in that folder: $candidate"
                $state.MessageKind = 'warn'
                continue
            }
        }
        elseif (Test-FilePath $candidate) {
            $expanded = @((Resolve-Path -LiteralPath $candidate).Path)
        }
        elseif ($candidate -match '[\*\?]') {
            $expanded = (Get-ChildItem -Path $candidate -File -ErrorAction SilentlyContinue |
                         Sort-Object { Get-NaturalKey $_.Name } | ForEach-Object { $_.FullName })
        }

        if ($expanded.Count -eq 0) {
            Write-Warn "Not found: $candidate"
            $bad++
            continue
        }

        foreach ($file in $expanded) {
            $ext = [IO.Path]::GetExtension($file).ToLowerInvariant()
            if ($VIDEO_EXTENSIONS -notcontains $ext) {
                Write-Warn ("Not a supported video format, skipped: {0}" -f [IO.Path]::GetFileName($file))
                $bad++
                continue
            }
            $info = Get-ClipInfo $tools.FFprobe $file
            if (-not $info) {
                Write-Warn ("Could not read as video, skipped: {0}" -f [IO.Path]::GetFileName($file))
                $bad++
                continue
            }
            $list.Add($info)
            $added++
        }
    }

    if ($added -gt 0) {
        $state.Message = "Added $added clip(s)." + $(if ($bad) { " $bad item(s) skipped." } else { '' })
        $state.MessageKind = if ($bad) { 'warn' } else { 'good' }
    }
    elseif ($bad -gt 0) {
        $state.Message = "Nothing added - $bad item(s) could not be used."
        $state.MessageKind = 'bad'
        Start-Sleep -Seconds 2
    }
    return $added
}

# Accepts "2", "2,5", "2-4" and combinations, returns 0-based indexes.
function Resolve-IndexSpec([string]$spec, [int]$count) {
    $idx = New-Object System.Collections.Generic.List[int]
    foreach ($part in ($spec -split '[,\s]+')) {
        if ($part -eq '') { continue }
        if ($part -match '^(\d+)\s*-\s*(\d+)$') {
            $a = [int]$Matches[1]; $b = [int]$Matches[2]
            if ($a -gt $b) { $t = $a; $a = $b; $b = $t }
            for ($i = $a; $i -le $b; $i++) { if ($i -ge 1 -and $i -le $count) { $idx.Add($i - 1) } }
        }
        elseif ($part -match '^\d+$') {
            $i = [int]$part
            if ($i -ge 1 -and $i -le $count) { $idx.Add($i - 1) }
        }
    }
    return ($idx | Sort-Object -Unique)
}

function Start-Tui($tools, $scriptRoot, $preloadFiles) {
    $list = New-Object System.Collections.Generic.List[object]

    $state = @{
        OutputName  = 'merged.mp4'
        Quality     = $Quality
        Encoder     = $Encoder
        Target      = $null   # $null = work it out from the clips
        Message     = ''
        MessageKind = 'info'
    }

    if ($preloadFiles -and @($preloadFiles).Count -gt 0) {
        # Files were dropped onto the launcher: start with exactly those, in the
        # order they were dropped.
        Write-Host ''
        Write-Info ("Reading {0} dropped file(s)..." -f @($preloadFiles).Count)
        [void](Add-ClipsToList $tools $list @($preloadFiles) $state)
        $state.Message = ("Loaded {0} dropped clip(s). Reorder if you like, then press S." -f $list.Count)
        $state.MessageKind = 'info'
    }
    else {
        # Otherwise preload whatever is already sitting next to the script - the
        # common case is "the clips are in this folder", and it saves any typing.
        $preload = Get-VideoFilesInFolder $scriptRoot $null
        if ($preload) {
            Write-Host ''
            Write-Info ("Reading {0} clip(s) already in this folder..." -f @($preload).Count)
            [void](Add-ClipsToList $tools $list @($preload | ForEach-Object { $_.FullName }) $state)
            $state.Message = ("Loaded {0} clip(s) from this folder. Drag in more, or press S to start." -f $list.Count)
            $state.MessageKind = 'info'
        }
    }

    while ($true) {
        Show-TuiScreen $list $state
        $state.Message = ''

        Write-Host '  Drop files here, or pick a letter: ' -NoNewline -ForegroundColor Cyan
        $entry = Read-Answer
        if ($null -eq $entry) { return }
        $entry = $entry.Trim()
        if ($entry -eq '') { continue }

        # A path (or several) means "add these", whatever it looks like.
        $looksLikePath = ($entry -match '[\\/]') -or ($entry -match '^[A-Za-z]:') -or
                         (Test-AnyPath $entry.Trim('"'))

        if ($looksLikePath) {
            [void](Add-ClipsToList $tools $list (Split-PathLine $entry) $state)
            continue
        }

        # A single letter is the documented way in, but accept the obvious
        # whole words too - "exit" should not toggle the encoder.
        $cmd = $entry.Substring(0, 1).ToUpperInvariant()
        switch -Regex ($entry.ToLowerInvariant()) {
            '^(exit|quit|bye|close)$'   { $cmd = 'X' }
            '^(start|merge|go|run|ok)$' { $cmd = 'S' }
            '^(clear|reset|empty)$'     { $cmd = 'C' }
            '^(sort|arrange|order)$'    { $cmd = 'A' }
            '^(remove|delete|del)$'     { $cmd = 'R' }
            '^(move|reorder)$'          { $cmd = 'M' }
            '^(output|name|save)$'      { $cmd = 'O' }
            '^(quality)$'               { $cmd = 'Q' }
            '^(folder|add)$'            { $cmd = 'F' }
            '^(target|size|res|fps)$'   { $cmd = 'T' }
        }

        switch ($cmd) {

            'S' {
                if ($list.Count -eq 0) {
                    $state.Message = 'Nothing to merge yet - drag some files in first.'
                    $state.MessageKind = 'warn'
                    continue
                }

                $folder = Split-Path -Parent $list[0].Path
                $name   = $state.OutputName
                if ([IO.Path]::GetExtension($name) -eq '') { $name = "$name.mp4" }
                $outPath = if ([IO.Path]::IsPathRooted($name)) { $name } else { Join-Path $folder $name }

                if (Test-AnyPath $outPath) {
                    Write-Host ''
                    Write-Warn ("{0} already exists." -f $outPath)
                    Write-Host '  Overwrite it? [y/N] ' -NoNewline -ForegroundColor Cyan
                    $ans = Read-Answer;   if ($null -eq $ans)   { return }
                    if ($ans -notmatch '^[Yy]') {
                        $outPath = Get-DefaultOutputPath $folder
                        Write-Info ("Writing to {0} instead." -f [IO.Path]::GetFileName($outPath))
                        Start-Sleep -Seconds 1
                    }
                }

                try { Clear-Host } catch { }
                Write-Banner
                Write-Head ("Merging {0} clip(s)" -f $list.Count)
                Write-Info ("Output: {0}" -f $outPath)

                $totalDuration = ($list | Measure-Object -Property Duration -Sum).Sum
                $started = Get-Date
                $ok = Invoke-MergeAll $tools $list.ToArray() $outPath $state.Quality $state.Encoder $ForceReencode.IsPresent $state.Target
                [void](Show-Result $tools $ok $outPath $totalDuration ((Get-Date) - $started).TotalSeconds)

                Write-Host ''
                Write-Host '  [Enter] back to the list    [P] open the folder    [X] exit' -ForegroundColor White
                Write-Host '  > ' -NoNewline -ForegroundColor Cyan
                $after = Read-Answer; if ($null -eq $after) { return }
                if ($after -match '^[Pp]') {
                    try { Start-Process 'explorer.exe' -ArgumentList ('/select,"{0}"' -f $outPath) } catch { }
                }
                if ($after -match '^[Xx]') { return }

                if ($ok) {
                    $state.Message = 'Merge finished. The list is still here if you want to merge more.'
                    $state.MessageKind = 'good'
                    $state.OutputName = [IO.Path]::GetFileName((Get-DefaultOutputPath (Split-Path -Parent $outPath)))
                }
                else {
                    $state.Message = 'That merge failed - see the messages above.'
                    $state.MessageKind = 'bad'
                }
            }

            'R' {
                if ($list.Count -eq 0) { continue }
                Write-Host '  Remove which? (e.g. 3  or  2,5  or  2-4): ' -NoNewline -ForegroundColor Cyan
                $spec = Read-Answer;  if ($null -eq $spec)  { return }
                $targets = Resolve-IndexSpec $spec $list.Count
                if (-not $targets -or @($targets).Count -eq 0) {
                    $state.Message = 'Nothing matched that.'
                    $state.MessageKind = 'warn'
                    continue
                }
                foreach ($i in (@($targets) | Sort-Object -Descending)) { $list.RemoveAt($i) }
                $state.Message = ("Removed {0} clip(s)." -f @($targets).Count)
                $state.MessageKind = 'good'
            }

            'M' {
                if ($list.Count -lt 2) { continue }
                Write-Host '  Move which number? ' -NoNewline -ForegroundColor Cyan
                $from = Read-Answer;  if ($null -eq $from)  { return }
                Write-Host '  To which position? ' -NoNewline -ForegroundColor Cyan
                $to   = Read-Answer;  if ($null -eq $to)    { return }
                if ($from -notmatch '^\d+$' -or $to -notmatch '^\d+$') {
                    $state.Message = 'Both answers need to be numbers.'
                    $state.MessageKind = 'warn'
                    continue
                }
                $f = [int]$from - 1
                $t = [int]$to   - 1
                if ($f -lt 0 -or $f -ge $list.Count) {
                    $state.Message = "There is no clip number $from."
                    $state.MessageKind = 'warn'
                    continue
                }
                if ($t -lt 0) { $t = 0 }
                if ($t -ge $list.Count) { $t = $list.Count - 1 }
                $item = $list[$f]
                $list.RemoveAt($f)
                $list.Insert($t, $item)
                $state.Message = ("Moved {0} to position {1}." -f $item.Name, ($t + 1))
                $state.MessageKind = 'good'
            }

            'A' {
                if ($list.Count -lt 2) { continue }
                $sorted = @($list.ToArray() | Sort-Object { Get-NaturalKey $_.Name })
                $list.Clear()
                foreach ($c in $sorted) { $list.Add($c) }
                $state.Message = 'Sorted by filename (1, 2, 10 - not 1, 10, 2).'
                $state.MessageKind = 'good'
            }

            'F' {
                Write-Host '  Folder path (or drag the folder in): ' -NoNewline -ForegroundColor Cyan
                $f = Read-Answer;     if ($null -eq $f)     { return }
                if ($f.Trim() -eq '') { continue }
                [void](Add-ClipsToList $tools $list (Split-PathLine $f) $state)
            }

            'C' {
                $list.Clear()
                $state.Message = 'List cleared.'
                $state.MessageKind = 'good'
            }

            'O' {
                Write-Host '  Output file name: ' -NoNewline -ForegroundColor Cyan
                $n = Read-Answer; if ($null -eq $n) { return }; $n = $n.Trim().Trim('"')
                if ($n -eq '') { continue }
                # Reject a name Windows cannot store, here rather than at the end
                # of a long merge. A full path is allowed, so only its last part
                # is checked for illegal characters.
                $leaf = ''
                $dir  = ''
                try {
                    $leaf = [IO.Path]::GetFileName($n)
                    $dir  = [IO.Path]::GetDirectoryName($n)
                }
                catch { }
                if (-not $leaf -or (@([IO.Path]::GetInvalidFileNameChars() | Where-Object { $leaf.Contains($_) })).Count -gt 0) {
                    $state.Message = 'A file name cannot contain any of  \ / : * ? " < > |'
                    $state.MessageKind = 'warn'
                    continue
                }
                if ($dir -and -not (Test-FolderPath $dir)) {
                    $state.Message = "There is no folder called $dir"
                    $state.MessageKind = 'warn'
                    continue
                }
                if ([IO.Path]::GetExtension($n) -eq '') { $n = "$n.mp4" }
                $state.OutputName = $n
                $state.Message = "Output name set to $n"
                $state.MessageKind = 'good'
            }

            'Q' {
                Write-Host ''
                for ($i = 0; $i -lt $QUALITY_LEVELS.Count; $i++) {
                    Write-Host ("   {0}) {1}" -f ($i + 1), $QUALITY_LEVELS[$i]) -ForegroundColor Gray
                }
                Write-Host '  Pick 1-4: ' -NoNewline -ForegroundColor Cyan
                $p = Read-Answer;     if ($null -eq $p)     { return }
                if ($p -match '^[1-4]$') {
                    $state.Quality = $QUALITY_LEVELS[[int]$p - 1]
                    $state.Message = "Quality set to $($state.Quality)"
                    $state.MessageKind = 'good'
                }
                # Only matters when clips get re-encoded; the fast path is
                # always a bit-for-bit copy.
            }

            'T' {
                if ($list.Count -eq 0) {
                    $state.Message = 'Add some clips first - the choices depend on what they are.'
                    $state.MessageKind = 'warn'
                    continue
                }

                $auto = Get-TargetFormat $list $null
                $big  = ($list.ToArray() | Sort-Object { [int]$_.Width * [int]$_.Height } -Descending |
                         Select-Object -First 1)
                $small = ($list.ToArray() | Sort-Object { [int]$_.Width * [int]$_.Height } |
                          Select-Object -First 1)

                Write-Host ''
                Write-Info 'Everything gets converted to one shape and framerate.'
                Write-Info 'Smaller and slower-framerate means a much faster merge.'
                Write-Host ''
                Write-Host ("   1) Auto - matches most of your footage : {0}" -f $auto.Label) -ForegroundColor Gray
                Write-Host ("   2) Biggest clip : {0}x{1} @ {2} fps" -f $big.Width, $big.Height, (Format-Fps $big.Fps)) -ForegroundColor Gray
                Write-Host ("   3) Smallest clip - fastest : {0}x{1} @ {2} fps" -f $small.Width, $small.Height, (Format-Fps $small.Fps)) -ForegroundColor Gray
                Write-Host  '   4) 1920x1080 @ 30 fps' -ForegroundColor Gray
                Write-Host  '   5) 1280x720 @ 30 fps' -ForegroundColor Gray
                Write-Host  '   6) Type your own, e.g. 1080x1920@25' -ForegroundColor Gray
                Write-Host '  Pick 1-6: ' -NoNewline -ForegroundColor Cyan
                $pick = Read-Answer; if ($null -eq $pick) { return }

                switch ($pick.Trim()) {
                    '1' { $state.Target = $null }
                    '2' { $state.Target = @{ Width = $big.Width;   Height = $big.Height;   Fps = $big.Fps } }
                    '3' { $state.Target = @{ Width = $small.Width; Height = $small.Height; Fps = $small.Fps } }
                    '4' { $state.Target = @{ Width = 1920; Height = 1080; Fps = 30 } }
                    '5' { $state.Target = @{ Width = 1280; Height = 720;  Fps = 30 } }
                    '6' {
                        Write-Host '  Size and framerate (WxH@fps): ' -NoNewline -ForegroundColor Cyan
                        $custom = Read-Answer; if ($null -eq $custom) { return }
                        if ($custom -match '^\s*(\d{2,5})\s*[xX*]\s*(\d{2,5})\s*(?:@\s*([\d.]+))?\s*$') {
                            $fps = if ($Matches[3]) { [double]$Matches[3] } else { $auto.Fps }
                            $state.Target = @{ Width = [int]$Matches[1]; Height = [int]$Matches[2]; Fps = $fps }
                        }
                        else {
                            $state.Message = "Could not read '$custom'. Use something like 1920x1080@30."
                            $state.MessageKind = 'warn'
                            continue
                        }
                    }
                    default { continue }
                }

                $chosen = Get-TargetFormat $list $state.Target
                $state.Message = "Target set to $($chosen.Label)" + $(if (-not $state.Target) { ' (auto)' } else { '' })
                $state.MessageKind = 'good'
            }

            'E' {
                $state.Encoder = if ($state.Encoder -eq 'auto') { 'cpu' } else { 'auto' }
                $state.Message = "Encoder set to $($state.Encoder)" +
                                 $(if ($state.Encoder -eq 'auto') { ' (use the GPU when possible)' } else { ' (always libx264)' })
                $state.MessageKind = 'good'
            }

            'X' { return }

            default {
                $state.Message = "Don't know '$entry'. Pick one of the letters, or drag files in."
                $state.MessageKind = 'warn'
            }
        }
    }
}

# ---------------------------------------------------------------------------
# One-shot (non-interactive) run
# ---------------------------------------------------------------------------

function Start-OneShot($tools, $scriptRoot) {
    $selected = @()
    $sourceFolder = $null

    if ($Files -and $Files.Count -gt 0) {
        foreach ($f in $Files) {
            if (Test-FilePath $f) { $selected += (Resolve-Path -LiteralPath $f).Path }
            else { Write-Warn "Not found, ignoring: $f" }
        }
        if ($selected.Count -gt 0) { $sourceFolder = Split-Path -Parent $selected[0] }
        Write-Info 'Using the files you dropped, in the order they were dropped.'
    }
    else {
        $sourceFolder = if ($Folder) { $Folder } else { $scriptRoot }
        if (-not (Test-FolderPath $sourceFolder)) {
            Stop-Here "Folder not found: $sourceFolder"
        }
        $sourceFolder = (Resolve-Path -LiteralPath $sourceFolder).Path

        $outputLeaf = if ($Output) { [IO.Path]::GetFileName($Output) } else { $null }
        $found = Get-VideoFilesInFolder $sourceFolder $outputLeaf

        # order.txt (one filename per line) overrides filename ordering.
        $orderFile = Join-Path $sourceFolder 'order.txt'
        if (Test-Path -LiteralPath $orderFile) {
            Write-Info 'order.txt found - using the order listed in it.'
            $byName = @{}
            foreach ($f in $found) { $byName[$f.Name.ToLowerInvariant()] = $f.FullName }
            foreach ($line in (Get-Content -LiteralPath $orderFile)) {
                $key = $line.Trim().Trim('"').ToLowerInvariant()
                if ($key -eq '' -or $key.StartsWith('#')) { continue }
                if ($byName.ContainsKey($key)) { $selected += $byName[$key] }
                else { Write-Warn "order.txt lists a file that is not here: $line" }
            }
            if ($selected.Count -eq 0) { Write-Warn 'order.txt matched nothing; falling back to filename order.' }
        }

        if ($selected.Count -eq 0) { $selected = @($found | ForEach-Object { $_.FullName }) }
    }

    if ($selected.Count -eq 0) {
        Write-Host ''
        Write-Bad 'No video clips found.'
        Write-Host ''
        Write-Info ("Folder checked: {0}" -f $sourceFolder)
        Write-Info ("Formats read:   {0}" -f (($VIDEO_EXTENSIONS | ForEach-Object { $_.TrimStart('.') }) -join ' '))
        Write-Host ''
        Write-Info 'Copy your clips into that folder, then run this again.'
        $script:ExitCode = 1
        Complete-Run
    }

    if ($Output) {
        $outPath = if ([IO.Path]::IsPathRooted($Output)) { $Output } else { Join-Path $sourceFolder $Output }
        if ([IO.Path]::GetExtension($outPath) -eq '') { $outPath = "$outPath.mp4" }
    }
    else { $outPath = Get-DefaultOutputPath $sourceFolder }

    Write-Head ("Found {0} clip(s) in: {1}" -f $selected.Count, $sourceFolder)
    Write-Host ''

    $clips = @()
    $i = 0
    foreach ($path in $selected) {
        $i++
        Write-Host ("  {0,3}. {1}" -f $i, [IO.Path]::GetFileName($path)) -NoNewline
        $info = Get-ClipInfo $tools.FFprobe $path
        if (-not $info) {
            Write-Host '   [SKIPPED - not a readable video]' -ForegroundColor Yellow
            continue
        }
        $audioNote = if ($info.HasAudio) { $info.AudioCodec } else { 'no audio' }
        Write-Host ("   {0}x{1}  {2}fps  {3}  {4}  {5}" -f `
            $info.Width, $info.Height, (Format-Fps $info.Fps), $info.VideoCodec, $audioNote,
            (Format-Duration $info.Duration)) -ForegroundColor DarkGray
        $clips += $info
    }

    if ($clips.Count -eq 0) { Stop-Here 'None of the files could be read as video.' }

    $totalDuration = ($clips | Measure-Object -Property Duration  -Sum).Sum
    $totalInput    = ($clips | Measure-Object -Property SizeBytes -Sum).Sum

    Write-Host ''
    Write-Info ("Total input : {0} clips, {1}, {2}" -f $clips.Count, (Format-Duration $totalDuration), (Format-Size $totalInput))
    Write-Info ("Output file : {0}" -f $outPath)

    $started = Get-Date
    $ok = Invoke-MergeAll $tools $clips $outPath $Quality $Encoder $ForceReencode.IsPresent $null
    $ok = Show-Result $tools $ok $outPath $totalDuration ((Get-Date) - $started).TotalSeconds
    if (-not $ok) { $script:ExitCode = 1 }
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

$scriptRoot = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }

# A path argument ending in a backslash - "%~dp0" from a .bat file, say - loses
# its closing quote to Windows argument parsing and arrives with a stray quote
# stuck on the end. Scrub the incoming paths so one never reaches the rest of
# the script.
foreach ($name in 'Folder', 'Output', 'FileList') {
    $value = Get-Variable -Name $name -ValueOnly -ErrorAction SilentlyContinue
    if ($value -is [string] -and $value -ne '') {
        Set-Variable -Name $name -Value ($value.Trim().Trim('"').Trim())
    }
}

if (Test-FilePath $FileList) {
    # cmd's echo writes in the console (OEM) code page, so read it back that way
    # or non-English filenames arrive mangled.
    $Files = @(
        Get-Content -LiteralPath $FileList -Encoding Oem |
            ForEach-Object { $_.Trim().Trim('"') } |
            Where-Object   { $_ -ne '' }
    )
}

# Anything unexpected must stay on screen. A window that vanishes tells the
# user nothing, so every escape route from here ends at Complete-Run.
try {
    Write-Banner
    $tools = Resolve-Tools $scriptRoot

    if ($Tui) {
        $tuiFolder = if (Test-FolderPath $Folder) { (Resolve-Path -LiteralPath $Folder).Path } else { $scriptRoot }
        Start-Tui $tools $tuiFolder $Files
        exit $script:ExitCode
    }

    Start-OneShot $tools $scriptRoot
    Complete-Run
}
catch {
    Write-Host ''
    Write-Bad ('Unexpected error: ' + $_.Exception.Message)
    if ($_.InvocationInfo -and $_.InvocationInfo.PositionMessage) {
        Write-Info ($_.InvocationInfo.PositionMessage.Trim())
    }
    $script:ExitCode = 1
    Complete-Run
}
