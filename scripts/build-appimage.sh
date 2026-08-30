#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
cd "$project_root"

if [[ $(uname -m) != x86_64 ]]; then
    echo "Capsule AppImage packaging currently supports x86_64 only" >&2
    exit 2
fi

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "AppImage build dependency is missing: $1" >&2
        exit 2
    fi
}

for command_name in cargo file ldd install python readelf; do
    require_command "$command_name"
done

appimagetool=${APPIMAGETOOL:-}
if [[ -z $appimagetool ]]; then
    appimagetool=$(command -v appimagetool || true)
fi
if [[ -z $appimagetool || ! -x $appimagetool ]]; then
    echo "Set APPIMAGETOOL to the official executable AppImage build tool" >&2
    exit 2
fi

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
if [[ -z $version ]]; then
    echo "Could not read the package version from Cargo.toml" >&2
    exit 2
fi

work_root=$project_root/target/appimage
appdir=$work_root/Capsule.AppDir
python_env=$work_root/python-env
pyinstaller_work=$work_root/pyinstaller
dist_dir=$project_root/dist
output=$dist_dir/Capsule-$version-x86_64.AppImage

rm -rf -- "$appdir" "$pyinstaller_work"
mkdir -p -- \
    "$appdir/usr/bin" \
    "$appdir/usr/lib/capsule" \
    "$appdir/usr/libexec/capsule" \
    "$appdir/usr/share/applications" \
    "$appdir/usr/share/icons/hicolor/scalable/apps" \
    "$appdir/usr/share/metainfo" \
    "$appdir/usr/share/licenses/capsule" \
    "$appdir/usr/share/capsule" \
    "$dist_dir"

cargo build --locked --release --bins --lib

install -Dm755 target/release/capsule "$appdir/usr/bin/capsule"
install -Dm755 target/release/capsule-network "$appdir/usr/libexec/capsule/capsule-network"
install -Dm755 target/release/capsule-window-center "$appdir/usr/libexec/capsule/capsule-window-center"
install -Dm755 target/release/capsule-xwayland "$appdir/usr/libexec/capsule/capsule-xwayland"
install -Dm755 target/release/libcapsule.so "$appdir/usr/lib/capsule/libcapsule.so"

install -Dm755 packaging/appimage/AppRun "$appdir/AppRun"
install -Dm644 packaging/appimage/io.github.fin_39.Capsule.desktop \
    "$appdir/usr/share/applications/io.github.fin_39.Capsule.desktop"
install -Dm644 packaging/appimage/io.github.fin_39.Capsule.metainfo.xml \
    "$appdir/usr/share/metainfo/io.github.fin_39.Capsule.appdata.xml"
install -Dm644 packaging/appimage/io.github.fin_39.Capsule.svg \
    "$appdir/usr/share/icons/hicolor/scalable/apps/io.github.fin_39.Capsule.svg"
install -Dm644 LICENSE "$appdir/usr/share/licenses/capsule/LICENSE"
install -Dm644 THIRD_PARTY_NOTICES.md "$appdir/usr/share/licenses/capsule/THIRD_PARTY_NOTICES.md"

ln -s usr/share/applications/io.github.fin_39.Capsule.desktop \
    "$appdir/io.github.fin_39.Capsule.desktop"
ln -s usr/share/icons/hicolor/scalable/apps/io.github.fin_39.Capsule.svg \
    "$appdir/io.github.fin_39.Capsule.svg"
ln -s io.github.fin_39.Capsule.svg "$appdir/.DirIcon"

cp -a assets/fonts "$appdir/usr/share/capsule/fonts"
cp -a assets/dxvk "$appdir/usr/share/capsule/dxvk"
cp -a assets/pipewire "$appdir/usr/share/capsule/pipewire"
cp -a assets/wireplumber "$appdir/usr/share/capsule/wireplumber"

copy_tool() {
    local name=$1
    local source
    source=$(command -v "$name" || true)
    if [[ -z $source || ! -x $source ]]; then
        echo "Runtime tool required for the AppImage is missing: $name" >&2
        exit 2
    fi
    install -Dm755 -T "$source" "$appdir/usr/bin/$name"
}

