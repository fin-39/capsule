# Capsule

[![CI](https://github.com/fin-39/capsule/actions/workflows/ci.yml/badge.svg)](https://github.com/fin-39/capsule/actions/workflows/ci.yml)
[![AppImage](https://github.com/fin-39/capsule/actions/workflows/appimage.yml/badge.svg)](https://github.com/fin-39/capsule/actions/workflows/appimage.yml)

Capsule is an experimental Linux launcher for running an untrusted Windows or Linux game/application in its own removable, configurable environment.

The MVP combines a Rust/GTK interface with Sandwine, Bubblewrap, Gamescope, optional Wine and a single sparse ext4 `.capsule` file. Capsule copies a portable Windows or Linux application from a folder or supported archive into that file and chooses its verified launcher. It can also register a compatible existing `.capsule` file without copying it. While the application is running, Capsule mounts the image in a private temporary runtime location; when it stops, the mount disappears. Moving a Capsule-owned file to Trash removes the application's contained state from Capsule.

> [!WARNING]
> Capsule is early-stage software and has **not** received a security audit. It reduces accidental host access and raises the cost of malicious behavior, but it does not make unknown software safe. The fast backend shares the host Linux kernel and, when enabled, the host GPU driver. Its host-side `fuse2fs` and Gamescope helpers are trusted, unsandboxed user processes; a vulnerability in either could expose your account. Do not use the container backend to analyze known malware.

## MVP goals

- Present installed capsules in a clean GTK 4/libadwaita library.
- Select a portable application folder, supported archive, or compatible existing `.capsule` and choose one validated Windows or Linux launcher; never run folder/archive content from `Downloads` or bind that source into the runtime sandbox.
- Keep the game, optional Wine prefix, contained application settings and saves in one sparse `.capsule` file at rest.
- Mount the image with `fuse2fs` only for the duration of a run.
- Launch native or Wine applications through Sandwine/Bubblewrap on Gamescope's per-run Xwayland display instead of exposing the host X11 display.
- Store authority-bearing permissions outside the capsule, where the contained application cannot edit them.
- Deny personal host files, network and desktop integration unless the user grants a narrowly described permission. Read-only host runtime libraries and hardware metadata remain visible.
- Fail closed when a mandatory isolation component is missing or a sandbox cannot be constructed. Capsule must never fall back to bare `wine`.
- Supervise the process tree, unmount the image and clean temporary runtime objects after the application exits.

The one-file promise applies to application state. Capsule itself still keeps a small, trusted launcher registry and permission records in the user's XDG application data/configuration. Temporary mountpoints and sockets exist under the user's runtime directory while a capsule is running.

Capsule also ships a shared open-source Windows compatibility font pack derived
from the fonts distributed with Proton. Wine capsules receive only that asset
directory as a read-only mount. During the hidden preparation phase Capsule
registers the pack in each prefix using a versioned marker, so existing
capsules migrate on their next launch without duplicating roughly 70 MiB of
fonts inside every image. The pack includes Windows-compatible Latin,
Japanese, Korean and Chinese family names; it contains no Microsoft
proprietary font data.

Wine capsules also use a shared, read-only DXVK 2.7.1 runtime by default.
DXVK translates Direct3D 8–11 to Vulkan and avoids WineD3D's CPU-heavy
Direct3D-to-OpenGL path. Prefixes contain only symlinks to Capsule's immutable
32-bit and 64-bit DLL assets, so the runtime is not duplicated in every
`.capsule` file. A per-capsule **Graphics** setting can force WineD3D for
compatibility, and Capsule falls back to WineD3D with a visible warning if the
shared DXVK pack is unavailable.

## Security model in one minute

The `.capsule` file is a storage format, not the security boundary. Isolation comes from the launch path:

```text
Capsule UI
  -> unprivileged supervisor
  -> fuse2fs temporary mount
  -> host-side private Gamescope broker
  -> Sandwine / Bubblewrap namespaces
  -> native application or Wine application
```

The contained program receives its capsule filesystem plus read-only host system libraries/configuration and only the devices or brokers allowed by its external permission policy. It does not receive the backing image file, the host home directory, the session/system D-Bus, SSH or GPG agents, or the host X11 socket.

Container mode cannot protect against a Linux kernel escape or a vulnerability in an exposed GPU driver. Native-speed graphics and maximum isolation are mutually competing goals. A KVM/QEMU backend with a separate guest kernel is planned for applications that require a stronger boundary.

See [the architecture document](docs/architecture.md) for the threat model, runtime lifecycle and permission design.

## One-file release

The public release format is one executable `Capsule-<version>-x86_64.AppImage`.
It bundles Capsule, GTK/libadwaita, Wine, Gamescope, Xwayland, Sandwine,
Bubblewrap, FUSE/e2fsprogs tools, the network helpers, 7-Zip, ImageMagick,
compatibility fonts and DXVK. Users do not install those packages separately:

```console
chmod +x Capsule-*-x86_64.AppImage
./Capsule-*-x86_64.AppImage
```

The one file cannot contain or replace the host kernel and desktop session. A
supported host therefore still needs:

- an x86_64 GNU/Linux system with user namespaces and a systemd user manager;
- a Wayland session, tested with KDE Plasma 6 and Niri (other standard Wayland
  compositors should work as well);
- working host GPU drivers and `/dev/dri`;
- kernel FUSE support with an accessible `/dev/fuse` device;
- PipeWire/Pulse services only when audio is enabled.

Those are host facilities, not application packages Capsule can safely bundle.
The AppImage deliberately uses the host GPU driver because a copied driver can
be incompatible with the running kernel.

### Adding an existing capsule

Open **Add**, select **Capsule…**, and choose a `.capsule` file previously
created by Capsule. The image is locked, mounted read-only, and checked for the
expected `prefix/drive_c/Game` layout. Capsule discovers the contained Windows
or Linux launchers without executing them, then lets you choose the launcher
and access preset before adding the entry.

The selected file is registered in place, so this operation does not duplicate
a potentially large image. Keep it at that path. Removing an existing-image
entry only unregisters it and leaves the file untouched; Capsule-owned images
created from a folder or archive still use **Move to Trash**. Registering the
same image more than once is rejected.

Release artifacts are produced by
[the AppImage workflow](.github/workflows/appimage.yml). The AppImage directory
and entrypoint follow the official AppImage layout, while Capsule's runtime
overrides keep every bundled helper relocatable.

Every push to `main` uploads a 14-day downloadable AppImage workflow artifact.
Pushing a version tag matching `Cargo.toml` (for example `v0.2.0`) also creates
a public GitHub Release automatically and attaches both the AppImage and its
SHA-256 file. The workflow can still be started manually without publishing a
release.

## Building from source

The following packages are needed by contributors building or running directly
from a source checkout. They are not required by someone using the AppImage.

Build requirements:

- Rust 1.92 or newer
- GTK 4.18 development files
- libadwaita 1.7 development files
- Xlib development files
- a C toolchain and `pkg-config`

Source-tree runtime requirements:

- `fuse2fs` and FUSE 3 (`fusermount3`)
- `mkfs.ext4` from e2fsprogs
- Bubblewrap (`bwrap`)
- [Sandwine](https://github.com/hartwork/sandwine)
- `slirp4netns` and nftables (`nft`) for Internet-only networking
- [Gamescope](https://github.com/ValveSoftware/gamescope)
- Xwayland
- a Wine runner suitable for imported Windows applications
- a Vulkan-capable graphics driver for the default DXVK Wine backend
- the full `/usr/lib/7zip/7z` engine and `7z.so` plugin for archive imports
- ImageMagick (`/usr/bin/magick`) for optional executable-icon thumbnails
- PipeWire-Pulse and WirePlumber 0.5 for the playback-only audio permission
- `curl` for downloading Valve's official Windows Steam installer on demand

Optional permissions can require additional host components. Capsule should detect these before launching and explain exactly what is missing. In particular, a missing `fuse2fs` is a hard error: the MVP does not kernel-mount the untrusted image and does not extract it into a persistent host directory as a fallback.

ICMP-based game connectivity checks additionally require the launcher user's
primary group to be included in the host's `net.ipv4.ping_group_range`.
Capsule permits only the nested sandbox's group `0` in its private network
namespace; it does not grant raw sockets or bypass the Internet-only nftables
policy. A host administrator can persist a single-user setup (replace `1000`
with the output of `id -g`) as follows:

```console
sudo sh -c 'printf "%s\n" "net.ipv4.ping_group_range = 1000 1000" > /etc/sysctl.d/90-capsule-ping.conf'
sudo sysctl --system
```

On Arch Linux and derivatives, the contributor dependencies are available from
the distribution:

```console
sudo pacman -S --needed base-devel rust gtk4 libadwaita e2fsprogs fuse2fs fuse3 bubblewrap gamescope xorg-xwayland wine python 7zip imagemagick slirp4netns nftables curl
```

Capsule auto-detects Sandwine in a project-local virtual environment. This is the setup used for the current MVP:

```console
python -m venv .venv
.venv/bin/pip install sandwine==8.0.1
```

Alternatively, install Sandwine using its upstream-supported package for your distribution. Do not install security-sensitive helpers from an unverified binary source.

### Build and run

From the repository root:

```console
cargo build
cargo test
cargo run -- --doctor
cargo run
```

The equivalent convenience targets are:

```console
make check
make test
make release
```

For an optimized local build:

```console
cargo build --release --bins --lib
target/release/capsule
```

To create the same one-file artifact used for releases, provide the official
`appimagetool` executable and run:

```console
APPIMAGETOOL=/absolute/path/to/appimagetool-x86_64.AppImage make appimage
```

The result and its SHA-256 file are written to `dist/`. The packaging script
uses an isolated Python environment for the pinned Sandwine/PyInstaller build
tools and copies available third-party license files into the artifact.

Development builds find the compatibility fonts under
`assets/fonts/windows-compat`. A packaged installation should install that
directory unchanged at `/usr/share/capsule/fonts/windows-compat`, including
`NOTICE.md` and `LICENSE.OFL.txt`.

Development builds find DXVK under `assets/dxvk/windows-compat`. A packaged
installation should install that directory unchanged at
`/usr/share/capsule/dxvk/windows-compat`, including both architecture
directories, `NOTICE.md` and `LICENSE`.

The AppImage can install its playback-only audio policy once for the current
user and reload the user audio services itself:

```console
./Capsule-*-x86_64.AppImage --install-audio-integration
```

It writes only Capsule's three named policy fragments, refuses to overwrite a
modified fragment, and does not require administrator privileges. Contributors
running from a source checkout can install the same embedded policy with
`cargo run -- --install-audio-integration`, or install the files manually:

```console
install -Dm644 assets/pipewire/60-capsule-playback.conf ~/.config/pipewire/pipewire-pulse.conf.d/60-capsule-playback.conf
install -Dm644 assets/pipewire/60-capsule-playback-sink.conf ~/.config/pipewire/pipewire.conf.d/60-capsule-playback-sink.conf
install -Dm644 assets/wireplumber/60-capsule-playback.conf ~/.config/wireplumber/wireplumber.conf.d/60-capsule-playback.conf
systemctl --user restart pipewire.service pipewire-pulse.service wireplumber.service
```

The project targets compositor-neutral Wayland APIs and does not depend on a
Niri configuration. KDE Plasma Wayland uses its normal KWin placement and
focus behavior. Niri currently needs one narrowly scoped optional focus
workaround because Gamescope does not request xdg-activation; that workaround
is activated only when `XDG_CURRENT_DESKTOP` contains `niri`. Gamescope creates
one private Xwayland display per run and fits the selected game surface without
force-resizing fixed-size Windows clients. The `capsule-xwayland` helper
disables MIT-SHM on that display because the contained process has a separate
IPC namespace. When Wine virtual-desktop compatibility is enabled, the trusted
`capsule-window-center` sidecar centers stable, visible Win32 children and later
dialogs inside that desktop without resizing them; it is bound to the Sandwine
process lifetime. Wine receives only the per-run X socket; the desktop X11
socket is not passed through. The outer Gamescope surface follows the host
compositor's normal placement and tiling policy.

Clipboard sharing is off by default. Capsule blocks Gamescope's host clipboard protocols in that state. Enabling Clipboard permits two-way UTF-8 text transfer: Gamescope exports copied game text, while the trusted private-display sidecar imports a bounded host text selection. The contained Wine process still does not receive the host Wayland socket.

## Intended workflow

1. Choose an extracted portable Windows/Linux application folder or an archive. The picker accepts every format recognized by the installed full 7-Zip engine, including ZIP/ZIPX, 7z, RAR/RAR5, CAB, ARJ, LZH/LHA, TAR and compressed TAR, WIM, ISO, XAR and CPIO.
2. Capsule inspects the source without running it and lists verified `MZ` `.exe`, shebang `.sh` and ELF AppImage launchers. Dual-platform packages prefer their Linux launcher. Choose the program to start.
3. Capsule sizes a sparse ext4 image from the validated uncompressed payload, leaves bounded room for filesystem metadata and saves, and writes the payload directly into its fixed private game directory. The temporary directory used during import is only the image's mount point, not a second persistent copy.
4. Capsule extracts a Windows icon, when available, in a separate no-network metadata sandbox and caches a display-safe PNG outside the capsule. If this optional step fails, the library uses a generic icon.
5. Review permissions. Network and host-file access begin disabled. Audio has separate Playback and Microphone grants.
6. Start the capsule. For Wine applications, Capsule first completes first-run setup or a Wine-version prefix upgrade in a display-less Sandwine sandbox and waits for it to stop. Capsule then opens the selected native or Wine application through Sandwine and a private Gamescope window. Both runners receive a capsule-local `HOME` and XDG directories; Wine additionally receives a capsule-local `WINEPREFIX` that never overlays the real user's `~/.wine` path.
   A Wine capsule can optionally install the normal Windows Steam client from Valve's official installer. **Install or repair Steam** opens its standard setup/login flow; **Open Steam** reopens the contained client later. Enabling **Start Steam with this game** starts that client in the same prefix, filtered network namespace and private display, then waits for its current login and library initialization to finish before starting the game. Steam credentials and account state remain inside that capsule and are accessible to every application in it; Capsule never imports the host Steam session.
7. Exit the application. The transient service ends, and the supervisor unmounts the image and removes its private runtime directory.
8. Use Remove in Capsule to move the one-file capsule to the desktop Trash and unregister it.

For multipart input, keep every part together and unchanged until import finishes. Capsule resolves numbered `.001/.002/...` sets, modern `.part01.rar/.part02.rar`, legacy `.rar/.r00/.r01`, and classic `.z01/.z02/.zip` sets from any selected part. Import copies the payload into one `.capsule` file and does not delete or modify the original source.

## Permission philosophy

Permissions are grants from the launcher, not declarations trusted from inside an image. A capsule manifest may request a feature, but only the external policy can authorize it.

The initial profiles are intended to be:

- **Locked:** no network, audio, host files, raw input devices or direct GPU. It currently fails closed because a software-only private-display backend has not been implemented.
- **Offline game:** private display plus direct GPU rendering; network, audio and controllers remain off. This is the default runnable profile.
- **Online game:** the Offline game profile plus outbound Internet access through a private `slirp4netns` network. Host loopback, link-local, private/LAN ranges and inbound forwarding remain blocked by nftables.
- **Custom:** individual controls with warnings for high-risk grants.

Direct access to the host home directory, host X11, session/system D-Bus, `/dev/input`, `/dev/bus/usb` or all devices is not an ordinary permission and should not be offered as a convenience toggle.

## Current limitations

- The project is an honest MVP, not a security product or malware-analysis laboratory.
- A shared kernel means a kernel vulnerability can cross the container boundary.
- Direct GPU access exposes a large kernel-driver ioctl surface.
- Gamescope, Xwayland, Capsule's private-display helpers, Wine, `fuse2fs` and network/audio helpers become part of the trusted computing base to the extent that they bridge the capsule to the host.
- Audio starts disabled. Playback-only uses Capsule's restricted PipeWire-Pulse socket; enabling Microphone uses Sandwine's broader native Pulse socket and therefore grants playback and recording. Per-application playback volume remains under the user's normal session-manager control during a run. Restricted streams start at unity gain instead of inheriting WirePlumber's saved per-application volume, preventing a stale zero-volume entry from muting recreated Wine streams.
- Clipboard sharing supports UTF-8 text up to 1 MiB. Files, images, rich clipboard formats and primary-selection import are not bridged.
- Portable folder import uses fd-relative `openat2` traversal and rejects source mutation, symlinks, hardlinks, mount crossings and special files. Ordinary ZIP import uses a bounded Rust parser with strict Windows-path and collision checks.
- Archive input selected in the UI is parsed by the fixed full `/usr/lib/7zip/7z` executable inside a narrow Bubblewrap worker. It receives the already-open archive parts read-only, a minimal runtime, no network, and the import destination only while extracting. Compressed TAR input is streamed between two such workers instead of materializing the inner TAR on the host. The import coordinator still runs in Capsule's trusted process; do not use Capsule as a malware-analysis tool.
- Each archive worker has a 2 GiB address-space limit, at most 16 processes and a 256 MiB temporary filesystem. It does not yet have a cgroup-wide aggregate memory ceiling.
- Internet-only networking uses a separate rootless namespace and blocks host loopback, link-local and private/LAN destinations. Enabling both Internet and LAN uses Sandwine's host-network grant and is intentionally broader. LAN-only and custom-endpoint policies still fail closed; controller forwarding is also not implemented.
- Capsule supports the regular-file archive formats recognized by the installed full 7-Zip engine, including multipart 7z, RAR and ZIP naming conventions. When an archive is password-protected, the Add page requests its password and sends it to the isolated archive worker over standard input; the password is not placed in process arguments or saved in Capsule metadata. Arbitrary standalone installers are not supported. The Settings page has one narrowly scoped exception for Valve's official Windows Steam installer; Capsule downloads it over HTTPS, validates that the response is a bounded PE executable and copies it below `drive_c/Capsule/Installers` without changing the saved game entrypoint.
- Image capacity is chosen at import time from the validated payload size. Automatic growth and recovery are not implemented yet.
- Some anti-cheat and DRM systems will not work in Wine, namespaces or virtual machines.
- FUSE and a nested compositor add some overhead. It should normally be small for games, but “zero performance loss” is not a defensible guarantee.
- Deleting a Capsule-owned image is logical removal, not forensic erasure from SSD history, swap, backups or filesystem snapshots. Existing images registered in place are only unregistered.
- A crashed or forcibly killed supervisor can leave a stale FUSE mount; automatic ownership-checked recovery and an in-app Stop action are not implemented yet.
- Gamescope 3.16.24 can fault while tearing down its private Xwayland display on the current NVIDIA stack after the contained application has exited. Capsule disables core-file creation for the transient service and still waits for teardown before unmounting, but the helper failure may be reported after an otherwise successful run.

## Roadmap

- [x] Add direct portable-folder/archive import, executable discovery, a GTK library, one-file image storage and the offline Wine launch flow.
- [x] Add trusted-system-path executable discovery and missing-tool diagnostics with fail-closed behavior.
- [x] Add exclusive image locking and external permission profiles.
- [x] Add cgroup v2 memory/process limits to transient user services.
- [x] Add a portable game-directory/archive workflow that creates a fresh prefix without exposing the download at runtime.
- [x] Add bounded ZIP validation and race-resistant dirfd traversal with mount-boundary checks.
- [x] Add full-engine archive import, multipart 7z/RAR/ZIP resolution and compressed-TAR streaming through resource-limited Bubblewrap workers.
- [x] Add read-only inspection and in-place registration for compatible existing capsule images.
- [ ] Move the remaining import coordination into a separate worker.
- [ ] Add a contained standalone-installer workflow and post-install executable selection.
- [ ] Add configurable/bounded sparse-image growth, `e2fsck` recovery and safe stale-mount cleanup.
- [ ] Add a clear pre-launch effective-permission summary and an in-app Stop action.
- [ ] Add functional doctor probes for versions, user namespaces, FUSE, Wayland and the user systemd manager.
- [x] Isolate PE icon parsing and conversion from the main UI process.
- [x] Add a PipeWire/WirePlumber playback-only audio broker with a private sink monitor and no host capture sources.
- [x] Add rootless Internet-only networking with private/LAN destination filtering and no inbound forwarding.
- [ ] Add a selected-controller broker instead of exposing broad host devices.
- [ ] Add Landlock defense in depth and tested Wine-aware seccomp profiles.
- [ ] Add snapshots with “commit run” and “discard run” actions.
- [ ] Add explicit copy-in/copy-out workflows without sharing host directories.
- [x] Add contained native Linux launcher detection and execution for shell launchers and AppImages.
- [ ] Add an optional rootless KVM/QEMU backend with no shared folders, clipboard, USB or network by default.
- [ ] Commission an independent security review before making stronger security claims.

## License

Capsule's original source code is available under the permissive [MIT
License](LICENSE). You may use, modify, redistribute and sell it subject to
that short license notice.

Bundled third-party components are not relicensed. The compatibility fonts
remain under their upstream font licenses, and DXVK remains under the zlib
license. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) and the license
files shipped beside those assets.
