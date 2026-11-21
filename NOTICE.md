# Notices

This repository contains the Trenchlight launcher (`mcl`), which is MIT
licensed. It does not contain, and does not redistribute, any of the following.

## Horizon

Horizon is a separate Minecraft client written in Rust, licensed under
**LGPL-3.0**. Its source and releases belong to the Horizon project.

The launcher downloads official Horizon release archives from GitHub **at the
user's request**, verifies the published SHA-256, and unpacks them into the
user's own application data folder. No Horizon source code, build, patch or
binary is included in this repository. `horizon-launcher.exe` is never executed;
the launcher drives `horizon-ctl.exe` and `horizon-prepare.exe`.

## Minecraft

Minecraft is a trademark of Mojang Synergies AB / Microsoft. This project is
**not affiliated with, endorsed by or sponsored by** Mojang or Microsoft.

Game assets, libraries and the client jar are downloaded from Mojang's public
CDN at the user's request. The pinned server jar used for Horizon's integrated
server is downloaded from Mojang's content store and verified by SHA-1. Neither
is redistributed here.

## WebView2

The GUI embeds Microsoft's WebView2 runtime, which is installed separately on
the user's machine. It is not bundled.

## Java

The launcher detects Java installations already present on the machine. It does
not download or redistribute a JDK.
