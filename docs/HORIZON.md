# The Horizon page

The Horizon page runs [Horizon](https://github.com/horizon-rust/horizon) - a
Rust/wgpu Minecraft client — with **no Microsoft account and no Horizon
launcher**. This document records how it works, what it refuses to do, and the
`horizon-ctl` behaviour it has to work around.

## Contract

- Only `horizon-ctl.exe` and `horizon-prepare.exe` are executed. The launcher
  never starts `horizon-launcher.exe`; that is the part that asks for an account.
- No Horizon source is copied, compiled, patched or vendored. Nothing is
  redistributed.
- Every release archive is checked against the SHA-256 published with it before
  it is unpacked. A one-byte change is refused, and a test proves it.
- The pinned server jar comes from Mojang's content store and is verified by
  SHA-1 before use.
- All Horizon builds share one game folder (`instances/horizon`). A new nightly
  must not hide yesterday's world.

## Layout

```
%APPDATA%\mcl\horizon\
  builds\<folder>\            horizon-client.exe, horizon-ctl.exe, horizon-prepare.exe
  resources\<folder>\         prepared assets
  instances\horizon\
    saves\<world>\            one folder per world
    backups\<stamp>_<folder>.zip
    screenshots\
    home\                     HORIZON_HOME: logs, crash-reports, options
  downloads\
  builds.json                 the last good build list, for an offline start
```

A zip with a single wrapper folder (`horizon-windows-x64/`) has that folder
stripped so `builds\<folder>\horizon-client.exe` is the real path. Partially
installed `.tmp-install` folders are ignored.

## What each stage does

| Stage | Action |
| --- | --- |
| Build list | GitHub API first; `releases.atom` and `releases/expanded_assets/<tag>` as fallbacks. Tags starting `nightly` are accepted except `nightly-source/...`. A dated tag keeps its own day. |
| Install | Stream the archive, hash it while it streams, unpack only on a match. |
| Prepare | `horizon-prepare --prepare --complete` (plain `--prepare` omits sounds). Progress is reported from its output; `--check` must exit 0 before Play is enabled. |
| Play | `horizon-ctl launch --wait --world <name> --game ... --resources ... --game-dir ... --home ... [--name <player>]`, then the launcher docks and polls the process. |
| Quit | `horizon-ctl quit`, 20 s poll, then `taskkill /F /T` as a fallback. |
| Worlds | List, back up, restore, screenshot, delete. |
| Name | `--name` is passed only when Settings holds one; it persists in `%APPDATA%\mcl\settings.json` as `default_user`. |

## `horizon-ctl` quirks

These are measured, not guessed:

- **`worlds` needs a running game.** It talks over the control socket at
  `%TEMP%\horizon-control.json`. With no dive open it answers
  `cannot connect … is the game running with --control?`. The page says so
  instead of pretending there are zero worlds.
- **`world-backup` needs the title screen**, not a loaded world.
- **`world-delete` refuses without `--confirm`** ("ask the player, then add
  --confirm"). The two-click *Delete for good* button is that question.
- **ctl splits its command line on spaces.** `world-backup "New World"` arrives
  as folder `New` and fails with `os error 2`. Folders with spaces are zipped
  and deleted by the launcher itself.
- **Restore does not exist in ctl.** It is ours: `<stamp>_<folder>.zip` with all
  entries under `<folder>/`, and restoring over an existing world is refused.
- `launch --wait` answers when the player spawns (~20 s), not when the game
  is playable (~60–90 s, of which resources load ~55 s). A ten-minute deadline
  stops a stuck launch from hanging forever.
- `screenshot <path>` writes a real PNG while a world is loaded.
- `quit` saves properly: `level.dat`'s mtime advances and `level.dat_old` is
  written.

## Edge cases the page handles

- A restored copy keeps the original's `LevelName`, so two rows could both say
  "New World". Rows show `folder <name>` when the folder disagrees.
- The `.toast` element used to sit invisibly over the Settings **Save settings**
  button and swallow the click. It now has `pointer-events: none`.
- Deleting a world only through ctl fails with no game running, so it falls back
  to the folder. A running game holds `session.lock`, which stops a live world
  from being pulled out from under the player.

## Testing

`tests/horizon_negative_control.rs` proves the installer refuses a corrupted
archive: it flips a single byte and expects the hash check to fail before
anything is unpacked. The rest of the page is exercised by hand, and the ctl
quirks above are what that manual pass found.