for tool in \
    wine wineserver wineboot winepath \
    gamescope Xwayland bwrap fuse2fs mkfs.ext4 \
    slirp4netns nft wl-paste curl magick timeout prlimit systemd-run systemctl script; do
    copy_tool "$tool"
done

if [[ ! -d /usr/lib/wine || ! -f /usr/share/wine/wine.inf ]]; then
    echo "The build host's Wine runtime is incomplete" >&2
    exit 2
fi
cp -a /usr/lib/wine "$appdir/usr/lib/wine"
cp -a /usr/share/wine "$appdir/usr/share/wine"
# Camera and scanner devices are intentionally never exposed to a capsule.
# Drop Wine's corresponding optional Unix bridges instead of shipping modules
# whose libgphoto2/libsane dependencies could not be used in the sandbox.
rm -f -- \
    "$appdir/usr/lib/wine/x86_64-unix/gphoto2.so" \
    "$appdir/usr/lib/wine/x86_64-unix/sane.so"

if [[ ! -x /usr/lib/7zip/7z || ! -f /usr/lib/7zip/7z.so ]]; then
    echo "The full 7-Zip runtime is missing from /usr/lib/7zip" >&2
    exit 2
fi
cp -a /usr/lib/7zip "$appdir/usr/lib/7zip"

