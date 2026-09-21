# Local deleted-folder workflow

FastCull can move unwanted photos into a `deleted` subfolder beside the originals.
This keeps them available for review and recovery without editing RAW data or
assigning a rejection tag.

## Controls

| Input | Action |
| --- | --- |
| Space / X | Move the displayed main RAW and matching XMP to `deleted`, then advance |
| Right | Next photo |
| Left / Shift+Space | Previous photo |
| S / + | Zoom in; held keys may repeat |
| D / − | Zoom out; held keys may repeat |
| Cmd+Z | Undo the latest rating edit or local move in this session |
| U / 0 | Clear rating/rejection metadata; does not restore a moved file |
| A | Toggle auto-advance for rating edits; local moves always advance |

Space/X act once per physical press. Cmd, Control, or Option combinations do not
trigger moves, so Cmd+Option+X still selects the rejected-photo filter. Moving
acts on the displayed main photo, independently of the filmstrip's scroll range
and the pinned reference. Success selects the next surviving visible photo,
or the previous survivor when removing the last one.

**File → Open deleted Folder** opens moved photos for review. Local moves are
refused within that folder to prevent nested `deleted/deleted` directories.
**File → Move Rejected Photos to deleted…** confirms a batch of existing saved
`Rating=-1` photos. The separate macOS Trash commands retain their previous
behavior; local undo does not undo Trash operations.

## File handling and responsiveness

Moves and restores share the serialized I/O queue with rating edits. Pending
sidecar saves finish before moving; failed saves block the affected move. Old
paths are tracked so a delayed rating write cannot recreate a moved sidecar.
The UI waits for success before removing a photo from its list. A fresh listing
generation rejects directory snapshots and image results from before the move.

RAW and XMP contents stay unchanged during a move. Regular `.xmp` and `.XMP`
sidecars are recognized; ambiguous pairs and symlinks are refused. Existing
destination files are preserved: collisions select an unused numbered RAW/XMP
pair. Each rename uses macOS exclusive rename on the same filesystem. If the
RAW rename fails after the sidecar moved, the operation attempts to roll the
sidecar back and reports remaining locations if rollback also fails. The pair
is not a single filesystem transaction, so this is not a crash-atomic two-file
move or a guarantee against an uncooperative process replacing directories.

Undo verifies the moved RAW's identity and refuses to replace a newly created
original. It restores the current matching sidecar, preserving ratings edited
while reviewing `deleted`, including a sidecar created after the move. Restored
ratings are read on the worker before updating active filters. Undo history is
bounded to 100 actions during the running session; files remain in `deleted`
after quitting and can also be returned using Finder.

Surviving CPU images are remapped by path to their new list indexes; main GPU
textures and filmstrip textures are retained by path. Removing one file does
not discard all prepared neighbors or clear the strip. Completed rating indexes
remain valid after a removal, and undo reads only restored sidecars. Restoring a
batch uses a membership set to avoid repeated scans of the growing file list.
S/D zoom changes the existing GPU transform without requesting a new photo.

## Verification

The project passed **180 tests**, strict Clippy (`-D warnings`), and formatting
checks. Coverage includes pure Space/X modifier/repeat routing, byte-preserving
move/restore, collision handling, symlink rejection, pair rollback, FIFO rating
and move ordering, stale-write protection, failed/retried undo, restoration of
updated sidecars and ratings, survivor selection, natural-order restoration,
and cached-neighbor reuse after index changes.

Filesystem mutation tests use temporary synthetic RAW/XMP fixtures; they do not
move or rewrite the user's original photos. Test and Clippy logs are in
`bench-results/deleted-workflow-validation/tests.log` and `clippy.log`.

**Desktop recheck passed.** Both read-only runs completed all 45 captures:

| Input | Budget | Cached texture identity checks | Result |
| --- | ---: | ---: | --- |
| Actual 433-photo A7 V folder | 512 MiB | 3,692 | Passed |
| Six-photo fixture with 7008×4672 previews | 256 MiB | 1,750 | Passed |

Both runs exercised S/D press and repeat and verified that zoom changed only the
transform, preserving selection, loading generation, request count, and the
current GPU texture. The original viewport was restored afterward. Space/X was
not invoked on the real corpus. Comparing real-folder frames 40 and 41 confirmed
that scrolling the strip changed its contents while the main image stayed
byte-identical. The 433 original RAWs and two sidecars retained their filenames,
inodes, sizes, and modification timestamps across validation.

The final Intel-only `dist/FastCull.app` was rebuilt and its ad hoc signature
verified. Its command-line help was checked for the new shortcuts. Existing
running copies were not restarted. Evidence is in
`bench-results/deleted-workflow-validation/`: `summary.json`, `package.log`,
`help.txt`, `smoke-real512.log`, `smoke-full256.log`, both frame directories,
`real-scroll-comparison.json`, and `originals-check.json`. These functional
checks are not a controlled latency or FPS benchmark; actual file moves were
exercised by temporary-file tests rather than against the real photo folder.
