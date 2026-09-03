# Capsule architecture

This document describes the current Windows/Linux MVP and the security properties it should preserve as it grows. It is a design document, not a security audit or a claim that every described hardening measure is already implemented.

The current `0.2.0` implementation has bounded portable-folder/archive import, read-only inspection and in-place registration of compatible existing images, sparse ext4 image creation, rootless FUSE mounting, an external locked JSON policy store, advisory image locking, transient-user-service resource limits, native/Wine execution through Sandwine/Bubblewrap, a private Gamescope display, rootless Internet-only networking, playback-only audio, opt-in text clipboard brokering and a narrowly scoped official Steam-installer workflow. Archive parsing runs in narrow full-7-Zip workers, while import coordination remains in Capsule's trusted process. It deliberately blocks controller access and the no-GPU profile. General standalone-installer ingestion, LAN-only/custom-endpoint filters, a separate worker for the remaining importer, sandboxing host-side FUSE/Gamescope helpers, stale-mount recovery, an in-app Stop action, snapshots and the VM backend remain design targets.

The release artifact is a relocatable x86_64 AppImage. Its read-only AppDir
contains Capsule's user-space runtime and exposes every authority-bearing helper
through a validated absolute environment override. Long-running supervisors
restart the outer AppImage rather than borrowing the UI process's temporary
mount, so closing the library cannot invalidate a running service's runtime.
The artifact does not attempt to bundle the kernel, host compositor, systemd
user manager, FUSE device or GPU driver.

## Design principles

1. **Fail closed.** Missing tools, unsupported host features, malformed images and teardown failures produce visible errors. They never cause a fallback to an unsandboxed process.
2. **One persistent application file.** An installed application, Wine prefix and contained data live in one sparse `.capsule` image. Runtime mountpoints and sockets are temporary.
3. **Storage is not isolation.** The capsule image provides packaging and persistence; Bubblewrap namespaces and carefully brokered interfaces provide containment.
4. **The capsule cannot grant itself authority.** Executable selection, permission grants and runner policy are stored in launcher-controlled state outside the image.
5. **Copy, do not share.** Imports are copied into a capsule. Exports are deliberate copies out. A host directory is never mounted merely because an executable originally came from it.
6. **No privileged daemon.** The normal fast backend runs entirely as the logged-in user using unprivileged user namespaces and FUSE.
7. **Make tradeoffs visible.** Enabling network, audio, controllers or direct rendering changes the attack surface. The UI must say so before launch.

## Threat model

Capsule treats the imported executable, all files shipped with it, generated Wine-prefix content, manifests, icons and saved state as attacker-controlled.

The fast backend aims to prevent that attacker from:

- reading or modifying files outside its capsule;
- discovering host user files through accidental bind mounts;
- communicating with host services over session/system D-Bus or inherited Unix sockets;
- inspecting or signalling processes outside its PID namespace;
- using the host X11 server to observe or control other windows;
- reading the global keyboard through raw input devices;
- using the microphone, camera, USB devices or LAN without a grant;
- persisting outside the capsule image;
- escaping merely by spawning child processes after the visible game window exits.

The container backend does **not** claim to defend against:

- a Linux kernel or host GPU-driver exploit;
- vulnerabilities in a deliberately exposed broker protocol;
- hardware, firmware or microarchitectural attacks;
- all denial-of-service attacks, including GPU hangs;
- forensic recovery from swap, backups, SSD behavior or host filesystem snapshots;
- software already known to be malware.

It also cannot fully conceal hardware identity, timing, kernel version or all other evidence that the program is running on the host. A virtual machine is required for a materially stronger kernel boundary, and even a VM is not absolute.

## Trusted computing base

The initial trusted computing base includes:

- the Linux kernel and enabled LSMs;
- the Capsule UI and supervisor;
- Bubblewrap and the policy construction performed by Sandwine/Capsule;
- the FUSE stack and the per-capsule `fuse2fs` helper;
- Gamescope, its per-run Xwayland process and the small `capsule-xwayland` argument adapter;
- any enabled audio, network or controller broker;
- the selected host graphics driver when direct rendering is enabled.

