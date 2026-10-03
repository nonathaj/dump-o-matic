//! Audio CD identification and filing.
//!
//! The disc ID read off the TOC is looked up in MusicBrainz, and the rip's tracks are
//! checked against AccurateRip. When exactly one release carries the disc ID and its
//! track list agrees with the TOC, the identification is [`Confidence::Exact`]: it is a
//! hash of the disc itself matching, the same class of evidence as a Redump match.
//!
//! Filing encodes each track with LAME `-V 2` and tags it the way MusicBrainz Picard
//! does, into `ready/music/<Album Artist> - <Year> - <Album>/<D>-<NN> - <Artist> -
//! <Title>.mp3`. The lossless `.bin`/`.cue` dump stays in the job directory as the
//! archival record, as a game's disc image does.

use anyhow::{bail, Context, Result};
use dumo_backends::{audio, id3};
use dumo_core::config::Config;
use dumo_core::job::Job;
use dumo_core::{hash, Confidence, ReadyFile};
use dumo_identify::accuraterip::{self, TrackResult};
use dumo_identify::musicbrainz::{self, Medium, Release, Track};
use std::path::{Path, PathBuf};

/// The ready-tree category music is routed by.
pub const CATEGORY: &str = "music";

/// Picard's Windows-compatible filename rule: every character Windows forbids becomes
/// `_`, which is why `AC/DC` is filed under `AC_DC`.
pub fn picard_safe(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// A folder name: as [`picard_safe`], plus Windows refuses a trailing dot on a folder,
/// which Picard turns into `_` (`R.E.V.O.` is filed as `R.E.V.O_`). A file name needs
/// no such rule — its extension follows — so `Luckie St..mp3` keeps both dots.
pub fn picard_safe_dir(s: &str) -> String {
    let safe = picard_safe(s.trim());
    match safe.strip_suffix('.') {
        Some(stem) => format!("{stem}_"),
        None => safe,
    }
}

/// `Album Artist - Year - Album`, or `Album Artist - Album` for an undated release.
pub fn album_dir(release: &Release) -> String {
    let name = match release.year() {
        Some(y) => format!("{} - {} - {}", release.artist.name, y, release.title),
        None => format!("{} - {}", release.artist.name, release.title),
    };
    picard_safe_dir(&name)
}

pub fn track_file(medium: &Medium, track: &Track) -> String {
    format!(
        "{}-{:02} - {}.mp3",
        medium.position,
        track.position,
        picard_safe(&format!("{} - {}", track.artist.name, track.title))
    )
}

/// The ID3v2.4 frames Picard writes, in Picard's order.
pub fn frames(release: &Release, medium: &Medium, track: &Track, encoder: &str) -> Vec<id3::Frame> {
    use id3::Frame::*;
    let mut f = vec![
        Text("TIT2", track.title.clone()),
        Text("TPE1", track.artist.name.clone()),
        Numeric("TRCK", format!("{}/{}", track.position, medium.tracks.len())),
        Text("TALB", release.title.clone()),
        Numeric("TPOS", format!("{}/{}", medium.position, release.media.len())),
    ];
    if let Some(d) = &release.date {
        f.push(Text("TDRC", d.clone()));
    }
    if let Some(g) = &release.genre {
        f.push(Text("TCON", g.clone()));
    }
    f.push(Text("TPE2", release.artist.name.clone()));
    f.push(Text("TSO2", release.artist.sort.clone()));
    f.push(Text("TSOP", track.artist.sort.clone()));
    if let Some(ms) = track.length_ms {
        f.push(Numeric("TLEN", ms.to_string()));
    }
    if let Some(l) = &release.label {
        f.push(Text("TPUB", l.clone()));
    }
    if let Some(m) = &medium.format {
        f.push(Text("TMED", m.clone()));
    }
    if let Some(d) = &release.original_date {
        f.push(Text("TDOR", d.clone()));
    }
    if let Some(s) = &release.script {
        f.push(UserText("SCRIPT".into(), s.clone()));
    }
    if let Some(i) = &track.isrc {
        f.push(Text("TSRC", i.clone()));
    }
    f.push(UserText("ARTISTS".into(), track.artist.artists.join("; ")));
    if let Some(y) = release.original_date.as_deref().and_then(|d| d.get(..4)) {
        f.push(UserText("originalyear".into(), y.to_string()));
    }
    if let Some(c) = &release.catalog_number {
        f.push(UserText("CATALOGNUMBER".into(), c.clone()));
    }
    if let Some(b) = &release.barcode {
        f.push(UserText("BARCODE".into(), b.clone()));
    }
    if let Some(t) = &release.release_type {
        f.push(UserText("MusicBrainz Album Type".into(), t.clone()));
    }
    if let Some(s) = &release.status {
        f.push(UserText("MusicBrainz Album Status".into(), s.clone()));
    }
    if let Some(c) = &release.country {
        f.push(UserText("MusicBrainz Album Release Country".into(), c.clone()));
    }
    if release.is_compilation() {
        f.push(Numeric("TCMP", "1".into()));
    }
    f.push(Text("TSSE", encoder.to_string()));
    f.push(UserText("MusicBrainz Album Id".into(), release.id.clone()));
    f.push(UniqueId("http://musicbrainz.org".into(), track.recording_id.clone()));
    f.push(UserText(
        "MusicBrainz Artist Id".into(),
        track.artist.artist_ids.join("; "),
    ));
    f.push(UserText(
        "MusicBrainz Album Artist Id".into(),
        release.artist.artist_ids.join("; "),
    ));
    f.push(UserText(
        "MusicBrainz Release Group Id".into(),
        release.release_group_id.clone(),
    ));
    f.push(UserText("MusicBrainz Release Track Id".into(), track.id.clone()));
    f
}

/// redumper's per-track images, in track order. They are named `<name> (Track NN).bin`,
/// or plain `<name>.bin` on a single-track disc.
fn track_bins(job: &Job, job_dir: &Path) -> Vec<PathBuf> {
    let mut bins: Vec<(u32, PathBuf)> = job
        .artifacts
        .iter()
        .filter(|a| a.relative_path.ends_with(".bin"))
        .map(|a| {
            let n = a
                .relative_path
                .rsplit_once("(Track ")
                .and_then(|(_, rest)| rest.split(')').next())
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0);
            (n, job_dir.join(&a.relative_path))
        })
        .collect();
    bins.sort();
    bins.into_iter().map(|(_, p)| p).collect()
}

