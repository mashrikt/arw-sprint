# FastCull

A fast, minimal photo culler for **Intel Macs** and Sony `.ARW` photographs,
including the A7 V. It displays embedded JPEG previews so you can browse, compare,
and rate photos quickly. It does not develop or edit RAW images.

## Open photos

Open **FastCull.app**, then press **Cmd+O** and choose the folder containing your
photos. You can also drop a folder or ARW onto the app. Subfolders are not scanned.

## Everyday controls

| Key | Action |
| --- | --- |
| Right | Next photo |
| Left / Shift+Space | Previous photo |
| **Space / X** | Move this photo and its XMP sidecar to `deleted`, then advance |
| 1–5 | Assign stars |
| U / 0 | Clear rating or rejection metadata |
| Cmd+Z | Undo the latest rating edit or local move during this session |
| S / D | Zoom in / out |
| Z | Switch between fit and 100% |
| L | Toggle keeping zoom and position across photos |
| Click and drag | Pan while zoomed |
| Tab | Show or hide the thumbnail filmstrip |
| Cmd+O / Cmd+Q | Open a folder / quit |

Scroll over the filmstrip to browse thumbnails **without changing the main photo**.
Click a thumbnail to open it. Scroll over the main photo to zoom.
Zoom and pan position carry over to the next photo by default; **L** toggles this.

Use the **Filter** menu to browse a star rating, rated photos, or unrated photos.
Choose **All photos** to return to the whole folder. Ratings are saved in XMP
sidecars and survive restart.

Space/X moves files into a local `deleted` subfolder; it does not permanently
delete them. Choose **File → Open deleted Folder** to review those photos.
**Cmd+Z** restores a move during the same session; **U/0** only clears metadata.

The **View** and **Help** menus cover fullscreen, comparison, zoom lock, and the
remaining shortcuts.

## What keeps browsing fast

- **Read the preview directly.** Container metadata identifies the largest embedded
  JPEG, so opening a photo reads the preview instead of the whole ARW.
- **Use Intel acceleration.** The JPEG decoder selects instructions supported by
  the CPU automatically, with a compatible fallback for older Intel Macs.
- **Prioritize the photo you requested.** Background workers load and decode;
  rapid navigation replaces queued requests and discards stale results.
- **Keep nearby photos ready.** Prefetch follows your browsing direction and keeps
  decoded neighbors within a memory budget, avoiding repeated work when possible.
- **Reuse Metal textures.** Zoom, pan, and resize transform the uploaded image.
  Background uploads keep large image transfers off the event thread.
- **Cache the filmstrip.** Small embedded thumbnails stay in a rolling cache.
  Scrolling loads missing cells without changing the main photo or rebuilding it.
- **Sleep when idle.** Workers wait for work or available memory, and the window
  redraws when needed. A full cache does not trigger periodic polling.

## Intel measurements · 21 September 2026

Measured locally on an **Intel Core i7-1068NG7, 4 cores, 32 GB RAM, Iris Plus**.
Desktop smoke tests also opened a folder of **433 Sony A7 V ARWs** read-only.

| Full-size JPEG decode | Median time |
| --- | --- |
| Intel acceleration enabled | 178.8–179.9 ms |
| Acceleration disabled for comparison | 377.8–381.6 ms |

This comparison used six 7008×4672 previews, 18 decodes per process, and two runs
with the order reversed. Intel acceleration was about **2.1× faster**. These are
uncached decode timings, not cached navigation timings; uncached photos are not
guaranteed to appear within 100 ms. See the [Intel audit](docs/intel-audit.md) for
methods, limitations, and graphics measurements.

## Build from source

Requires Rust, Xcode Command Line Tools, CMake, and NASM on macOS.

```sh
brew install cmake nasm
rustup target add x86_64-apple-darwin
./scripts/package-macos.sh
open dist/FastCull.app
```

The app is Intel-only and requires Metal graphics. The script creates a locally
signed app in `dist`; it does not install it in Applications.

More detail: [file moves and undo](docs/deleted-workflow.md),
[architecture](docs/architecture.md), and [performance measurements](docs/measurements.md).