Wine and the imported application run inside the containment boundary. Wine should not receive ambient host authority simply because it is installed by the host.

Bubblewrap is deliberately a low-level mechanism rather than a complete policy. Capsule is responsible for constructing and testing the policy supplied to it. Sandwine is useful policy-oriented integration for Wine, but neither Sandwine nor Capsule has undergone an independent audit for this use case.

## Component model

```text
┌────────────────────────────────────────────────────────────┐
│ Trusted host side                                          │
│                                                            │
│  GTK/libadwaita UI                                         │
│          │                                                  │
│  unprivileged supervisor ─── external permission store     │
│          │                                                  │
│          ├── fuse2fs helper ── capsule image               │
│          ├── Gamescope/display broker                      │
│          ├── PipeWire/WirePlumber audio broker             │
│          └── slirp4netns + nftables policy helper          │
└──────────┼─────────────────────────────────────────────────┘
           │ private mount and broker sockets only
┌──────────▼─────────────────────────────────────────────────┐
│ Sandwine / Bubblewrap containment                          │
│                                                            │
│  trusted runtime (read-only)                               │
│  capsule filesystem (persistent read/write)                │
│  private /tmp, /run, /dev and /proc                        │
│  native or Wine runner → application and descendants       │
└────────────────────────────────────────────────────────────┘
```

The supervisor owns lifecycle and cleanup. The GTK process should not directly parse complex untrusted filesystem content or supervise Wine descendants.

## Capsule storage

### MVP format

The MVP uses one regular sparse file containing an ext4 filesystem. Sparseness provides a large logical capacity without allocating the full size at creation. Apparent size and allocated disk usage therefore differ, and the UI should show both.

The implemented filesystem layout is:

```text
/.capsule/manifest.json     untrusted descriptive metadata
/prefix/                   private Wine prefix and contained application state
/prefix/drive_c/Game/      validated portable import payload
/prefix/drive_c/Capsule/Installers/  trusted installers copied by Capsule
/home/                     reserved contained user data
/logs/                     reserved capsule-local logs
```

The manifest can describe a title, icon, candidate executable and requested features. It cannot authorize network, devices, host paths or other permissions. Authority-bearing settings live in Capsule's external XDG state and are keyed by a stable capsule identifier. The launcher must validate all manifest fields, paths and sizes before displaying or using them.

Portable import places source content only below `/prefix/drive_c/Game`. Folder traversal is fd-relative and rejects symlinks, hardlinks, mount crossings, special files and concurrent mutation. Archive import applies strict entry, size, depth, compression-ratio and Windows-path limits and rejects colliding and link-like inputs. Password-protected archives require an ephemeral password before their encrypted entries are accepted.

The UI routes archive input to the fixed full `/usr/lib/7zip/7z` engine inside a resource-limited Bubblewrap worker. The backend resolves contiguous numbered 7z/ZIP sets, modern and legacy multipart RAR sets, and classic split ZIP sets from any selected part. The worker receives only already-open, read-only archive-part descriptors, a minimal runtime, private namespaces, no network and bounded time, memory, process count and output. Its extraction run also receives the otherwise empty import destination as writable. Compressed TAR formats are decoded as a bounded pipe between two workers so the inner TAR is never materialized beside the capsule. Archive passwords are held only for the Add operation, redacted from debug output, sent to the worker over standard input and never persisted or exposed in its command line.

Launcher discovery recognizes regular `.exe` files beginning with `MZ`, shebang `.sh` scripts and ELF AppImages; import never executes them. A native launcher is preferred when a package supplies both platforms. The source folder or archive parts remain unchanged. Validated content is copied into one final `.capsule` file; Capsule does not delete the separate original input.

### Mounting

The supervisor opens and exclusively locks the image, then starts `fuse2fs` to present it below a private directory such as:

```text
$XDG_RUNTIME_DIR/capsule/<run-id>/root
```

