#!/bin/sh
#
# One-line installer:
#
#   curl -fsSL https://raw.githubusercontent.com/alee-ibrahim/vmerge/main/install.sh | sh
#
# Why this exists, and why it is a pipe rather than a file to download:
# macOS quarantines anything a browser saves, refuses to run it, and passes the
# mark on to everything unpacked from it - so a downloaded zip cannot contain a
# script that clears its own quarantine, because the script is quarantined too.
# curl sets no such mark. Piping the installer is not a shortcut around
# Gatekeeper; it is the only shape that can do the setup at all without a
# notarised build, which needs a paid Apple developer account.
#
# Everything is wrapped in a function called on the last line. A pipe that is
# cut halfway leaves a truncated script, and without this the shell would run
# whatever complete lines arrived before the break.

set -eu

main() {
    REPO="${VMERGE_REPO:-alee-ibrahim/vmerge}"
    PREFIX="${VMERGE_PREFIX:-$HOME/.local/bin}"

    say() { printf '  %s\n' "$*"; }
    die() { printf '\n  %s\n\n' "$*" >&2; exit 1; }

    printf '\n  Installing vmerge\n\n'

    # ---------------------------------------------------------------- platform
    os=$(uname -s)
    arch=$(uname -m)

    case "$os/$arch" in
        Darwin/arm64)
            asset="MERGE-VIDEOS-macos-arm64"
            ;;
        Darwin/*)
            die "This installs the Apple Silicon build, and this Mac is $arch.
  Build from source instead: https://github.com/$REPO"
            ;;
        *)
            die "There is no published build for $os/$arch.
  Windows: download MERGE-VIDEOS.exe from
    https://github.com/$REPO/releases/latest
  Anything else: build from source, https://github.com/$REPO"
            ;;
    esac

    for tool in curl shasum install; do
        command -v "$tool" >/dev/null 2>&1 || die "This needs $tool, which is not on PATH."
    done

    # ---------------------------------------------------------------- download
    # The /releases/latest/download/ route redirects to the newest release's
    # asset without an API call. That matters: api.github.com rate-limits by IP
    # and would make this fail for reasons having nothing to do with the user.
    base="https://github.com/$REPO/releases/latest/download"

    work=$(mktemp -d)
    # Cleans up on failure and on ctrl-c as well as on success, so a stopped
    # install does not leave 3 MB in the temp folder.
    trap 'rm -rf "$work"' EXIT INT TERM

    say "Downloading $asset"
    curl -fsSL --retry 3 -o "$work/vmerge" "$base/$asset" \
        || die "Could not download the program from $base/$asset"

    # ------------------------------------------------------------------ verify
    # The release publishes a digest beside each binary. Checking it costs half
    # a second and is the difference between "it downloaded" and "it downloaded
    # intact".
    if curl -fsSL --retry 3 -o "$work/expected" "$base/$asset.sha256" 2>/dev/null; then
        expected=$(tr -d ' \t\r\n*' < "$work/expected" | cut -c1-64)
        actual=$(shasum -a 256 "$work/vmerge" | cut -d' ' -f1)
        if [ "$expected" != "$actual" ]; then
            die "The download does not match its published checksum.
  expected $expected
  got      $actual
  Nothing has been installed."
        fi
        say "Checksum verified"
    else
        say "No checksum published for this release; skipping that check"
    fi

    # ----------------------------------------------------------------- install
    mkdir -p "$PREFIX" || die "Could not create $PREFIX"
    install -m 755 "$work/vmerge" "$PREFIX/vmerge" \
        || die "Could not write to $PREFIX"

    # curl does not set the quarantine attribute, so there should be nothing to
    # clear. Done anyway because it costs nothing and covers the case where
    # someone saved this script in a browser and ran it by hand.
    xattr -d com.apple.quarantine "$PREFIX/vmerge" 2>/dev/null || true

    # The release is already ad-hoc signed on the build machine. An arm64 binary
    # with no signature at all is killed by the kernel before it starts, so this
    # re-signs only when the existing signature does not verify.
    if command -v codesign >/dev/null 2>&1; then
        codesign -v "$PREFIX/vmerge" 2>/dev/null \
            || codesign --force --sign - "$PREFIX/vmerge" 2>/dev/null \
            || true
    fi

    say "Installed to $PREFIX/vmerge"

    # ------------------------------------------------------------ warm up tools
    # ffmpeg installs itself on first use. Doing it now means the first real
    # merge starts immediately instead of pausing for a 40 MB download, and it
    # surfaces a broken setup here, where there is a message to read, rather
    # than in the middle of something.
    #
    # stdin comes from /dev/null for two reasons: the program waits on a
    # keypress when it has something to report, and under `curl | sh` this
    # script's own stdin is the pipe - a child reading it would eat the rest of
    # the installer.
    say "Fetching ffmpeg (about 40 MB, once)"
    empty=$(mktemp -d)
    "$PREFIX/vmerge" --no-tui --no-update --folder "$empty" </dev/null >/dev/null 2>&1 || true
    rm -rf "$empty"

    if [ -x "$PREFIX/ffmpeg/bin/ffmpeg" ]; then
        say "ffmpeg ready"
    else
        say "ffmpeg is not set up yet - it will download on first use"
    fi

    # -------------------------------------------------------------------- done
    printf '\n  Done.\n\n'

    case ":$PATH:" in
        *":$PREFIX:"*)
            say "Run it by typing:  vmerge"
            printf '\n'
            return 0
            ;;
    esac

    # Not on PATH. Offering to fix it beats printing a line to copy: typing the
    # full path every time is the friction that makes people ask for the program
    # to be installed somewhere silly instead.
    say "$PREFIX is not on your PATH, so \"vmerge\" on its own will not be found."
    printf '\n'

    # The shell this will belong to, not the one running the installer - `sh`
    # here is whatever the pipe invoked. A wrong file is worse than none: the
    # line lands somewhere never read, and nothing happens with no error.
    case "${SHELL:-}" in
        */zsh)  rc="$HOME/.zshrc" ;;
        */bash) rc="$HOME/.bashrc" ;;
        *)      rc="" ;;
    esac

    # stdin is the pipe carrying this script, so a plain `read` would swallow
    # the rest of it rather than wait for a person. /dev/tty is the keyboard.
    # Without one - a scripted install - nothing is asked and nothing is
    # written: editing someone's shell config unasked is not a default.
    # `[ -r /dev/tty ]` is not the test to use: the device node is readable by
    # its permissions even where there is no controlling terminal to open, so it
    # passes and the open then fails - printing the question and a shell error
    # at someone who was never going to be asked. Opening it is the only honest
    # check.
    reply=n
    if [ -n "$rc" ] && { : </dev/tty; } 2>/dev/null; then
        printf '  Add it to %s so you can just type "vmerge"? [Y/n] ' "$rc"
        read -r reply </dev/tty || reply=n
    fi

    case "${reply:-y}" in
        y|Y|yes|Yes|YES|"")
            if [ -n "$rc" ]; then
                # Idempotent: running the installer twice must not leave two
                # copies of the line in the file.
                if grep -qsF "$PREFIX" "$rc"; then
                    say "Already in $rc."
                else
                    printf '\n# Added by the vmerge installer\nexport PATH="%s:$PATH"\n' \
                        "$PREFIX" >> "$rc" || die "Could not write to $rc"
                    say "Added to $rc."
                fi
                printf '\n'
                say "Open a new Terminal window, then type:  vmerge"
                printf '\n'
                return 0
            fi
            ;;
    esac

    say "Run it in full:"
    say ""
    say "  $PREFIX/vmerge"
    say ""
    say "or add it to your PATH yourself, then reopen Terminal:"
    say ""
    say "  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ${rc:-~/.zshrc}"
    printf '\n'
}

main "$@"
