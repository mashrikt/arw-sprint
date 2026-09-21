#!/bin/bash
# Produce a local, ad hoc signed Intel app; does not install or launch it.
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
if [[ "$(uname -s)" != Darwin ]]; then
    echo "FastCull packaging requires macOS and Apple's command line tools." >&2
    exit 1
fi

dist_dir="$project_dir/dist"
bundle="$dist_dir/FastCull.app"
lock_dir="$dist_dir/.fastcull-package.lock"
staging_root=""
previous_bundle=""
mkdir -p "$dist_dir"
if ! mkdir "$lock_dir"; then
    echo "Packaging is already running, or a previous run left $lock_dir." >&2
    echo "If no packaging process is running, remove that empty lock directory and retry." >&2
    exit 1
fi

finish() {
    local status=$?
    trap - EXIT
    # If publication failed after retiring the old bundle, restore its name.
    # Never touch the previous bundle's executable or resource file contents.
    if [[ -n "$previous_bundle" && -d "$previous_bundle" && ! -e "$bundle" && ! -L "$bundle" ]]; then
        if mv "$previous_bundle" "$bundle"; then
            echo "Restored the previous app at $bundle." >&2
        else
            echo "Could not restore the app; the previous bundle is safe at $previous_bundle." >&2
            status=1
        fi
    fi
    if [[ -n "$staging_root" && -d "$staging_root" ]]; then
        if ! rmdir "$staging_root" 2>/dev/null; then
            echo "Unpublished staging files retained at $staging_root." >&2
        fi
    fi
    rmdir "$lock_dir" || true
    exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

export MACOSX_DEPLOYMENT_TARGET=10.15
cargo build --release --locked --features desktop --bin fastcull --target x86_64-apple-darwin
binary="$project_dir/target/x86_64-apple-darwin/release/fastcull"
if [[ "$(lipo -archs "$binary")" != x86_64 ]]; then
    echo "Refusing to package a binary that is not exclusively x86_64." >&2
    exit 1
fi

# Stage on the destination volume so publication only renames directories.
# install/codesign must never modify a bundle that a running app may map.
staging_root="$(mktemp -d "$dist_dir/.fastcull-stage.XXXXXX")"
staged_bundle="$staging_root/FastCull.app"
iconset="$project_dir/target/fastcull-icon.iconset"
mkdir -p "$staged_bundle/Contents/MacOS" "$staged_bundle/Contents/Resources" "$project_dir/target/swift-module-cache"
xcrun swift -module-cache-path "$project_dir/target/swift-module-cache" "$project_dir/packaging/make-icon.swift" "$iconset"
iconutil --convert icns --output "$staged_bundle/Contents/Resources/FastCull.icns" "$iconset"
install -m 755 "$binary" "$staged_bundle/Contents/MacOS/fastcull"
install -m 644 "$project_dir/packaging/Info.plist" "$staged_bundle/Contents/Info.plist"
plutil -lint "$staged_bundle/Contents/Info.plist"
codesign --force --sign - "$staged_bundle"
codesign --verify --deep --strict "$staged_bundle"

if [[ -e "$bundle" || -L "$bundle" ]]; then
    if [[ ! -d "$bundle" || -L "$bundle" ]]; then
        echo "Refusing to replace a non-directory or symlink at $bundle." >&2
        exit 1
    fi
    previous_root="$(mktemp -d "$dist_dir/.fastcull-previous.XXXXXX")"
    previous_bundle="$previous_root/FastCull.app"
    mv "$bundle" "$previous_bundle"
fi
mv "$staged_bundle" "$bundle"
echo "Created $bundle (Intel x86_64, locally ad hoc signed; not notarized)."
if [[ -n "$previous_bundle" ]]; then
    echo "Previous app preserved at $previous_bundle; running apps were not restarted."
fi
