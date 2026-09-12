# Xbox 360 storage devices

What a USB drive written by an Xbox 360 actually contains, how `dump-o-matic` reads it, and
what the resulting files are and are not. Everything here was measured on a real drive
(Seagate FreeAgent Go, 250 GB, eight Games-on-Demand titles) and cross-checked against
known-good conversions; where a value was derived rather than observed, it says so.

## The layout

The console formats the whole device as FAT32 — no partition table — and writes `XBOX360`
as the BPB OEM name. On the volume:

```
NAME.TXT                                    UTF-16BE, e.g. "USB Storage Device"
Content/
  0000000000000000/                         profile id; all-zero means not profile-bound
    545407E0/                               title id
      00004000/                             content type
        9906C8D03D0BD52CBFC39FCD1ABA0DD5        45,056-byte package header
        9906C8D03D0BD52CBFC39FCD1ABA0DD5.data/
          Data0000 … Data0027                   the image, in 0xA290000 chunks
```

Content types seen: `00004000` Games on Demand, `00040000` a dashboard cache file. An
install from disc is `00007000` and uses the same container, so it is handled identically.

There is **no FATX layer**. Older "configured" Xbox 360 drives wrap a FATX volume inside
`Xbox360/Data####` files; this drive does not, and the content sits directly in FAT32.

## The package container (GoD / SVOD)

Each data file is laid out as blocks of 0x1000 bytes:

```
[ master hash block ] [ hash block ] [ 204 data blocks ] [ hash block ] [ 204 data blocks ] …
```

with 203 groups in a full file, which is exactly the observed file size:
`0x1000 + 203 × (0x1000 + 204 × 0x1000) = 0xA290000`.

A hash block holds the SHA-1 of each of the 204 data blocks after it; the master block holds
the SHA-1 of each group hash block. **Every block of the image is therefore covered by a
hash the console wrote**, which is why extraction verifies rather than trusts. On the
measured drive every block of every title matched, including across data-file boundaries.

The header's volume descriptor (at 0x379) supplies two fields this tool needs, and their
encoding is worth stating because published descriptions of it disagree:

| Field | Offset in descriptor | Encoding | Meaning |
|---|---|---|---|
| data block count | +0x19 | 24-bit **big**-endian | image length in blocks |
| data block offset | +0x1C | 24-bit **little**-endian | origin of the filesystem's sector numbers |

The mixed endianness is not a typo: read either field the other way round and it becomes an
absurd value (billions of blocks). Both readings were confirmed against all eight packages.

Note that the count is consistently **one lower** than the files' own geometry, because the
final block is padding the header does not count. The image includes it, as God2Iso's output
confirms.

## Addressing, and why a plain concatenation is not an ISO

The image holds only the game partition, and its XDVDFS sector numbers are **not relative to
the start of that image**. They are relative to an origin some way before it, different for
every title:

| Title | Title ID | Origin (blocks) | Image blocks |
|---|---|---|---|
| Ninety-Nine Nights | 4D5307DB | 56,799 | 1,409,224 |
| Chromehounds | 534507D4 | 185,690 | 920,284 |
| CoD: World at War | 4156081C | 25,612 | 1,660,559 |
| Guitar Hero II | 415607E7 | 271,755 | 620,213 |
| Frontlines: Fuel of War | 545107D8 | 67,463 | 1,641,718 |
| Rainbow Six® Vegas | 555307D6 | 23,467 | 1,720,749 |
| Command & Conquer 3 | 4541080E | 326,023 | 1,407,296 |
| Prey | 545407E0 | 198,639 | 1,113,037 |

In every case the origin is the declared `data block offset` **minus one block**. Rather than
hardcode that, `xdvdfs::resolve_base` treats the field as a hint and proves each candidate by
reading the root directory it implies and requiring it to parse — a wrong base does not fail
loudly, it produces an image that looks structurally fine and is entirely corrupt.

So concatenating the data blocks yields a file whose volume descriptor is at offset 0 rather
than the 0x10000 a reader expects, and whose every sector number points nowhere. Converting
properly takes two changes:

1. Put 32 sectors (0x10000) in front, so the volume descriptor lands where readers look.
2. Subtract `origin × 2 − 32` from every sector number, in the volume descriptor and in every
   directory table.

No file payload moves, and nothing else is touched.

## Agreement with God2Iso

The conversion above is what the abandoned God2Iso tool produced. That was verified, not
assumed, against an existing God2Iso conversion of Call of Duty: World at War:

- **Size.** God2Iso's file is 6,801,715,200 bytes, exactly `1,660,559 × 0x1000 + 0x10000`.
  The same relationship held for all four already-converted titles in the library.
- **Payload.** Our reconstructed stream is byte-identical to its output at five points
  spanning all 6.8 GB, including the final block.
- **Rewriting.** Applying the shift to our root directory table produced a table with an
  **identical SHA-256** to God2Iso's, and its individual entries match
  (`mp_seelow.ff` 1,848,488 → 1,797,296, `$SystemUpdate` 3,368,795 → 3,317,603).

God2Iso also fills the otherwise-unused 32 reserved sectors with a small `XSF` descriptor
recording the image length, its own name at 0x7A69, and an ISO 9660 primary volume descriptor
at sector 16 so generic tools recognise the file as an image. `dump-o-matic` writes the same
shape with its own name in place of God2Iso's, so the files are the same kind of thing but not
byte-identical in that region. The manifest therefore records `image_sha256` — the hash of the
image data alone, excluding the reserved region — which *is* comparable between the two tools.

## What identification can and cannot claim

Redump's Xbox 360 hashes describe a whole disc: video partition, game partition and security
sectors. A package holds only the game partition, so **no extraction from a USB drive can ever
match a Redump entry**, however faithful it is. This is a permanent property of the source,
not a gap to be closed.

What is available instead:

- the package's own title ID, media ID and name;
- the same title and media IDs read independently out of the game's `default.xex` (all eight
  measured titles agreed);
- every block checked against the package's hash tree during extraction.

Two independent signals agreeing is recorded as `strong` confidence — enough to name the file,
and deliberately not enough to file unattended. `identify --apply` therefore refuses it until
`--accept-inferred` is given, exactly as for an inferred video identification. A package is a
verified *copy*; it is not a verified *dump*, and the manifest keeps the two distinct.

One further caveat: a `CON `-signed package was licensed to the console that wrote it. The
image extracted from it is complete, but the licence does not travel.

## Reading a device without mounting it

Devices are opened `O_RDONLY` and the FAT32 volume is parsed in-process. Mounting read-write
would let the kernel update the volume's dirty bit — a write to media being preserved — and a
read-only mount needs privileges the tool may not have. Parsing it directly means nothing in
the code path can write to the device at all.

Reading a block device directly does require permission: membership of the `disk` group (log
out and back in afterwards) or running as root. Failing that, mount the device read-only and
pass the mount directory instead — `catalog` and `pull` accept a directory anywhere a device
node is accepted.

Whole disks are what `devices` lists. A partitioned device is not a FAT32 volume at its start,
so pass the partition (`/dev/sdd1`) explicitly in that case.