The exact path is an implementation detail and must use a freshly created, mode-0700 directory owned by the current user. The contained process sees the mounted filesystem, not the host path of the `.capsule` file and not the raw backing-file descriptor.

The MVP intentionally does not attach the file to a kernel loop device or ask a privileged disk service to mount it. Parsing ext4 in userspace avoids giving the kernel ext4 parser the image. In `0.2.0`, however, `fuse2fs` is an ordinary process running with the logged-in user's host authority; a compromise of that trusted helper is not contained by Capsule. It should eventually run in a small sandbox with only:

- the already-open capsule FD;
- its FUSE connection;
- its private mountpoint; and
- no network, D-Bus, host home or unrelated inherited descriptors.

If `fuse2fs` is absent, FUSE is unavailable, the exclusive lock fails, or the mount cannot be verified, launch stops. Extracting into a normal persistent directory or invoking Wine against the source files is not an acceptable fallback.

### Persistence and removal

Application writes go back to the ext4 image. The image must have a configured capacity; the guest should receive an ordinary out-of-space error rather than being allowed to consume unbounded host storage. Only one run may hold a writable image at a time.

Deleting the file removes the game, prefix and contained data from Capsule's application library. Capsule should also remove its corresponding launcher record, permission policy and nonessential cache. This is logical deletion rather than a secure-erase guarantee.

The image format can later evolve to an immutable, content-addressed base plus a journaled writable layer while preserving one-file-at-rest behavior and stable import/export APIs.

## Runtime lifecycle

### 1. Preflight

The intended preflight is listed below. The current implementation validates trusted absolute executable paths and capsule-relative paths, fails closed on missing tools/unsupported permissions, and discovers some host failures only when the relevant operation starts.

Before touching the image, the supervisor should:

1. resolves the selected capsule by stable identifier;
2. reads the external permission policy;
3. verifies required executable paths and minimum versions;
4. checks unprivileged user namespaces, FUSE and the expected Wayland environment;
5. confirms the requested GPU/audio/network features can be constructed narrowly; and
6. allocates a private runtime directory.

Mandatory MVP executables include `fuse2fs`, `bwrap`, `sandwine`, `gamescope`, `Xwayland`, the `capsule-xwayland` adapter and the `capsule-window-center` private-display helper. Internet-only entries additionally require `capsule-network`, `slirp4netns` and `nft`; Windows entries require the selected Wine runner. Dependency discovery must use trusted absolute paths or a sanitized search path. It must not accept host helpers supplied from inside the capsule.

### 2. Open and mount

The current supervisor rejects an observed symlink/non-regular image and obtains an exclusive advisory lock before mounting. A race-free `openat2`/`O_NOFOLLOW` path plus ownership and post-open identity verification remains to be implemented. A capsule must not be moved, replaced or launched concurrently while this lock is held.

### 3. Construct containment

The launch backend uses Sandwine to construct a Bubblewrap environment for native and Wine runners. The effective policy should include:

- new user, mount, PID, IPC, UTS and network namespaces;
- a new session and death with the supervisor;
- a private `/proc` associated with the new PID namespace;
- minimal synthetic `/dev`, `/tmp`, `/run`, `/etc` and home content;
- a trusted runtime mounted read-only;
- the capsule filesystem as the only persistent writable tree;
- no host home, removable-media paths or host runtime directory;
- no session/system D-Bus, SSH agent, GPG agent, keyring or host X11 socket;
- a cleared and reconstructed environment;
- no unexpected inherited file descriptors; and
- all capabilities dropped with `no_new_privs` before executing the application.

The actual Bubblewrap argument vector is security-sensitive code. It should be generated from typed policy objects, logged in a redacted diagnostic form, and covered by tests that inspect the child's mount table, namespaces, sockets and device view.

Landlock filesystem/socket restrictions and a Wine-compatible seccomp policy are defense-in-depth roadmap items. They must not be advertised as active until the supervisor applies and verifies them. Seccomp must cover both 64-bit and 32-bit syscall ABIs and cannot be so narrow that developers routinely disable it to run games.

