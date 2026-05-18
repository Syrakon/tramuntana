# Syncing tramuntana with Servo upstream

`tramuntana` is a fork of [`servo/servo`](https://github.com/servo/servo)
with a small number of patches on top (the bridge to gpui used by
rumb, NVIDIA-friendly adapter selection, the dma-buf export path,
etc). The fork's `main` branch is always Servo upstream **plus our
patches on top**. This file documents how to keep up to date with
upstream.

## TL;DR

```sh
# from tramuntana/, on main:
torii sync --fetch upstream                   # bring in upstream commits
torii sync upstream/main --merge              # merge them into our main
# resolve any conflicts, save, push:
torii save -am "merge: sync upstream/main @ <short OID>"
torii sync --push
```

A weekly CI job (`.gitlab-ci.yml: upstream-sync`) does this
automatically — see "Automatic sync" below.

## Layout

```
remotes:
  origin   → gitlab.com/syrakon/tramuntana   (our fork, R/W)
  upstream → github.com/servo/servo          (Servo upstream, read-only)

branches (single):
  main     → Servo upstream + our patches, the only branch we maintain
```

We deliberately don't keep a local `servo-upstream` mirror branch.
The remote-tracking ref `refs/remotes/upstream/main` (updated by
`torii sync --fetch upstream`) is enough to merge from.

## Manual sync flow

1. **Stand on `main`** with a clean working tree:

   ```sh
   torii status              # working tree clean, branch main
   torii branch              # confirm we're on main
   ```

2. **Fetch upstream**:

   ```sh
   torii sync --fetch upstream
   ```

   First time this may be slow (the Servo repo is ~1-2 GB). Subsequent
   fetches are incremental.

3. **Merge upstream/main into main**:

   ```sh
   torii sync upstream/main --merge
   ```

   If there are no conflicts, you get a clean merge commit. If
   upstream touched files our patches also touch, you get conflicts
   to resolve.

4. **Resolve conflicts (when they happen)**: for each conflicted
   file, decide:

   - If upstream Servo now does what our patch did, **drop our
     patch**: take upstream's side (`<<<<<<< HEAD` block goes, keep
     the `>>>>>>> upstream/main` block).
   - If upstream Servo doesn't cover our use case, **keep our patch**:
     keep `HEAD` side, drop the upstream-only side.
   - If upstream changed our patched area in a way we want, **merge
     both** by hand.

   Then `torii save -am "..."` to record the merge.

5. **Sanity check by building**:

   ```sh
   RUSTUP_TOOLCHAIN=1.94.0 RUST_MIN_STACK=67108864 cargo build -j 2 -p servo-paint-api
   # and ideally the full rumb build at /mnt/datos/repos/rumb:
   cargo build -j 2
   ```

6. **Push**:

   ```sh
   torii sync --push
   ```

## What our patches touch (so you know where to expect conflicts)

Our diff against upstream Servo, as of this writing:

- `components/shared/paint/rendering_context.rs` — added
  `new_offscreen_only`, the dma-buf cache, `framebuffer_color_texture_id`,
  `export_framebuffer_dma_buf`, the `DmaBufHandle` struct.
- `components/shared/paint/Cargo.toml` — adds `khronos-egl` as a
  direct dependency.
- `components/paint/paint.rs` — `create_hardware_adapter().or_else(create_adapter)`
  fallback (the WebGL three.js fix on NVIDIA + EGL).
- `components/servo/lib.rs` — re-exports `DmaBufHandle` alongside
  the other rendering context types.
- `Cargo.toml` (root workspace) — adds `khronos-egl` to
  `workspace.dependencies`.
- `third_party/gpui/` — entirely new directory, vendored gpui from
  zed. **Never touched by upstream**, so never a conflict source.
- `docs/STAGE_2_GPU_SHARING.md` — our design doc. **Never upstream**.
- `SYNCING.md` — this file. **Never upstream**.

The bulk of conflicts will come from `components/shared/paint/`
because that's where the bridge lives.

## Automatic sync (CI weekly)

`.gitlab-ci.yml` includes an `upstream-sync` job scheduled weekly
that runs the fetch + merge above. On conflict-free merges it
pushes directly to `main`. On conflicts it stops and opens a MR
(merge request) on gitlab tagged with `needs-manual-merge` so the
maintainer resolves the conflict by hand.

To trigger a manual run from gitlab UI:

```
Pipelines → "Run pipeline" → set variable RUN_UPSTREAM_SYNC=1
```

## Aborting / rolling back

If you start a merge and it gets messy:

```sh
torii history rebase --abort         # if you used --rebase by accident
# OR for an aborted merge:
torii save --reset HEAD --reset-mode hard
```

This rewinds to the state before the merge started. Then start over
or skip the sync for this week.

## When we should *not* sync

- Mid-release: if rumb is being prepped for a tag, freeze upstream
  syncs until the tag ships.
- Mid-feature: if Stage 2 / cursor / input subsystems have
  in-progress half-finished work in `paint/` or `script/`, an
  upstream sync that touches the same area will multiply pain.
  Finish the feature first.
- Servo just shipped a known-broken commit upstream: check the
  servo/servo issue tracker before pulling. Wait for the revert.

## Why merge instead of rebase

Single-developer fork, but the `main` branch is already pushed to
`origin/main` on gitlab. Rebasing would rewrite OIDs and force-push
on every sync — break any clone elsewhere. Merge keeps history
non-destructive at the cost of a slightly messier graph; the graph
is fine, we'll never look at it once it's working.
