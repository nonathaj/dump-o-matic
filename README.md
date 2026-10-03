# dump-o-matic

A staged, conservative media backup pipeline: probe a disc, rip it to staging, identify
what it actually is with a confidence level, organize it to your naming conventions, and
migrate it to permanent storage — without ever risking data loss.

Supports movies/TV, audio CDs, game discs (PS1/PS2 and friends), and games stored on a
console's own USB drive. See [PLAN.md](PLAN.md) for the full design, the survey of existing
tools, and the roadmap.

> **Status: early, but working.** The full pipeline runs end to end for **game discs**,
> **video discs** and **audio CDs**: probe → rip → identify → (repack) → migrate,
> including verified transfer to a network share. Nothing in this repository ever writes
> to a disc.

## Pipeline

| Stage | Command | Games | Video | Audio CD |
|---|---|---|---|---|
| 0. Adopt existing files | `adopt` | ✅ | — | ✗ |
| 1. Probe | `probe` | ✅ | ✅ | ✅ |
| 2. Rip to staging | `rip` | ✅ redumper | ✅ MakeMKV | ✅ redumper |
| 3. Identify | `identify` | ✅ Redump datfiles | ✅ TMDB, runtimes + dialogue | ✅ MusicBrainz + AccurateRip |
| 3b. Re-package | `repack` | ✅ CHD | — | — (MP3s are encoded at identify) |
| 4. Migrate | `migrate` | ✅ | ✅ | ✅ |
| Re-verify | `verify` | ✅ | ✅ | ✅ |

Games and audio CDs are identified exactly — a Redump hash, or a MusicBrainz disc ID
(a hash of the disc's table of contents) carried by exactly one release — so `--apply`
files them, and `run` does so unattended. Video identification is always inference from
runtimes and dialogue: `identify --apply --accept-inferred` files it once you have read
the proposal, and `run` never will.

### Audio CDs

Ripped by redumper, which corrects the drive's read offset and checks C2 errors, to a
lossless per-track `.bin`/`.cue` that stays in the job directory as the archival record.
Set `drives.read_offset` to your drive's value from
[AccurateRip's list](http://www.accuraterip.com/driveoffsets.htm) if redumper does not
know the drive — a pure audio CD has no data track to measure it from.

`identify` looks the disc ID up in MusicBrainz and checks every track against
AccurateRip; a track that matches no other submitter's rip stops filing. If several
releases share the disc ID, it lists them and files nothing until you pick one with
`--release <id>`. With `--apply`, each track is encoded with LAME `-V 2` and tagged as
MusicBrainz Picard would (ID3v2.4, MusicBrainz IDs, artist names translated to English,
titles in their own script):

```
music/Hiroyuki Sawano - 2014 - アルドノア・ゼロ オリジナル・サウンドトラック/
  1-01 - Hiroyuki Sawano - No differences.mp3
```

### Storage devices

A console's USB drive is a filesystem rather than a disc, so it has its own front half of
the pipeline. The back half is shared: `pull` produces an ordinary job, and `identify`,
`migrate`, `verify` and `clean` then treat it like any other.

| Stage | Command | Counterpart |
|---|---|---|
| List attached devices | `devices` | `drives` |
| See what is on one | `catalog` | `probe` |
| Extract one title to staging | `pull` | `rip` |

```console
$ dump-o-matic catalog /dev/sdd
/dev/sdd — Xbox 360 content storage
  medium: FAT32, OEM name "XBOX360"

  title id  name                       kind              on device     as iso  discs
  534507D4  Chromehounds               Games on Demand     3.79 GB    3.77 GB  -
  545407E0  Prey                       Games on Demand     4.58 GB    4.56 GB  -
  ...
  8 of 9 item(s) can be pulled off as game images.

$ dump-o-matic pull /dev/sdd --title 545407E0
```

`pull --format iso` (the default) writes a single image that ordinary tools and emulators
read; `--format package` copies the package exactly as the console wrote it, so its hash
tree travels with it and it stays verifiable indefinitely; `--format both` writes each.

The device is opened read-only and its FAT32 volume is parsed in-process rather than
mounted, so the kernel never gets the chance to update a dirty bit on media being
preserved. Reading a block device needs `disk` group membership — or mount it read-only and
pass the mount directory, which works anywhere a device node does.

The format, how it was decoded, and exactly what the resulting files are and are not is in
[docs/xbox360-storage.md](docs/xbox360-storage.md).

### How identification decides

Game discs are matched by hash against Redump datfiles, and **only an exact match is ever
filed unattended**. A multi-file CD set is not filed until every file in its datfile entry
is accounted for, which includes rebuilding the `.cue` into Redump's canonical form and
checking it byte-for-byte.

Video has no hash to match, so it is inference and is reported as such — it always needs
confirming, however strong the evidence. Episodes are scored on runtime *and* on subtitle
dialogue compared against episode synopses, weighted by how informative each word is
across that season rather than against any hardcoded stopword list. Discs of a box set are
solved jointly, since the discs of a season hold consecutive non-overlapping runs of
episodes, which frequently decides cases that runtimes alone cannot.

Content pulled off a storage device sits between the two. There is no hash to match — a
Games-on-Demand package holds only the game partition, while Redump's Xbox 360 hashes cover
a whole disc, so the two can never be compared — but every block copied is checked against
a SHA-1 hash tree the console itself wrote, and the game's executable independently repeats
the title and media IDs the package claims. That is reported as `strong`: enough to name
the file, not enough to file unattended, so it needs `--accept-inferred` like video does. A
package extraction is a verified *copy*, which is not a verified *dump*.

Confidence comes from corroboration, not from one score being small. Measured on a real
four-disc set: dialogue, choosing freely across all 30 episodes, independently reached the
same placement as the constrained solve for 9 of 10 titles; runtime managed 2. A set with
no subtitle track anywhere cannot reach a confidence worth acting on, and says so.

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
| Original Xbox disc | ISO 9660 application identifier (`VTC Sector Offset`), which marks the video partition of a disc whose game is not addressable |
| Xbox 360 content on a USB drive | FAT32 `Content/<profile>/<title id>/<type>/` tree, package headers read for title and media IDs, cross-checked against the game's own `default.xex` |
| Disc present but unreadable | `CDS_DISC_OK` from the tray state contradicted by `ENOMEDIUM` on open, reported with the medium profile so a damaged disc is distinguishable from an unsupported format |

Which media this actually works on is recorded per disc type, with the evidence and
the limits of each result, in [docs/media-compatibility.md](docs/media-compatibility.md).

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

Reading a storage device (`devices`, `catalog`, `pull`) needs the `disk` group instead:

```sh
sudo usermod -aG disk "$USER"    # log out and back in
```

Or avoid it entirely: mount the device read-only and pass the mount directory, which every
device command accepts in place of a device node.

## License

MIT — see [LICENSE](LICENSE).
