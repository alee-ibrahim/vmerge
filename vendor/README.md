# Vendored ffmpeg

`ffmpeg-release-essentials.7z` in this folder is an **unmodified** copy of the
Windows build published by [gyan.dev](https://www.gyan.dev/ffmpeg/builds/) —
ffmpeg 9.0.1-essentials_build, 32.5 MB,
`sha256 49a73bdf0850092a252ac4641d922f3048d63ed113e196cc65ce1e4f7fb33e85` —
mirrored here so that first-run setup does not depend on that one server.

It is served to the program over `raw.githubusercontent.com`, which is reachable
and fast on networks where GitHub's *release asset* host is not — that host
(`objects.githubusercontent.com`) returns nothing at all from some connections,
which is why the archive lives in the repository tree rather than attached to a
release.

## Licence

The essentials build includes **libx264**, which is GPL-2.0. Combining it with
ffmpeg makes the resulting binary **GPLv3**, which is how gyan.dev licenses it.
The full text is in [COPYING.GPLv3.txt](COPYING.GPLv3.txt).

Redistributing it — which is what hosting it here is — carries two obligations,
both met by this folder:

1. **Convey the licence** with the binary: `COPYING.GPLv3.txt`, above.
2. **Offer the corresponding source.** It is available at no charge from:
   - ffmpeg itself — <https://github.com/FFmpeg/FFmpeg> (see `MANIFEST.txt` in
     the archive for the exact commit the build was made from)
   - libx264 — <https://code.videolan.org/videolan/x264>
   - the build scripts and package sources gyan.dev used —
     <https://github.com/GyanD/codexffmpeg> and the "source code @ github" links
     on the builds page above

`vmerge` itself invokes `ffmpeg.exe` and `ffprobe.exe` as separate processes and
does not link against them, so it is a separate work rather than a derivative of
ffmpeg. That is the same basis every tool that ships an ffmpeg binary relies on.
It is settled practice rather than settled law; if that distinction matters for
your use, take advice.

## Verifying and updating

The program checks the archive's SHA-256 against `EXPECTED_SHA256` in
`src/ffmpeg.rs` when it downloads from this mirror, so a corrupted or swapped
file is caught before anything is unpacked. Upstream is not hash-checked, because
its contents change with every ffmpeg release.

To refresh the mirror:

```
curl -L -o vendor/ffmpeg-release-essentials.7z \
  https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.7z
sha256sum vendor/ffmpeg-release-essentials.7z    # paste into EXPECTED_SHA256
```

Then commit both the archive and the new hash together, or setup will reject the
mirror and fall back to downloading from gyan.dev.
