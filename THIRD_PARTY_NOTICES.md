# Third-party notices

Capsule's original source code is licensed under MIT. A release may aggregate
separate open-source runtime components; those components retain their own
copyrights and licenses.

## Files stored in this repository

- `assets/fonts/windows-compat`: compatibility fonts under the SIL Open Font
  License 1.1 and the other terms described in that directory's `NOTICE.md`.
- `assets/dxvk/windows-compat`: unmodified DXVK 2.7.1 binaries under the zlib
  license. See its `NOTICE.md` and `LICENSE`.

## Components included in the one-file AppImage

The AppImage packaging workflow collects the following separate programs and
their runtime libraries from the build environment:

- Wine (LGPL-2.1-or-later)
- Gamescope (BSD-2-Clause)
- PipeWire, WirePlumber and the PulseAudio-compatible server (MIT)
- PulseAudio client libraries (LGPL-2.1-or-later)
- Xwayland and X.Org libraries (MIT-family licenses)
- Bubblewrap (LGPL-2.0-or-later)
- Sandwine (GPL-3.0-or-later)
- fuse2fs/e2fsprogs (GPL-2.0-or-later and LGPL components)
- FUSE 3 (GPL-2.0 and LGPL-2.1 components)
- slirp4netns and libslirp (GPL-2.0-or-later / BSD-3-Clause)
- nftables and its libraries (GPL-2.0-only and LGPL components)
- GTK, GLib and libadwaita (LGPL-2.1-or-later)
- GStreamer and the bundled plugin sets (LGPL-2.1-or-later, with individual
  plugin exceptions documented upstream)
- 7-Zip (LGPL-2.1-or-later with the upstream unRAR restriction where
  applicable)
- wl-clipboard (GPL-3.0-or-later)
- ImageMagick (ImageMagick License)
- curl and its protocol libraries (curl license and their respective licenses)
- Python and the PyInstaller-built Sandwine launcher (PSF-2.0 and
  GPL-2.0-or-later with PyInstaller's bootloader exception)
- GNU C Library compatibility runtime used only by Sandwine
  (LGPL-2.1-or-later)
- systemd and util-linux command-line helpers (LGPL-2.1-or-later and
  GPL-2.0-or-later components)

Their transitive shared libraries are bundled as well and retain their own
licenses.

The packaging script copies available upstream license files into the
AppImage under `usr/share/licenses`. Anyone redistributing a generated
AppImage is responsible for preserving those notices and satisfying the
corresponding source requirements of the included versions.
