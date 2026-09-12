# Media compatibility record

What this hardware has actually been observed to do, disc type by disc type. Kept
because "can we dump X?" turned out to have a different answer per console, and the
reasons are not guessable from the outside — two of my own predictions in this table
were wrong before the disc went in.

Every row is an observation from this setup. Nothing here is inferred from
documentation, and the **Proves / does not prove** column exists because a single
result usually constrains less than it appears to.

## Test setup

| | |
|---|---|
| Drive | PIONEER BD-RW BDR-XD07U, firmware 1.03 |
| Connection | USB (`08e4:017a` Pioneer Corp. BD-XD07 BD/DVD/CD Writer), external slim tray |
| Advertised media | CD, DVD, BD — from udev `ID_CDROM_BD`; see the caveat under BD below |
| Host | Ubuntu 24.04.4 LTS, Linux 6.8.0-139-generic |
| redumper | build b744 |
| MakeMKV | v1.18.3 linux(x64-release) |
| chdman | MAME 0.264 |

The drive is a **slim USB tray** model, which matters: the disc must click onto the
centre spindle clips. A disc resting in the tray without engaging reports `no disc`,
not a read error. This cost real time twice before it was understood.

## Results

| Media | Result | Evidence | Proves / does not prove |
|---|---|---|---|
| **DVD-Video** | ✅ Works | 12 discs (ESPN 30 for 30) ripped, identified, migrated and verified. Probe profile `dvd_rom`. | Solid. The MakeMKV path is exercised end to end. |
| **PS2 game disc (DVD)** | ✅ Works | 1 disc, `SLUS-20578`, probe profile `dvd_rom`. CHD round-trips to Redump's exact digests. | Solid for single-layer. Dual-layer PS2 has **not** been through the drive. |
| **PS2 game disc (CD)** | ✅ Works | 2 discs, `SLUS-20247` and `SLUS-21038`, probe profile `cd_rom`. Both verify against Redump, including byte-exact cue reconstruction. | Solid. |
| **Original Xbox** | ⚠️ Video partition only | Probe reads a 14.3 MB `VIDEO_TS` volume; application id `Session Offset : 0 VTC Sector Offset: 0`; label `SEP13011042` is a pressing date code. Sectors ≥ 7000 return 0 bytes. | The game is in an XDVDFS partition outside the addressable range. Not a software limit: redumper supports these discs, but only through Kreon firmware. |
| **PS3** | ❌ Unreadable | `not ready` ×3 while spinning up, then `disc present, unreadable`. GET CONFIGURATION returns **`unknown profile 0x0000`** — no medium type determined. `dvd+rw-mediainfo` agrees: empty current configuration. | Does **not** prove PS3 discs are unreadable in PC drives generally. Three causes remain open — see below. |
| **Blu-ray (any)** | ❓ Never tested | — | **The BD read path has never been validated on this drive.** "reads: BD" is an advertised capability from udev, not an observation. |
| **Wii** | ❓ Not yet tested | — | — |
| **GameCube** | ❓ Not yet tested | — | — |
| **Xbox 360** | ❓ Not yet tested | — | Expected to resemble Xbox, but it is unknown whether it carries the same `VTC Sector Offset` marker our detection keys on. |
| **Audio CD** | ❓ Not tested | — | No backend implemented. |

### The open PS3 question

The `0x0000` profile means the drive never classified the medium — it is not the
"readable filesystem, encrypted payload" outcome that was predicted. Three
explanations remain, and this setup cannot currently distinguish them:

1. PS3 discs are genuinely unreadable in standard PC BD drives.
2. This drive's BD-ROM support is the problem — **untested**, since no ordinary
   Blu-ray has ever been put in it.
3. That particular disc did not seat, or is dirty. A seating failure happened on this
   drive earlier the same day.

**One disc settles it:** an ordinary Blu-ray movie. `0x0040` and a readable filesystem
eliminates cause 2 and leaves 1 or 3; another `0x0000` means the drive is at fault and
the PS3 row says nothing about PS3 discs. Until then, treat that row as inconclusive.

### Hardware needed for what this drive cannot do

| Media | Route |
|---|---|
| Original Xbox | Kreon-firmware drive. redumper's database lists exactly four, all TSSTcorp: `SH-D163B`, `SH-D163A`, `SH-D162D`, `SH-D162C`, matched on a `KREON V1.00` vendor string. Neither the BDR-XD07U nor the one Pioneer entry in that database qualifies. |
| Xbox 360 | Different firmware from the above; not yet investigated for this setup. |
| Wii / GameCube | Normally dumped on the console itself (CleanRip on a homebrew-enabled Wii, which handles GameCube discs too), not on a PC drive. |
| PS3 | Normally a console on custom firmware, or a 3k3y-style ODE. |

## Not drive evidence

Three migrated PS2 titles have **no probe record** because they were adopted from an
existing library rather than dumped here: Scooby-Doo *Night of 100 Frights*, and Star
Ocean *Till the End of Time* discs 1 and 2. They verify against Redump datfiles, but
they say nothing about this drive and must not be counted as compatibility results.

## Reproducing a row

```sh
dump-o-matic drives      # tray state, including "disc present, unreadable"
dump-o-matic probe       # medium profile, filesystem, content classification
```

Useful when probe refuses, since it reports the drive's own view of the medium:

```sh
dvd+rw-mediainfo /dev/sr0            # INQUIRY + GET CONFIGURATION
blockdev --getsz /dev/sr0            # addressable size, in 512-byte units
dd if=/dev/sr0 of=/dev/null bs=2048 skip=N count=1   # is sector N reachable?
```