### 4. Display and execute

Capsule starts a fresh nested Gamescope compositor and one Xwayland server for each run. The desktop `DISPLAY` is replaced with a nonexistent sentinel before Gamescope starts; Gamescope must replace it with its per-run display or Sandwine fails on a missing socket. Sandwine binds only that X socket into the container, so Wine's X11 driver never receives the host X11 socket. The host-facing side uses standard Wayland variables and works under KDE Plasma/KWin and Niri; only Niri's missing Gamescope activation receives a compositor-specific, best-effort focus action.

Gamescope uses fit scaling and does not force inner windows to the nested display size. That distinction matters for fixed-size GDI applications: resizing their outer window can leave a large unpainted client area and break input coordinates even though the application continues drawing at its original resolution. A per-capsule Wine virtual-desktop option keeps old fixed-size applications and their modal dialogs inside one persistent Wine surface; this is a compatibility option rather than a permission boundary. Wine represents Win32 top-levels as direct X children of that surface, so the trusted `capsule-window-center` sidecar waits for each visible child to stabilize and moves it once to the center without changing its size. It connects only after Gamescope replaces the nonexistent `DISPLAY` sentinel, has a parent-death signal tied to the exec'd Sandwine process and exits if its expected parent changes. The outer Gamescope surface is left to the host compositor's normal placement, tiling and focus policy.

Bubblewrap gives Wine a private IPC namespace. Xwayland's MIT-SHM extension is therefore disabled by the trusted `capsule-xwayland` adapter; otherwise Wine can create a System V shared-memory segment that the host-side X server cannot access. Ordinary X11 transport and direct-rendering devices remain available through the private display path.

This prevents an X11 application from sharing an X server with desktop applications. It also gives Capsule one host window to associate with one run. When Clipboard is off, a Gamescope-only registry guard hides the host data-device and primary-selection protocols so private selections cannot be exported. When Clipboard is on, Gamescope exports private X11 text selections and the trusted sidecar imports at most 1 MiB of UTF-8 text through `wl-paste` into the private X server. Wine never receives the host Wayland socket. Screencopy, virtual-input and other privileged protocol bridges remain disabled.

Gamescope is still a broker that talks to the host compositor and GPU. In `0.2.0` it runs host-side with a sanitized set of session variables, but it is not itself filesystem-sandboxed. The two small private-display adapters are trusted host processes as well. A future version should reduce their host files, D-Bus and network access to what presentation strictly needs. Gamescope, Xwayland and those adapters remain part of the trusted computing base.

### 5. Supervise and stop (target)

The current implementation places the run in a transient systemd user service and waits for it before unmounting. Before a Wine game becomes visible, the supervisor compares Wine's `.update-timestamp` with the installed `wine.inf` revision and, when needed, runs `wineboot` in a separate display-less Sandwine service. It stops and waits for that preparation-only Wine server before starting Gamescope, preventing first-run or version-upgrade dialogs from taking the game's initial launch. Fixed, quoted in-sandbox wrappers give both runners an explicit capsule-local `HOME` and XDG directories and select the working directory. Wine additionally receives a capsule-local `WINEPREFIX`, and its wrapper terminates Wine background services after the application exits. The capsule root is bind-mounted only at its supervisor-owned runtime path; it is never overlaid on the desktop user's `~/.wine`, while Sandwine masks the real home with tmpfs. Normal application exit therefore cleans the mount. Core-file creation is disabled for the transient service. There is not yet an in-app Stop control or a complete recovery path if the supervisor itself is killed.

Wine Direct3D applications default to Capsule's shared DXVK runtime. Capsule
mounts the versioned 32-bit and 64-bit DLL asset directory read-only and
places only architecture-correct symlinks in the prefix. Native-first,
built-in-second Wine overrides retain loader-level WineD3D fallback, while a
per-capsule compatibility setting can force Wine's built-in D3D
implementation. Missing or invalid DXVK assets do not broaden the sandbox:
the launch stays contained, uses WineD3D and reports the performance fallback.

