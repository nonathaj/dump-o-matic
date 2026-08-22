# dump-o-matic — Project Plan

**Repo:** `github.com/nonathaj/dump-o-matic`
**Language:** Rust
**Status:** stages 1–4 working end to end for game discs; video identification not yet built (see §6)

A single tool for a deliberate, multi-stage media backup pipeline: probe a disc, rip it to
staging, identify what it actually is with a confidence score, organize it to your naming
conventions, and migrate it to permanent storage — without ever risking data loss.

---

## 1. Motivation & landscape survey

Before designing this, existing open-source tooling was surveyed to see whether an
existing project was good enough to just adopt.

| Tool | What it covers | Why it isn't sufficient |
|---|---|---|
| [Automatic Ripping Machine (ARM)](https://github.com/automatic-ripping-machine/automatic-ripping-machine) | Closest existing match. udev disc detection, auto-classify (movie/TV/audio/data), rips via MakeMKV/HandBrake/abcde, Flask web UI, SQLite job DB, Docker with device passthrough | Python; optical-video/audio focused; "insert and forget" auto-rip model rather than a staged pipeline the operator drives; no staging→migrate separation; no confidence-scored identification with a human confirmation gate; no game-disc identification (Redump/No-Intro); no CLI/TUI; no explicit never-delete-without-verified-backup guarantees |
| [redumper](https://github.com/superg/redumper) | Best-in-class bit-accurate disc dumping (CD/DVD, PS1/PS2 incl. subchannel & protection quirks), hash verification against Redump | Dumper only. No identification, organization, staging, or library workflow |
| [whipper](https://github.com/whipper-team/whipper) / abcde / cdparanoia | Accurate audio-CD ripping, MusicBrainz + AccurateRip lookup | Audio CD only |
| [Rip Rip Hooray!](https://github.com/Blobfolio/riprip) | Rust audio-CD ripper focused on track recovery, cue sheet generation | Audio CD only; a ripping backend, not a pipeline |
| MakeMKV / HandBrake | Video disc ripping and transcoding | No identification, no organization, no pipeline |
| [Filebot](https://filebot.net/) / [tinyMediaManager](https://www.tinymediamanager.org/) / [Renamr](https://github.com/Penderrin-Projects/Renamr) / Sonarr / Radarr | Post-hoc identification + renaming to media-server conventions via TMDB/TVDB | Movies/TV (and books) only; no games, no CDs, no ripping stage; not a single staged pipeline |
| [GameDB-PS2](https://github.com/niemasd/GameDB-PS2), [ps_ripper](https://github.com/garbled1/ps_ripper), [Redump](http://redump.org/) datfiles | Game disc serial/hash identification data and helpers | Data sources and one-off scripts, not an integrated workflow |

**Conclusion:** no existing project covers the full multi-stage pipeline across movies/TV,
CDs, and game discs, and nothing in this space is written in Rust. ARM is the best
architectural reference (event-driven disc detection, DB-backed job state, container-first
deployment) but targets a different workflow.

**Design consequence:** `dump-o-matic` does *not* reimplement disc I/O or metadata
databases. It shells out to proven backends (redumper, MakeMKV, CD ripping tools) and
consumes existing identification sources (Redump/No-Intro datfiles, MusicBrainz,
TMDB/TVDB). The new code is the missing layer: the staged pipeline, confidence-scored
identification with a human gate, naming/organization, verified migration, and the
data-safety guarantees.

---

## 2. Core principles

### 2.1 Data-safety invariant (non-negotiable)

> No source — physical disc or staged file — is ever deleted or overwritten until its
> replacement exists at the destination with a **verified checksum match on a re-read of
> the written data**, after the write has been confirmed durable (fsync).

Concretely:

- "Move" is always implemented as *copy → fsync → re-read → hash-compare → then* delete
  the source. Never `rename(2)` across filesystems assumed-safe, never trusting a copy
  utility's exit code alone.
- Any destructive operation lacking that proof is a **logged no-op**, not a best-effort
  attempt. The tool fails loud and leaves both copies in place.
- Destination collisions never silently overwrite. If a target path exists, the job halts
  and asks — even if the existing file appears identical.
- All checksums recorded in the job DB, so every file's provenance is auditable after the fact.
- A `--dry-run` mode on every stage that performs all checks and prints every filesystem
  action it *would* take.

### 2.2 Disk-space conservatism

- **Pre-flight free-space check** before any stage begins; a stage that can't be
  guaranteed to fit refuses to start rather than producing a partial write.
- Stream rather than buffer wherever the backend allows.
- Intermediate artifacts (e.g. unwanted MKV titles, raw scratch data) are reclaimed as
  soon as the containing stage is verified — but only under the §2.1 invariant.
- Configurable headroom threshold on the staging volume; jobs queue rather than crowd the disk.

### 2.3 Single drive now, multiple later

Drives and storage destinations are modeled as **lists** from day one, even with one
entry. Adding a second optical drive or a second staging/permanent volume later is a
config change, not a rearchitecture.

### 2.4 Network destinations are first-class (and treated as hostile)

The permanent destination is expected to be a **network share** (SMB/CIFS on a NAS),
not local disk. This is a load-bearing assumption, not an afterthought, and it makes §2.1
materially harder:

- **The destination can vanish mid-write.** Migration must be interruption-safe: partial
  writes are written to a temp name and only renamed into place after verification, so an
  interrupted migration never leaves a truncated file at a real path that a later run
  mistakes for complete.
- **`fsync` over SMB/CIFS is weaker than local.** A successful `fsync` does not reliably
  prove server-side durability. The verification re-read must therefore be a **genuine
  re-read after cache invalidation** (reopen the file, ideally `O_DIRECT`-ish or after a
  remount/flush), not a read served from local page cache — otherwise the tool verifies
  its own cached copy and proves nothing. This is the single easiest way to build a
  data-loss bug here, and needs explicit tests.
- **Free-space reporting over SMB can be wrong or absent**; the pre-flight check must
  handle "unknown free space" as a distinct case (warn and require confirmation) rather
  than treating it as zero or infinite.
- **Access model:** the tool consumes the destination as a **filesystem path**, and the
  share is mounted by the OS (`cifs`/`mount.smb3`, or an autofs/systemd mount unit). The
  tool does not implement an SMB client — Rust's native SMB story is immature, and an
  OS-level mount gives correct credential handling, kernel caching, and reconnect
  behavior for free. Mount credentials therefore live in the OS mount config, not in
  `dump-o-matic`'s config (which keeps SMB passwords out of this project's secret
  surface entirely — see §2.6).
- **Destination-unavailable is a normal state, not an error.** If the share is down,
  staged content simply waits; jobs remain queued and resumable, and nothing in staging is
  ever reclaimed while its destination copy is unverified.
- Because the network hop is slow, migration is the stage most likely to be interrupted —
  so it is also the stage with the strictest resume semantics (§3, Stage 4).

### 2.5 One core, many front-ends

All logic lives in library crates. The CLI, TUI, and Web UI are thin clients over the same
core API. No capability is exclusive to one interface.

### 2.6 Secrets

**No API key is ever bundled, vendored, or committed.** All external service credentials
(TMDB, TVDB, IGDB, and any future service) are supplied by the user via config file or
environment variable, with environment taking precedence. The repo ships config
*examples* with placeholder values only. Missing credentials degrade gracefully: the
affected identification source is skipped and reported as unavailable, rather than the
tool failing outright. Secrets are redacted from all logs and from any diagnostic bundle.

---

## 3. Pipeline stages

### Stage 1 — Pre-rip (fast probe)

Answer "what's in the drive?" in seconds, with zero ripping, so the operator can decide
before committing time and disk space.

- **All discs:** device/media type, capacity, volume label, session/track layout.
- **Game discs:** volume label plus primary executable name (e.g. `SLUS_XXX.XX` on PS2)
  for an instant best-guess serial before any hashing.
- **Audio CDs:** TOC → MusicBrainz disc ID lookup (fast, and usually decisive).
- **Video discs:** disc structure/title layout via a MakeMKV info scan (`-r` robot mode)
  or `libdvdread`.

Output is a structured probe report (available identically in CLI, TUI, and Web UI).

### Stage 2 — Stage (rip to local staging)

Get bits off the disc safely. Deliberately *dumb* about naming — no organization happens here.

- Writes into a simple, predictable layout under the configured staging root:
  `<staging>/<job-id>/raw/…` plus a `job.json` manifest.
- **Game discs:** `redumper` (handles PS1/PS2 subchannel and protection quirks correctly,
  and produces the hashes needed for Redump verification).
- **Audio CDs:** accurate-rip-style reader — shell out to `cdparanoia`, or integrate
  a Rust ripper.
- **Video discs:** MakeMKV.
- Backend stdout/stderr is parsed into structured progress events; raw logs are retained
  per-job for troubleshooting.

### Stage 3 — Post-rip analysis (identify, score, organize)

Automated identification against the appropriate source, **every result carrying an
explicit confidence score and the evidence behind it**.

| Media | Primary method | Fallback (lower confidence) |
|---|---|---|
| Game discs | Exact hash match against Redump / No-Intro datfiles (**user-supplied on disk**, path in config; refreshed only via an explicit `dump-o-matic datfiles update` — offline and deterministic by default, never auto-fetched behind your back) | Volume label + executable serial fuzzy match |
| Audio CDs | MusicBrainz disc ID / AccurateRip exact match | Track-count + duration fuzzy match |
| Movies/TV | Title/duration/chapter fingerprint against TMDB/TVDB | Disc volume label heuristics |

- **Auto-accept policy (decided): exact matches only.** An exact Redump/No-Intro hash
  match or an exact MusicBrainz disc-ID match may proceed unattended — these are
  effectively certain, and the evidence is a cryptographic hash rather than a similarity
  score. **Everything fuzzy always stops and asks**, with no threshold that can be raised
  to make it not stop. This is deliberate: a tunable confidence number on a fuzzy title
  match invites silently mis-filing content, and fuzzy matching is exactly where video/TV
  identification lives. There is no "auto-accept anyway" escape hatch for fuzzy results.
- Confidence is still recorded and displayed for every result (including exact ones), so
  the audit trail shows *why* something was accepted.
- The tool states not just the match but whether it *believes* the match is correct, with
  the reasoning shown (which datfile, which hash, which fields matched).
- Confirmed items are renamed and restructured **within staging** using the naming-template
  engine, so the staged tree already mirrors its final form — making Stage 4 a pure move.

### Stage 4 — Migrate to permanent storage

- Verified copy into permanent storage following the final naming/folder convention,
  then source removal per §2.1.
- Only ever operates on items that passed Stage 3 with confirmation.
- Idempotent and resumable: interrupted migrations resume from the manifest, re-verifying
  rather than re-copying already-verified files.

### Job state

Every job's stage, artifacts, checksums, identification results, and confidence scores
live in a local SQLite DB (`rusqlite` or `sqlx`). Each stage is independently resumable
and re-runnable; killing the process mid-rip and restarting loses no tracking.

---

## 3a. Naming conventions (observed from the existing library)

These were derived by inspecting the current libraries on this machine, not chosen from
defaults. The template engine must reproduce them exactly.

| Media | Convention | Source of truth |
|---|---|---|
| Movies | `Title (Year).mkv`, flat at library root | `Animal Farm (1954).mkv` |
| TV | `Show (Year)/Season NN/Show (Year) SNNEMM - Episode Title.mkv` | `Wonder Woman (1975)/Season 01/…` |
| Music | `Artist - Year - Album/` | `~/Music/AC_DC - 1980 - Back in Black` |
| Game discs | **Redump-style filename** inside an **emulator-standard platform folder** | see below |

### Game discs — hybrid convention (decided)

Filenames follow the Redump archival convention (region + disc + serial), so a rip can be
re-verified against the datfile forever. Folder structure follows the **ES-DE / EmuDeck
platform directory names** already in use under `~/Emulation/roms/`, so rips land where
emulators already scan:

```
<permanent-root>/ps2/Star Ocean - Till the End of Time (USA) (Disc 1) [SLUS-20488].iso
<permanent-root>/psx/…
<permanent-root>/gc/…
```

This requires a maintained **platform-slug mapping table** in `dumo-core`: Redump/No-Intro
platform names (`Sony PlayStation 2`) → ES-DE slugs (`ps2`). The observed platform set
already includes `psx, ps2, ps3, psp, gc, wii, dreamcast, saturn, segacd, 3do`, and the
full `roms/` tree has ~178 entries to map from.

Note the existing `roms/` files use a looser convention (`Star Ocean Til the End of Time
D1.iso`), so adopting Redump names is a **change** from current practice. Existing files
are left alone unless explicitly re-processed; the tool never rewrites the existing
library as a side effect.

## 3b. Ingesting an existing backlog

Most libraries that motivate a tool like this already contain content stuck between
stages: discs ripped to raw backend output that was never identified, files that were
renamed inconsistently, and duplicates created by two passes over the same disc. So
**Stage 3 must accept already-staged files as input**, not only fresh output from
Stage 2.

Two capabilities follow:

- **Ingest**: point identification at existing files and let it propose names, rather
  than requiring everything to be re-ripped.
- **Audit**: scan a library for duplicates and conflicts — two files claiming the same
  episode, or content sitting in a staging-shaped layout on permanent storage.

The hardest case is a directory of anonymous per-title rips (`Show Disc 1_t00.mkv` and
similar), where the filename carries no episode information at all. Matching those means
runtime, chapter layout, and disc ordering against an episode database, with low
confidence by design. Per §6 this is explicitly a **fuzzy, always-confirm** path, and an
assisted-manual ordering interface may be the honest answer rather than full automation.

## 4. Architecture

```
crates/
  dumo-core/       # domain model, pipeline state machine, job DB, naming-template engine,
                   # verified-copy/checksum primitives (safety-critical)
  dumo-drives/     # device detection & probing (udev on Linux), container-aware
  dumo-backends/   # subprocess adapters: redumper, makemkv, cd ripper
  dumo-identify/   # Redump/No-Intro datfile matcher, MusicBrainz, TMDB/TVDB clients,
                   # confidence scoring
  dumo-cli/        # clap-based CLI over dumo-core
  dumo-tui/        # ratatui front-end
  dumo-web/        # axum server + minimal frontend (htmx or small SPA)
```

### Deployment modes

- **Native:** detects drives via `udev` events on Linux; runs as a CLI invocation, an
  interactive TUI, or a local web server.
- **Container:** the image runs `dumo-web`; staging and permanent storage are mounted
  volumes; optical devices require **explicit passthrough** (`--device /dev/sr0`).
  Documented plainly — no bind-mount magic that could mask a misconfiguration. CLI and TUI
  remain usable via `docker exec` for scripted/headless setups.

### Configuration

A single config file (TOML) plus environment overrides, covering: drive list, staging root,
permanent storage roots, per-media naming templates, free-space headroom, datfile paths,
and API credentials (§2.6). `dump-o-matic config check` validates and prints the effective
resolved config with secrets redacted.

**No storage path is hardcoded or defaulted to a guess.** Staging and permanent roots are
required config; the tool refuses to run rather than inventing a location. `config check`
additionally verifies that each configured root exists, is writable, and — for network
destinations — is currently mounted and reporting sane free space.

---

## 5. MVP scope

**Game discs, driven by the Web UI**, with the full four-stage pipeline working end to end:

1. Stage 1 probe: media type, volume label, executable serial best-guess.
2. Stage 2 rip via the `redumper` wrapper, with verified hashes captured into the job DB.
3. Stage 3 identification via Redump/No-Intro datfile hash matching, with confidence
   scoring and a confirm/edit gate; rename in staging via the template engine
   (e.g. `<Platform>/<Title> (<Region>) [<Serial>]/…`).
4. Stage 4 verified migration to permanent storage.

`dumo-web` (axum) is the primary front-end: drive state, probe results, live rip progress,
identification confidence with confirm/correct UI, migrate trigger.

`dumo-cli` ships alongside from day one — cheap once the core exists, and essential for
automation and integration testing.

Game discs are the most novel part of this tool (least covered by existing software) and
have the cleanest identification story (exact hash → near-certain match), which makes them
a good vehicle for proving the confidence-scoring and safety machinery. Once the loop is
reliable, CDs and then movies/TV are added as new backend + identify adapters, with no
structural change to the pipeline or UI.

---

## 6. Roadmap

| Phase | Deliverable |
|---|---|
| ✅ 0 | Repo scaffold, Cargo workspace, CI (fmt / clippy / test), license, config format, naming-template engine + tests |
| ✅ 1 | `dumo-drives` detection + Stage 1 fast probe for game discs; `dumo-cli` skeleton |
| ✅ 1.5 | **CIFS verification spike** — prove the post-write re-read genuinely round-trips to the server and is not served from local page cache (§2.4). Small, but gates the safety of everything after it |
| ✅ 2 | Stage 2 rip via redumper wrapper; job DB; **verified-copy/checksum module** (safety-critical — heaviest test coverage in the project, including fault injection and mid-write share disconnection) |
| ✅ 3 | Stage 3: Redump/No-Intro identification, confidence scoring, staging rename |
| ✅ 4 | Stage 4: verified migration to permanent storage |
| 5 | `dumo-web` UI over the above |
| 6 | Docker image + device-passthrough documentation |
| 7 | Audio CD support (MusicBrainz + accurate-rip backend) |
| 8 | Movies/TV support (MakeMKV + TMDB/TVDB) |
| 8.5 | **Backlog ingest** — Stage 3 accepts already-staged files (§3b); tackle the Friends/Mad Men unidentified dumps and a library audit/duplicate-detection mode |
| 9 | `dumo-tui` (ratatui) front-end |
| 10 | Multi-drive and multi-destination support |

---

## 7. License

**MIT.** (Decided.)

- Permissive licensing is the right fit for this architecture: the tool *shells out to*
  backends including MakeMKV (proprietary, non-free) rather than linking against them.
  A copyleft license would raise avoidable questions about distributing a tool whose
  expected workflow depends on proprietary helpers.
- Maximum compatibility for downstream reuse — `dumo-core` and `dumo-identify` are the
  parts most likely to be picked up by other projects, and MIT imposes the fewest barriers.
- Well within Rust ecosystem norms (the ecosystem standard is MIT OR Apache-2.0; MIT alone
  is a strict subset of that permission set, so nothing downstream breaks).

Implementation notes for Phase 0:

- `LICENSE` file at repo root with the MIT text, copyright `nonathaj`.
- `license = "MIT"` in every crate's `Cargo.toml` manifest.
- No Apache-2.0 patent grant is included; this is a deliberate, accepted trade-off.

**Note on backends:** `redumper`, MakeMKV, and HandBrake are *not* redistributed by this
project. They are external dependencies the user installs. Documentation must be explicit
about this, and the container image must not bundle non-redistributable binaries — it
should document how the user supplies them.

---

## 8. Resolved decisions

| Item | Decision |
|---|---|
| License | MIT (§7) |
| Naming conventions | Derived from the existing library (§3a) — not defaults |
| Game naming | Redump-style filenames inside ES-DE/EmuDeck platform folders (§3a) |
| Datfiles | User-supplied on disk; explicit `datfiles update` command; never auto-fetched |
| Auto-accept | Exact hash / disc-ID matches only; fuzzy always confirms, with no override (§3) |
| Storage paths | **Fully configurable, no hardcoded defaults.** Expected usage: staging on local disk, permanent on an SMB-mounted NAS. The tool ships no built-in path assumptions |
| Permanent destination | Network share, mounted by the OS, consumed as a path (§2.4) |
| API keys | Config/env only, never bundled (§2.6) |

## 9. Still open

- **Platform-slug mapping table** (Redump platform names → ES-DE slugs) needs to be built
  and reviewed; ~178 platform dirs exist under `roms/` but only the disc-based ones matter
  for ripping. Cartridge platforms are out of scope until manual dump import lands.
- **Episode-identification strategy for anonymous `_t00.mkv` dumps** (§3b) — matching on
  runtime + chapter layout + disc ordering against TVDB. Needs prototyping against the
  real Friends/Mad Men backlog to see whether it is reliable enough to be worth shipping,
  or whether an assisted-manual ordering UI is the honest answer.
- **Verification-read strategy over CIFS** (§2.4) — the post-write re-read now evicts the
  page cache with `posix_fadvise(DONTNEED)` before hashing, which makes it a genuine
  read rather than a re-hash of local buffers. `POSIX_FADV_DONTNEED` is advisory, so this
  is a strong check rather than absolute proof of server-side durability.
- Whether audio CD ripping uses `cdparanoia` (already installed) or an integrated Rust
  ripper; AccurateRip integration is a separate question from the ripper choice.
- Retention policy for staging after a successful migrate: keep-until-space-needed vs.
  reclaim-immediately-on-verify. Both are safe under §2.1; it is purely a space/convenience
  trade-off and should be configurable.
