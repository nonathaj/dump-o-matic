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

The drive is a **slim USB tray** model, which matters in two ways:

- The disc must click onto the centre spindle clips. A disc resting in the tray without
  engaging reports `no disc`, not a read error. This cost real time three times before
  it was understood, and `no disc` versus `disc present, unreadable` is the tell: the
  first is seating, the second is the disc or the format.
- The tray is **manual close only**. `eject -t` fails with "CD-ROM tray close command
  failed", so the tray has to be pushed shut by hand. Software can open it but not
  close it.

Double-sided ("flipper") DVDs are worth calling out separately: both surfaces carry
data, so neither can be set down safely and handling damage is common. One side reading
perfectly while the other is unreadable is an expected outcome, not a contradiction —
observed on a Wonder Woman season 3 disc whose side 2 dumped cleanly while side 1 would
not read. Both sides also identify themselves as "disc 1", which breaks any inference
that maps disc number to episode number.

That disc was written off after retrying: side 2 yielded S03E04-E06, verified and
staged, while side 1 first reported `disc present, unreadable` and then stopped
registering at all across a ten-minute watch. E01-E03 remain unobtained and need
another copy of the disc. Note the asymmetry is not evidence about the disc as a whole
— half of it is fine.

## Results

| Media | Result | Evidence | Proves / does not prove |
|---|---|---|---|
| **DVD-Video** | ✅ Works | 12 discs (ESPN 30 for 30) ripped, identified, migrated and verified. Probe profile `dvd_rom`. | Solid. The MakeMKV path is exercised end to end. |
| **PS2 game disc (DVD)** | ✅ Works | 1 disc, `SLUS-20578`, probe profile `dvd_rom`. CHD round-trips to Redump's exact digests. | Solid for single-layer. Dual-layer PS2 has **not** been through the drive. |
| **PS2 game disc (CD)** | ✅ Works | 2 discs, `SLUS-20247` and `SLUS-21038`, probe profile `cd_rom`. Both verify against Redump, including byte-exact cue reconstruction. | Solid. |
| **Original Xbox** | ⚠️ Video partition only | Probe reads a 14.3 MB `VIDEO_TS` volume; application id `Session Offset : 0 VTC Sector Offset: 0`; label `SEP13011042` is a pressing date code. Sectors ≥ 7000 return 0 bytes. | The game is in an XDVDFS partition outside the addressable range. Not a software limit: redumper supports these discs, but only through Kreon firmware. |
| **PS3** | ❌ Unreadable | `not ready` ×3 while spinning up, then `disc present, unreadable`. GET CONFIGURATION returns **`unknown profile 0x0000`** — no medium type determined. `dvd+rw-mediainfo` agrees: empty current configuration. | Does **not** prove PS3 discs are unreadable in PC drives generally. Three causes remain open — see below. |
| **Blu-ray (any)** | ❓ Never tested | — | **The BD read path has never been validated on this drive.** "reads: BD" is an advertised capability from udev, not an observation. |
| **Wii** | ❌ Unreadable | `disc present, unreadable` on the first poll. GET CONFIGURATION returns **`unknown profile 0x0000`**; nothing addressable (`blockdev` and `dd` both `No medium found`). | Reasonably well supported, unlike the PS3 row — see below. Wii discs are DVD-form, and this drive's DVD classification is proven on 15 discs, so the failure to classify is a property of the format, not of an untested code path. |
| **GameCube** | ❌ Unreadable | Identical to Wii: `disc present, unreadable` on the first poll, **`unknown profile 0x0000`**, nothing addressable. | Same reasoning as Wii, and it is a second independent disc agreeing. Also shows the 8 cm form factor is detected as media — the failure is the format, not the disc size. |
| **Xbox 360** | ⚠️ Video partition only | Probe reads a 5.6 MB `VIDEO_TS`/`AUDIO_TS` volume, label `XGD2DVD_NTSC`, pressed 2006-03-06. **No application identifier at all**, so the original-Xbox marker does not fire; recognised from the label instead. | Same situation as Xbox: game outside the addressable area, needs Kreon firmware. The disc names its own generation (XGD2 here), which governs the dumping method. |
| **Xbox 360 USB drive** | ✅ Works | 250 GB whole-disk FAT32 (BPB OEM name `XBOX360`), 8 Games-on-Demand titles catalogued. Chromehounds extracted to a 3.77 GB `.iso`: all 920,284 blocks matched the SHA-1 hash tree inside the package, the executable's own title and media IDs agreed with the package header, and the image region came out **byte-identical** to an independent God2Iso conversion of the same title. | Solid for Games on Demand, and it sidesteps the disc problem entirely — the game partition is read from the console's own copy, so no Kreon drive is needed. Says nothing about discs, and can never match Redump: see [xbox360-storage.md](xbox360-storage.md). |
| **Audio CD** | ❓ Not tested | — | No backend implemented. |

### The Xbox 360 disc justified the size heuristic

Worth recording as a design outcome. Xbox 360 discs carry **no ISO 9660 application
identifier**, so the `VTC Sector Offset` marker that identifies an original Xbox disc
does not fire on them. Detection by marker alone would have missed this disc entirely.

What caught it was the deliberately weaker rule — a `VIDEO_TS` volume too small to be a
feature is reported as `DvdVideo` with **Weak** confidence and the doubt stated, rather
than as a confident film. On first contact that produced:

```
Detected:    DVD-Video
Title guess: XGD2DVD_NTSC
Confidence:  weak  (needs confirmation)
  - volume is only 2724 sectors (5.6 MB), too small for a feature —
    this may be the video partition of a console game disc
```

Wrong, but wrong in a way that was visibly untrustworthy and pointed at the real
explanation. The label was then added as a proper signal. The lesson is that the
fallback mattered more than the precise rule: a signal keyed to one console's quirk
missed the next console, and only the generic implausibility check spanned both.

### Why the Nintendo results are stronger evidence than the PS3 one

Both discs produce the identical symptom, but they are not equally well controlled.

Wii and GameCube discs are both physically DVD-form, and this drive's DVD handling is
proven: 15 discs classified correctly as `dvd_rom` or `cd_rom` and dumped to verified
images. So `0x0000` here is not an untested path failing — it is a working classifier
declining to recognise the disc, which matches Nintendo's format not being
DVD-compliant. There is no hidden partition to unlock as there is on Xbox; the disc is
opaque from the start. Two different discs give the identical result, and the GameCube
disc additionally shows the 8 cm form factor is detected as media, so neither disc size
nor mechanical detection is implicated.

The PS3 disc is BD-form, and this drive's BD handling is proven on **nothing**. Its
`0x0000` is consistent with both "PS3 discs are unreadable here" and "this drive cannot
read Blu-ray at all", and the evidence cannot separate them.

The residual doubt on these rows is the discs themselves — dirty, damaged, or unseated.
Against that: both reported media immediately rather than `no disc`, which is what the
seating failure earlier that day looked like, and two unrelated discs failing the same
way is unlikely to be coincidence. Not proof, but it points away from mechanical causes.

One observation deliberately **not** drawn on: the PS3 disc spent ~15 s in `not ready`
while the Wii disc was unreadable on the first poll. That looks like a difference in how
far each got, but the time between insertion and the first poll was not controlled in
either case, so the comparison is worthless.

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
| Xbox 360 | Kreon-firmware drive, as above. The disc's label states the generation: XGD2 is the routine case, XGD3 is harder and the method should be confirmed before trusting a dump. |
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