for shared_directory in \
    /usr/share/gamescope \
    /usr/share/gstreamer-1.0 \
    /usr/share/glib-2.0/schemas \
    /usr/share/icons/Adwaita \
    /usr/share/icons/hicolor; do
    if [[ -d $shared_directory ]]; then
        destination=$appdir${shared_directory%/*}
        mkdir -p -- "$destination"
        cp -a "$shared_directory" "$destination/"
    fi
done

# These are loaded at runtime rather than appearing in the main executable's
# ELF dependency list. In particular, Wine's Media Foundation bridge relies
# on GStreamer plugins for game video playback.
for runtime_module_directory in \
    /usr/lib/gio/modules \
    /usr/lib/gstreamer-1.0; do
    if [[ -d $runtime_module_directory ]]; then
        destination=$appdir${runtime_module_directory%/*}
        mkdir -p -- "$destination"
        cp -a "$runtime_module_directory" "$destination/"
    fi
done
if [[ -e /usr/share/X11/xkb ]]; then
    mkdir -p -- "$appdir/usr/share/X11"
    cp -aL /usr/share/X11/xkb "$appdir/usr/share/X11/xkb"
fi

magick_config=$(find /usr/share -maxdepth 1 -type d -name 'ImageMagick-*' -print -quit)
magick_coders=$(find /usr/lib -maxdepth 4 -type d -path '*/ImageMagick-*/modules-*/coders' -print -quit)
if [[ -z $magick_config || -z $magick_coders ]]; then
    echo "Could not locate ImageMagick configuration and coder modules" >&2
    exit 2
fi
mkdir -p -- "$appdir/usr/share/capsule/imagemagick" "$appdir/usr/lib/capsule/imagemagick"
cp -a "$magick_config"/. "$appdir/usr/share/capsule/imagemagick/"
mkdir -p -- "$appdir/usr/lib/capsule/imagemagick/coders"
for coder in icon png; do
    if [[ ! -f $magick_coders/$coder.so ]]; then
        echo "Required ImageMagick coder is missing: $coder" >&2
        exit 2
    fi
    cp -a "$magick_coders/$coder.so" "$appdir/usr/lib/capsule/imagemagick/coders/"
done

python_environment_ready() {
    [[ -x $python_env/bin/python ]] &&
        "$python_env/bin/python" -c \
            'from importlib.metadata import version; import PyInstaller, sandwine; assert version("pyinstaller") == "6.22.2"; assert version("sandwine") == "8.0.1"' \
            >/dev/null 2>&1
}

if ! python_environment_ready; then
    rm -rf -- "$python_env"
    python -m venv "$python_env"
    "$python_env/bin/pip" install --disable-pip-version-check \
        --requirement packaging/appimage/requirements-build.txt
fi
if ! python_environment_ready; then
    echo "The AppImage Python build environment is incomplete; remove $python_env and retry" >&2
    exit 2
fi
site_packages=$(
    "$python_env/bin/python" -c \
        'import sysconfig; print(sysconfig.get_paths()["purelib"])'
)
PYINSTALLER_CONFIG_DIR="$work_root/pyinstaller-cache" \
    "$python_env/bin/python" -m PyInstaller \
    --clean \
    --noconfirm \
    --onefile \
    --name sandwine \
    --copy-metadata sandwine \
    --distpath "$appdir/usr/libexec/capsule" \
    --workpath "$pyinstaller_work/work" \
    --specpath "$pyinstaller_work/spec" \
    packaging/appimage/sandwine_entry.py
mkdir -p -- "$appdir/usr/share/capsule/sources"
cp -a "$site_packages/sandwine" "$appdir/usr/share/capsule/sources/sandwine-8.0.1"
install -Dm644 "$site_packages/sandwine-8.0.1.dist-info/licenses/COPYING" \
    "$appdir/usr/share/licenses/sandwine/COPYING"

declare -a elf_queue=()
declare -A scanned=()

queue_elf() {
    local candidate=$1
    if file -Lb "$candidate" | grep -q '^ELF '; then
        elf_queue+=("$candidate")
    fi
}

copy_library() {
    local source=$1
    local relative=${source#/}
    local destination=$appdir/$relative
    case $source in
        /usr/lib/libc.so.*|/usr/lib/libm.so.*|/usr/lib/libdl.so.*|\
        /usr/lib/libpthread.so.*|/usr/lib/librt.so.*|/usr/lib64/ld-linux-*|\
        /lib64/ld-linux-*)
            return
            ;;
    esac
    if [[ ! -e $destination ]]; then
        mkdir -p -- "${destination%/*}"
        cp -aL -- "$source" "$destination"
        queue_elf "$destination"
    fi
}

while IFS= read -r -d '' candidate; do
    queue_elf "$candidate"
done < <(find "$appdir/usr/bin" "$appdir/usr/lib" "$appdir/usr/libexec" -type f -print0)

queue_index=0
while (( queue_index < ${#elf_queue[@]} )); do
    elf=${elf_queue[$queue_index]}
    ((queue_index += 1))
    if [[ -n ${scanned[$elf]:-} ]]; then
        continue
    fi
    scanned[$elf]=1
    while IFS= read -r dependency; do
        [[ -n $dependency ]] || continue
        if [[ ! -e $dependency ]]; then
            echo "Missing ELF dependency for $elf: $dependency" >&2
            exit 2
        fi
        copy_library "$dependency"
    done < <(
        ldd "$elf" 2>/dev/null |
            sed -n -e 's/.*=> \(\/[^ ]*\) .*/\1/p' -e 's/^\(\/[^ ]*\) .*/\1/p'
    )
done

if command -v pacman >/dev/null 2>&1; then
    package_inventory=$appdir/usr/share/licenses/PACKAGE_VERSIONS.txt
    : > "$package_inventory"
    for package in \
        wine gamescope xorg-xwayland bubblewrap e2fsprogs fuse2fs fuse3 \
        slirp4netns nftables gtk4 glib2 libadwaita 7zip wl-clipboard \
        curl imagemagick gstreamer gst-plugins-base-libs gst-plugins-base \
        gst-plugins-good gst-plugins-bad gst-plugins-ugly gst-libav; do
        if pacman -Q "$package" >> "$package_inventory" 2>/dev/null; then
            if [[ -d /usr/share/licenses/$package ]]; then
                cp -a /usr/share/licenses/$package "$appdir/usr/share/licenses/$package"
            fi
        fi
    done
fi

chmod 0755 "$appdir/AppRun"
rm -f -- "$output"
ARCH=x86_64 VERSION="$version" APPIMAGE_EXTRACT_AND_RUN=1 \
    "$appimagetool" "$appdir" "$output"
chmod 0755 "$output"
APPIMAGE_EXTRACT_AND_RUN=1 "$output" --doctor
sha256sum "$output" > "$output.sha256"

echo "Built $output"
