@echo off
setlocal
title Video Merger

rem ---------------------------------------------------------------
rem  MERGE-VIDEOS.bat
rem
rem  Double-click this file. The Video Merger screen opens and
rem  lists any clips already in this folder.
rem
rem  Then either:
rem    - drag more video files onto the window and press Enter, or
rem    - press S to merge what is already listed.
rem
rem  You can also drag clips straight onto this .bat file - they
rem  open in the list in the order you dropped them.
rem ---------------------------------------------------------------

set "PS1=%~dp0merge-videos.ps1"

if not exist "%PS1%" (
    echo.
    echo  ERROR: merge-videos.ps1 was not found next to this .bat file.
    echo  Both files must stay in the same folder.
    echo.
    pause
    exit /b 1
)

rem A roomy window so the clip table lines up.
mode con: cols=100 lines=40 >nul 2>&1

rem Start in this folder, so the script finds the clips beside it.
rem NOTE: never pass "%~dp0" as an argument - it ends with a backslash,
rem which Windows reads as escaping the closing quote.
cd /d "%~dp0"

if "%~1"=="" goto :plain

rem --- Files were dropped onto this .bat -------------------------
rem  The paths go into a temp list file, one per line, rather than
rem  onto a command line, so filenames containing spaces, quotes,
rem  apostrophes, ampersands or commas cannot break anything.

set "LISTFILE=%TEMP%\video-merge-drop-%RANDOM%%RANDOM%.txt"
break > "%LISTFILE%"

:collect
if "%~1"=="" goto :run_dropped
>> "%LISTFILE%" echo "%~1"
shift
goto :collect

:run_dropped
powershell -NoProfile -ExecutionPolicy Bypass -File "%PS1%" -Tui -FileList "%LISTFILE%"
set "RC=%ERRORLEVEL%"
del "%LISTFILE%" >nul 2>&1
goto :finish

:plain
powershell -NoProfile -ExecutionPolicy Bypass -File "%PS1%" -Tui
set "RC=%ERRORLEVEL%"
goto :finish

:finish
rem If PowerShell itself failed to start, the window would otherwise
rem vanish before the reason could be read.
if not "%RC%"=="0" (
    echo.
    echo  Video Merger exited with code %RC%.
    echo.
    pause
)
endlocal & exit /b %RC%