pub fn identify_audio(
    job: &mut Job,
    job_dir: &Path,
    cfg: &Config,
    apply: bool,
    release_choice: Option<&str>,
) -> Result<()> {
    let Some(toc) = job.probe.as_ref().and_then(|p| p.toc.clone()) else {
        bail!("job has no table of contents recorded; it cannot be identified");
    };
    let Some(disc_id) = toc.musicbrainz_discid.clone() else {
        bail!("job has no MusicBrainz disc ID recorded");
    };
    if job.stage != dumo_core::JobStage::Ripped {
        println!("  stage is {}, not ripped — nothing to identify", job.stage);
        return Ok(());
    }
    let audio_tracks: Vec<_> = toc.tracks.iter().filter(|t| !t.is_data).collect();
    if audio_tracks.len() != toc.tracks.len() {
        bail!("mixed audio/data discs are not supported yet");
    }

    // --- Which release is this? ---
    println!("  Looking up disc ID {disc_id} in MusicBrainz ...");
    let releases = musicbrainz::lookup_disc_id(&disc_id).context("querying MusicBrainz")?;
    if releases.is_empty() {
        println!("  Not in MusicBrainz. Nothing was filed.");
        println!("  The disc can be added at https://musicbrainz.org/cdtoc/attach?id={disc_id}");
        return Ok(());
    }
    let release = match (release_choice, releases.len()) {
        (Some(id), _) => match releases.iter().find(|r| r.id == id) {
            Some(r) => r,
            None => bail!("release {id} does not carry this disc ID"),
        },
        (None, 1) => &releases[0],
        (None, n) => {
            println!("  {n} releases share this disc ID; choose one with --release <id>:");
            for r in &releases {
                println!(
                    "    {}  {} - {} ({} {}, {})",
                    r.id,
                    r.artist.name,
                    r.title,
                    r.date.as_deref().unwrap_or("?"),
                    r.country.as_deref().unwrap_or("?"),
                    r.catalog_number.as_deref().unwrap_or("no catalog number")
                );
            }
            println!("  Nothing was filed.");
            return Ok(());
        }
    };
    let Some(medium) = release.medium_for(&disc_id) else {
        bail!("MusicBrainz returned release {} without the medium for this disc", release.id);
    };
    if medium.tracks.len() != audio_tracks.len() {
        bail!(
            "MusicBrainz lists {} tracks on this medium, the disc has {}; refusing to guess \
             which is which",
            medium.tracks.len(),
            audio_tracks.len()
        );
    }

    println!();
    println!("  {} - {}", release.artist.name, release.title);
    println!(
        "  {} {} · {} · {}{}",
        release.date.as_deref().unwrap_or("undated"),
        release.country.as_deref().unwrap_or(""),
        medium.format.as_deref().unwrap_or("CD"),
        release.label.as_deref().unwrap_or("no label"),
        release
            .catalog_number
            .as_deref()
            .map(|c| format!(" {c}"))
            .unwrap_or_default()
    );
    println!("  https://musicbrainz.org/release/{}", release.id);
    println!(
        "  matched on: MusicBrainz disc ID (a hash of this disc's table of contents){}",
        if releases.len() == 1 {
            ", the only release carrying it"
        } else {
            ", release chosen with --release"
        }
    );

    // --- Is the rip right? ---
    let bins = track_bins(job, job_dir);
    let program = audio::Program::open(&bins).context("opening track images")?;
    let expected = u64::from(toc.leadout_lba) * audio::SECTOR_BYTES;
    if program.len() != expected {
        bail!(
            "the dump holds {} bytes of audio but the TOC says {}; refusing to cut tracks \
             from it",
            program.len(),
            expected
        );
    }
    let starts: Vec<u32> = audio_tracks.iter().map(|t| t.start_lba).collect();
    let ranges = audio::track_ranges(&starts, toc.leadout_lba);

    let cddb = toc
        .freedb_discid
        .as_deref()
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .unwrap_or(0);
    let ids = accuraterip::DiscIds::compute(&starts, toc.leadout_lba, cddb);
    print!("  Checking AccurateRip ... ");
    let entries = match accuraterip::lookup(&ids) {
        Ok(e) => e,
        Err(e) => {
            println!("unavailable ({e})");
            Vec::new()
        }
    };
    if entries.is_empty() {
        println!("disc not in the database; the rip rests on redumper's own checks");
    } else {
        println!("{} pressing(s) on record", entries.len());
    }

    let mut mismatches = 0;
    println!();
    for (i, (track, (start, len))) in medium.tracks.iter().zip(&ranges).enumerate() {
        let pcm = program.read_range(*start, *len)?;
        let (v1, v2) = accuraterip::checksums(&pcm, i == 0, i + 1 == ranges.len());
        let verdict = if entries.is_empty() {
            "—".to_string()
        } else {
            match accuraterip::verify_track(&entries, i, v1, v2) {
                TrackResult::Accurate { confidence } => format!("accurate ({confidence})"),
                TrackResult::Mismatch => {
                    mismatches += 1;
                    "NOT ACCURATE".to_string()
                }
            }
        };
        println!(
            "  {:>2}. {:<44} {:>5}  {}",
            track.position,
            truncate(&format!("{} - {}", track.artist.name, track.title), 44),
            fmt_len(*len),
            verdict
        );
    }

    let dir = album_dir(release);
    println!();
    println!("  Proposed: {CATEGORY}/{dir}/");
    for t in medium.tracks.iter().take(3) {
        println!("    {}", track_file(medium, t));
    }
    if medium.tracks.len() > 3 {
        println!("    ... {} more", medium.tracks.len() - 3);
    }

    if mismatches > 0 {
        println!();
        println!(
            "  {mismatches} track(s) did not match AccurateRip. The rip is suspect (or the \
             drive read offset is wrong), so nothing was filed."
        );
        return Ok(());
    }

    let confidence = Confidence::Exact;
    println!("  Confidence: {confidence}");
    if !apply {
        println!("  Nothing was filed. Re-run with --apply to encode and file it.");
        return Ok(());
    }

    // --- File it ---
    let encoder = audio::lame_version().context("checking lame")?;
    let ready_rel = Path::new("ready").join(CATEGORY).join(&dir);
    let ready_abs = cfg.staging.root.join(&ready_rel);
    for t in &medium.tracks {
        let dest = ready_abs.join(track_file(medium, t));
        if dest.exists() {
            bail!("{} already exists; refusing to overwrite", dest.display());
        }
    }
    std::fs::create_dir_all(&ready_abs)?;
    let work = job_dir.join("encode");
    std::fs::create_dir_all(&work)?;

    println!();
    println!("  Encoding with {encoder}, -V 2 ...");
    let mut files = Vec::new();
    for (track, (start, len)) in medium.tracks.iter().zip(&ranges) {
        let pcm = program.read_range(*start, *len)?;
        let wav = work.join(format!("{:02}.wav", track.position));
        let mp3 = work.join(format!("{:02}.mp3", track.position));
        audio::write_wav(&wav, &pcm)?;
        audio::encode_mp3_v2(&wav, &mp3)?;
        let encoded = std::fs::read(&mp3)?;
        let tagged = id3::tag_mp3(
            &encoded,
            &frames(release, medium, track, &encoder),
            &id3::V1 {
                title: track.title.clone(),
                artist: track.artist.name.clone(),
                album: release.title.clone(),
                year: release.year().unwrap_or("").to_string(),
                track: track.position.min(255) as u8,
            },
        );
        let name = track_file(medium, track);
        let dest = ready_abs.join(&name);
        let sha = hash::sha256_of(&tagged);
        dumo_core::fsops::write_verified(&dest, &tagged, &sha)
            .with_context(|| format!("writing {}", dest.display()))?;
        std::fs::remove_file(&wav)?;
        std::fs::remove_file(&mp3)?;
        println!("    {name} ... filed");
        files.push(ReadyFile {
            path: ready_rel.join(&name).to_string_lossy().to_string(),
            bytes: tagged.len() as u64,
            sha256: sha,
        });
    }
    let _ = std::fs::remove_dir(&work);

    job.identification = Some(dumo_core::Identification {
        title: format!("{} - {}", release.artist.name, release.title),
        platform: "Audio CD".into(),
        platform_slug: CATEGORY.into(),
        matched_on: format!("MusicBrainz disc ID {disc_id} (release {})", release.id),
        confidence,
        source: "redumper .bin/.cue".into(),
        files,
        superseded: Vec::new(),
        identified_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    job.stage = dumo_core::JobStage::Identified;
    job.save(job_dir)?;

    println!();
    println!("  Filed {} track(s) under ready/{CATEGORY}/{dir}/", medium.tracks.len());
    println!("  job stage: identified");
    println!("  the lossless .bin/.cue dump stays in the job directory as provenance");
    Ok(())
}

fn fmt_len(bytes: u64) -> String {
    let secs = bytes / audio::SECTOR_BYTES / 75;
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dumo_identify::musicbrainz::Credit;

    fn credit(name: &str) -> Credit {
        Credit {
            name: name.into(),
            sort: name.into(),
            artists: vec![name.into()],
            artist_ids: vec!["id".into()],
        }
    }

    fn release(artist: &str, title: &str, date: &str) -> Release {
        Release {
            id: "r".into(),
            title: title.into(),
            artist: credit(artist),
            date: Some(date.into()),
            country: None,
            status: None,
            barcode: None,
            script: None,
            label: None,
            catalog_number: None,
            release_group_id: "rg".into(),
            release_type: None,
            original_date: None,
            genre: None,
            media: vec![],
        }
    }

    /// Real folder names from the library this naming was copied from.
    #[test]
    fn album_folders_match_the_existing_library() {
        assert_eq!(album_dir(&release("AC/DC", "Back in Black", "1980-07-25")), "AC_DC - 1980 - Back in Black");
        assert_eq!(
            album_dir(&release("Howard Shore", "The Lord of the Rings: The Two Towers", "2002")),
            "Howard Shore - 2002 - The Lord of the Rings_ The Two Towers"
        );
        assert_eq!(album_dir(&release("Ryan Star", "11:59", "2010")), "Ryan Star - 2010 - 11_59");
    }

    #[test]
    fn track_files_carry_disc_number_and_the_track_artist() {
        let m = Medium { position: 2, format: None, disc_ids: vec![], tracks: vec![] };
        let t = Track {
            id: "t".into(),
            recording_id: "rec".into(),
            position: 3,
            title: "Savages".into(),
            length_ms: None,
            artist: credit("David Ogden Stiers & Judy Kuhn"),
            isrc: None,
        };
        assert_eq!(track_file(&m, &t), "2-03 - David Ogden Stiers & Judy Kuhn - Savages.mp3");
    }

    #[test]
    fn picard_safe_replaces_every_windows_reserved_character() {
        assert_eq!(picard_safe(r#"a/b\c:d*e?f"g<h>i|j"#), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(picard_safe("BRE@TH//LESS"), "BRE@TH__LESS");
        assert_eq!(picard_safe("アルドノア・ゼロ"), "アルドノア・ゼロ");
    }

    /// Both from the library: a title's trailing dots survive in a file name, while a
    /// folder's final dot becomes `_`.
    #[test]
    fn trailing_dots_are_kept_in_files_and_replaced_on_folders() {
        let m = Medium { position: 1, format: None, disc_ids: vec![], tracks: vec![] };
        let mut t = Track {
            id: "t".into(),
            recording_id: "r".into(),
            position: 7,
            title: "Luckie St.".into(),
            length_ms: None,
            artist: credit("Cartel"),
            isrc: None,
        };
        assert_eq!(track_file(&m, &t), "1-07 - Cartel - Luckie St..mp3");
        t.title = "It Is Only Beginning...".into();
        assert!(track_file(&m, &t).ends_with("It Is Only Beginning....mp3"));
        assert_eq!(
            album_dir(&release("Walk off the Earth", "R.E.V.O.", "2013")),
            "Walk off the Earth - 2013 - R.E.V.O_"
        );
    }

    #[test]
    fn an_undated_release_drops_the_year_segment() {
        let mut r = release("Everything", "People Are Moving", "");
        r.date = None;
        assert_eq!(album_dir(&r), "Everything - People Are Moving");
    }

    #[test]
    fn frames_follow_picards_layout() {
        let mut r = release("Hiroyuki Sawano", "Album", "2014-09-10");
        r.media = vec![Medium { position: 1, format: Some("CD".into()), disc_ids: vec![], tracks: vec![] }];
        let t = Track {
            id: "track-id".into(),
            recording_id: "rec-id".into(),
            position: 1,
            title: "No differences".into(),
            length_ms: Some(277013),
            artist: credit("Hiroyuki Sawano"),
            isrc: Some("JPE301400782".into()),
        };
        let mut m = r.media[0].clone();
        m.tracks = vec![t.clone()];
        let f = frames(&r, &m, &t, "LAME 64bits version 3.100 (http://lame.sf.net)");
        assert_eq!(f[0], id3::Frame::Text("TIT2", "No differences".into()));
        assert!(f.contains(&id3::Frame::Numeric("TRCK", "1/1".into())));
        assert!(f.contains(&id3::Frame::Numeric("TPOS", "1/1".into())));
        assert!(f.contains(&id3::Frame::UniqueId("http://musicbrainz.org".into(), "rec-id".into())));
        assert!(f.contains(&id3::Frame::UserText("MusicBrainz Release Track Id".into(), "track-id".into())));
        assert!(!f.iter().any(|x| matches!(x, id3::Frame::Numeric("TCMP", _))));
    }
}