Wine settings also expose a narrowly scoped in-capsule Steam workflow. Capsule
downloads the current Windows bootstrap only from the HTTPS target of Valve's
official Install Steam page, bounds and validates the PE response, then copies
it through an ownership-checked image mount. The setup and **Open Steam**
utility launches wait for the complete Wine server because Steam replaces its
bootstrap process while updating. A per-capsule external setting can start the
installed client silently before the selected game in the same Wine prefix,
private display and network policy. The game launch ignores login results left
in old log data and waits for Steam's current login state machine to complete
account and library initialization; a bounded timeout fails closed instead of
racing one-shot Steamworks initialization. The host Steam installation, token
and IPC pipe are never mounted. This deliberately means the contained game can
access the credentials and account session stored in its own prefix, so the UI
describes that grant explicitly.

The target supervisor tracks the complete process tree, not just the initially selected executable. Closing the visible window does not prove that the capsule has stopped. Stop should perform an orderly termination, wait for a bounded interval, terminate remaining descendants, flush the FUSE filesystem, unmount it and remove only that run's verified runtime directory.

Cleanup errors remain visible. Capsule must not silently mark an image idle while a process can still write it or a mount remains active. A later startup may recover stale objects only after verifying ownership, expected path shape and the absence of live owning processes.

## External permissions

The permission store is outside the image because data controlled by the contained application cannot safely determine its own authority. An internal manifest can make a request; the UI resolves that request into an external grant.

Suggested controls and secure defaults:

| Resource | Default | Safer implementation |
|---|---:|---|
| Capsule filesystem | Read/write | The only persistent writable tree |
| Host files | None | Explicit copy-in/copy-out while stopped |
| Network | Off | Separate network namespace and policy helper |
| Display | Private | Per-run Gamescope and Xwayland |
| GPU | Off in Strict | Bind selected DRM render node in Gaming |
| Audio | Off | Playback-only socket; microphone is a separate grant |
| Keyboard/mouse | Private display only | Never expose raw `/dev/input` |
| Controller | None | Select and proxy one device |
| Camera/USB/Bluetooth | Off | No broad device passthrough |
| Clipboard | Off | Opt-in bounded text broker; no host socket inside Wine |
| In-capsule Steam session | Off | Separate Windows Steam installation and login stored inside one capsule |
| D-Bus and desktop agents | None | Avoid; narrowly filtered proxy only if required |

Permissions should be summarized before each run. Presets are conveniences that compile into the same explicit policy; they must not bypass individual validation.

Playback-only audio uses a dedicated Pulse-compatible PipeWire socket. Its
WirePlumber permission manager hides every physical sink, microphone and
unrelated output monitor. The socket exposes one Capsule virtual sink and its
private monitor, and pins both playback streams and Wine's capture probe to
that pair. This matters because Wine may abandon audio initialization when a
record stream is rejected outright. The monitor contains only audio sent to
the private Capsule sink, not host microphone input or unrelated application
audio. Restricted playback streams opt out of WirePlumber's per-application
stream-property restore and keep the host-side game stream at unity gain,
preventing both stale saved values and repeated client-side zero-volume
updates from muting playback. The game's own mixer still controls the samples
sent through that stream. Desktop-side volume is controlled by the Capsule
Playback loopback stream or the selected output device.
Enabling Microphone instead uses Sandwine's ordinary Pulse socket and
therefore explicitly grants both playback and recording.

## Performance/security boundary

### Strict profile

The strict profile has no direct GPU, network, audio or raw device access. Software rendering is compatible with the strongest container boundary but is unsuitable for many 3D games.

### Gaming profile

The gaming profile may expose a selected DRM render node and trusted userspace driver libraries. This keeps the normal rendering path near native and avoids a full VM boundary. It also gives malicious code access to a large GPU ioctl surface in the shared host kernel. Read-only bind mounts do not make device ioctls harmless.

