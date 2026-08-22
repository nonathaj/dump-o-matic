# dump-o-matic

A staged, conservative media backup pipeline: probe a disc, rip it to staging, identify
what it actually is with a confidence level, organize it to your naming conventions, and
migrate it to permanent storage — without ever risking data loss.

Supports movies/TV, audio CDs, and game discs (PS1/PS2 and friends). See [PLAN.md](PLAN.md)
for the full design, the survey of existing tools, and the roadmap.

> **Status: early, but working.** The full pipeline runs end to end for **game discs**:
> probe → rip → identify → migrate, including verified transfer to a network share.
> Video discs rip and hash correctly but cannot yet be identified automatically, and
> audio CD support is not built. Nothing in this repository ever writes to a disc.

## Pipeline

| Stage | Command | Games | Video | Audio CD |
|---|---|---|---|---|
| 1. Probe | `probe` | ✅ | ✅ | ✅ |
| 2. Rip to staging | `rip` | ✅ redumper | ✅ MakeMKV | ✗ |
| 3. Identify | `identify` | ✅ Redump datfiles | ✗ TMDB/TVDB not built | ✗ |
| 4. Migrate | `migrate` | ✅ | — | — |
| Re-verify | `verify` | ✅ | ✅ | ✅ |

## What works today

```console
$ dump-o-matic drives
/dev/sr0  PIONEER BD-RW  BDR-XD07U
    state:  disc present
    reads:  CD, DVD, BD
    firmware: 1.03

$ dump-o-matic probe
Device:     /dev/sr0
Medium:     DVD-ROM (DVD)
Capacity:   7.49 GB

Detected:   DVD-Video
Title guess: ESPN_30_FOR_30_DISC_1
Confidence: strong  (needs confirmation)
Evidence:
  - VIDEO_TS directory present in root
  - volume label "ESPN_30_FOR_30_DISC_1" used as title guess

Volume:
  label:       ESPN_30_FOR_30_DISC_1
  application: DVD Studio Pro:4.2.2, ...
  size:        3656640 sectors x 2048 bytes = 7.49 GB

Root directory (2 entries):
  AUDIO_TS/
  VIDEO_TS/

Probed in 249 ms
```

Both commands accept `--json` for scripting.

### Probe capabilities

| Signal | Source |
|---|---|
| Medium type (CD/DVD/BD, -R/-RW/-ROM) | SCSI MMC `GET CONFIGURATION` — from the drive, not guessed |
| True capacity | SCSI MMC `READ CAPACITY` |
| Drive identity | SCSI `INQUIRY`, falling back to sysfs |
| Tray/media state | `CDROM_DRIVE_STATUS` ioctl |
| Audio CD track list | `CDROMREADTOCHDR` / `CDROMREADTOCENTRY` ioctls |
| MusicBrainz + FreeDB disc IDs | Computed from the TOC (validated against the published MusicBrainz reference vector) |
| Volume label, publisher, dates, size | ISO 9660 primary volume descriptor |
| UDF presence | Volume recognition sequence |
| DVD-Video / Blu-ray detection | `VIDEO_TS` / `BDMV` in the root directory |
| PS1 / PS2 game serial | `SYSTEM.CNF` boot entry, normalised to Redump form (`SLUS_203.12` → `SLUS-20312`) |

Every conclusion carries its **evidence** and a **confidence level**. Only `exact`
(cryptographic or structural certainty) is ever eligible for unattended handling;
everything else is explicitly marked as needing confirmation.

## Design guarantees

- **Read-only, for now.** The implemented commands open devices read-only and issue only
  informational SCSI commands.
- **Never delete or overwrite without proof.** No source is removed until its replacement
  is verified by checksum on a genuine re-read at the destination. See PLAN.md §2.1.
- **No bundled credentials.** All API keys come from config or environment. See PLAN.md §2.6.

## Building

Requires Rust 1.75 or newer.

```sh
cargo build --release
cargo test
```

The binary is `target/release/dump-o-matic`.

### Permissions

Reading optical drives requires access to the device node. On most distributions that
means membership in the `cdrom` group:

```sh
sudo usermod -aG cdrom "$USER"   # log out and back in
```

## License

MIT — see [LICENSE](LICENSE).