On AMD and Intel, the render node is generally the narrowest useful device exposure. Proprietary NVIDIA drivers can require additional device nodes, increasing the attack surface. The UI should show the resolved nodes rather than representing “GPU” as a harmless boolean.

Gamescope and FUSE add some cost. The project can aim for low overhead but must not promise literally lossless performance.

## Audio, input and networking

The MVP can begin with these features disabled and add them as separately tested brokers.

Directly exposing the normal PulseAudio/PipeWire socket grants more than playback and can include capture and device enumeration. The intended audio design gives the capsule a private Pulse-compatible endpoint backed by an output-only host stream. Microphone capture is a different permission with a visible per-run indication. `/dev/snd` is not exposed.

Keyboard and pointer events travel through the private compositor, which avoids global raw input. Controller support should proxy a user-selected controller. Binding all of `/dev/input`, `/dev/hidraw` or `/dev/bus/usb` is not an acceptable shortcut.

With network disabled, Sandwine's new network namespace contains only private loopback. Internet-only mode creates an outer user/network/mount namespace, attaches `slirp4netns` without an API or inbound forwarding socket, supplies its synthetic DNS resolver and installs an nftables output policy before Sandwine starts. The helper also permits ICMP datagram sockets for group `0` in that private network namespace, matching Sandwine's nested identity; this does not grant raw sockets, and slirp's real host group must separately be present in the host administrator's `net.ipv4.ping_group_range`. The policy permits public IPv4 destinations while rejecting host loopback, link-local, private/LAN, carrier-grade NAT, multicast and other special-use ranges; non-loopback IPv6 is rejected because this backend does not enable slirp IPv6. Sandwine then creates a nested user namespace, so the application cannot administer the parent network namespace or remove the filter. Enabling both Internet and LAN is a separate high-risk grant that shares the host network namespace. LAN-only and custom-endpoint policies still fail closed.

## Resource control

Namespace isolation does not prevent resource exhaustion. The MVP applies configurable systemd `MemoryMax` and `TasksMax` properties to each transient run. Optional CPU/I/O limits, file-descriptor/core-dump rlimits and verified kill-all handling remain follow-up work.

These controls reduce fork bombs and memory exhaustion; they do not reliably contain GPU memory use, every kernel resource or a GPU hang.

## Future VM backend

A second backend should reuse the same library, permission vocabulary and one-file experience while running the workload under rootless KVM/QEMU with a separate guest kernel.

The secure VM default should include:

- no shared host directories;
- no clipboard integration;
- no USB, camera, microphone or controller passthrough;
- no network unless explicitly enabled;
- a minimal device model and read-only trusted firmware;
- no host management sockets exposed to the guest; and
- QEMU itself confined as an unprivileged host process.

The VM framebuffer can appear as one ordinary Capsule window. Virtio-gpu acceleration is a performance/security compromise because a host renderer still parses guest graphics commands. Dedicated VFIO GPU passthrough can approach native performance but requires an IOMMU, a spare GPU and careful hardware reset handling, so it cannot be the default consumer workflow.

The UI must describe the backends accurately: **Fast container, shared kernel** and **Stronger VM, separate kernel**. Neither should be labelled “safe” without qualification.

## Security development roadmap

1. Complete and test fail-closed dependency discovery, image creation, FUSE mounting, locking and teardown.
2. Test the concrete Bubblewrap filesystem, namespace, environment, socket and device view from inside hostile fixture programs.
3. Separate manifest/icon parsing from the GTK process and fuzz every capsule-controlled parser.
4. Add cgroup v2 limits and kill-all lifecycle management.
5. Add Landlock after all intentional descriptors and mounts are established.
6. Develop and test multi-architecture seccomp profiles against representative Wine, Proton and native applications.
7. Implement a selected-controller broker.
8. Add transactional snapshots, commit/discard and bounded rollback.
9. Design the KVM/QEMU backend and offline copy-in/copy-out path.
10. Obtain an independent security review before strengthening public claims.

Any compatibility workaround that exposes host home, X11, D-Bus, the host network namespace or all devices should be treated as a security design change, not a routine bug fix.
